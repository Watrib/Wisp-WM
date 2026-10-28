//! Direct DRM/GBM backend for a single GPU and connector.
//!
//! The backend is deliberately smaller than Anvil's `DrmCompositor`: it uses
//! one `GbmBufferedSurface` per output, composes into a GBM buffer with GLES,
//! and leaves hardware cursor planes, direct scanout and multi-GPU support for
//! later. The nested backend remains the default.

mod cursor;
mod output;
mod probe;

use std::{
    cell::RefCell,
    io::ErrorKind,
    os::unix::fs::MetadataExt,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, ensure};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use smithay::{
    backend::{
        SwapBuffersError,
        allocator::{
            Fourcc,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{DrmDevice, DrmDeviceFd, DrmError, DrmEvent, DrmNode, GbmBufferedSurface, NodeType},
        egl::{
            EGLDisplay,
            context::{ContextPriority, EGLContext},
        },
        input::{Device as InputDevice, DeviceCapability, Event, InputEvent, KeyboardKeyEvent},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{Bind, ImportDma, damage::OutputDamageTracker, gles::GlesRenderer},
        session::{
            Event as SessionEvent, Session,
            libseat::{LibSeatSession, LibSeatSessionNotifier},
        },
        udev::{UdevBackend, UdevEvent},
    },
    input::pointer::CursorImageStatus,
    output::{Mode as OutputMode, Output, PhysicalProperties},
    reexports::{
        calloop::{
            LoopHandle,
            timer::{TimeoutAction, Timer},
        },
        drm::Device as _,
        drm::control::{Device as _, connector, crtc},
        input::{Device as LibinputDevice, Led, Libinput},
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, Point, Transform},
    wayland::dmabuf::DmabufFeedbackBuilder,
};
use tracing::{debug, info, warn};

use self::cursor::SoftwareCursor;
use super::{Backend, LoopData, Options, Wisp};

pub(super) fn probe_direct(
    device: Option<&std::path::Path>,
    connector: Option<&str>,
    seat: &str,
) -> Result<()> {
    probe::run(device, connector, seat)
}

/// 8-bit scanout formats; Smithay negotiates compatible GBM modifiers.
const COLOR_FORMATS: &[Fourcc] = &[Fourcc::Abgr8888, Fourcc::Argb8888];
const FLIP_TIMEOUT: Duration = Duration::from_millis(750);
const GLOBAL_RETIRE_DELAY: Duration = Duration::from_secs(5);

type Surface = GbmBufferedSurface<GbmAllocator<DrmDeviceFd>, ()>;

struct OutputState {
    output: Output,
    global: GlobalId,
    connector: connector::Handle,
    crtc: crtc::Handle,
    mode: OutputMode,
    surface: Surface,
    damage: OutputDamageTracker,
}

pub(super) struct Direct {
    // Drop scanout and cached cursor textures before the renderer/GBM device.
    output: Option<OutputState>,
    cursor: SoftwareCursor,
    elements: Vec<super::scene::Element>,
    renderer: Rc<RefCell<GlesRenderer>>,
    gbm: GbmDevice<DrmDeviceFd>,
    drm: DrmDevice,
    libinput: Libinput,
    keyboards: Vec<LibinputDevice>,
    session: LibSeatSession,
    retired_globals: Vec<(Instant, GlobalId)>,
    seat_name: String,
    device_path: PathBuf,
    device_id: u64,
    connector_filter: Option<String>,
    driver: String,
    active: bool,
    pending_since: Option<Instant>,
    confirm_lock_on_flip: Option<smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1>,
    retry_scanout: bool,
    stall_retries: u8,
}

impl Direct {
    /// Open the selected card through libseat and build the renderer. The
    /// caller has already created the libseat session so the Wayland seat can
    /// be named correctly before `Wisp` is constructed.
    pub(super) fn new(
        state: &mut Wisp,
        mut session: LibSeatSession,
        notifier: LibSeatSessionNotifier,
        options: &Options,
        handle: &LoopHandle<'_, LoopData>,
    ) -> Result<Self> {
        ensure!(
            session.is_active(),
            "libseat session is inactive; start direct mode from an active VT"
        );
        let seat_name = session.seat();
        let device_path = probe::device_path(options.drm_device.as_deref(), &seat_name)?;
        let device_id = std::fs::metadata(&device_path)
            .with_context(|| format!("reading metadata for {}", device_path.display()))?
            .rdev();

        let fd = session
            .open(
                &device_path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .with_context(|| {
                format!(
                    "opening {} through libseat (is the seat active and the device accessible?)",
                    device_path.display()
                )
            })?;
        // LibSeatSession ignores open flags. Nonblocking reads are required
        // when draining obsolete flip events after a blocking KMS reset.
        fcntl_setfl(&fd, fcntl_getfl(&fd)? | OFlags::NONBLOCK)
            .context("making the DRM event descriptor nonblocking")?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));
        let (drm, drm_notifier) = DrmDevice::new(fd.clone(), true)
            .with_context(|| format!("initializing DRM device {}", device_path.display()))?;
        let driver = drm
            .get_driver()
            .map(|driver| driver.name().to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".into());
        let gbm = GbmDevice::new(fd)
            .with_context(|| format!("creating GBM device for {}", device_path.display()))?;
        // SAFETY: the GBM device is retained until after the renderer is dropped.
        let egl_display = unsafe { EGLDisplay::new(gbm.clone()) }
            .context("creating EGL display from GBM device")?;
        let egl_context = EGLContext::new_with_priority(&egl_display, ContextPriority::High)
            .context("creating EGL context")?;
        // SAFETY: this is a fresh GLES context used only by this renderer/thread.
        let renderer =
            unsafe { GlesRenderer::new(egl_context) }.context("creating GLES renderer")?;
        let formats = renderer.dmabuf_formats();
        let renderer = Rc::new(RefCell::new(renderer));
        if formats.iter().next().is_some() {
            // Only advertise texture-import formats; we do not support direct
            // scanout and must not advertise a scanout preference tranche.
            let primary_node = DrmNode::from_dev_id(device_id)
                .context("identifying the direct DRM device for dma-buf feedback")?;
            let main_node = primary_node
                .node_with_type(NodeType::Render)
                .transpose()
                .context("identifying the render node for dma-buf feedback")?
                .unwrap_or(primary_node);
            let feedback = DmabufFeedbackBuilder::new(main_node.dev_id(), formats.iter().copied())
                .build()
                .context("building linux-dmabuf feedback")?;
            state
                .dmabuf_state
                .create_global_with_default_feedback::<Wisp>(&state.display_handle, &feedback);
            state.dmabuf_renderer = Some(Rc::downgrade(&renderer));
            state.dmabuf_session_active = true;
        } else {
            warn!("GLES renderer has no dma-buf texture formats; wl_shm remains available");
        }

        let mut libinput = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(
            session.clone().into(),
        );
        libinput
            .udev_assign_seat(&seat_name)
            .map_err(|_| anyhow!("assigning libinput to seat {seat_name:?}"))?;
        let libinput_backend = LibinputInputBackend::new(libinput.clone());

        let udev = UdevBackend::new(&seat_name)
            .with_context(|| format!("starting udev monitor for seat {seat_name:?}"))?;

        let mut direct = Self {
            output: None,
            elements: Vec::new(),
            renderer,
            gbm,
            drm,
            libinput,
            keyboards: Vec::new(),
            session,
            cursor: SoftwareCursor::load(),
            retired_globals: Vec::new(),
            seat_name,
            device_path,
            device_id,
            connector_filter: options.connector.clone(),
            driver,
            active: true,
            pending_since: None,
            confirm_lock_on_flip: None,
            retry_scanout: false,
            stall_retries: 0,
        };
        direct.setup_output(state)?;
        state.backend_name = "direct-drm-gles";
        state.software_cursor = true;
        direct.update_status(state);

        handle
            .insert_source(drm_notifier, |event, _, data| {
                if let Backend::Direct(direct) = &mut data.backend {
                    direct.on_drm_event(&mut data.state, event);
                }
            })
            .map_err(|error| error.error)
            .context("registering DRM event source")?;
        handle
            .insert_source(udev, |event, _, data| {
                if let Backend::Direct(direct) = &mut data.backend {
                    direct.on_udev_event(&mut data.state, event);
                }
            })
            .map_err(|error| error.error)
            .context("registering udev event source")?;
        handle
            .insert_source(libinput_backend, |event, _, data| {
                if let Backend::Direct(direct) = &mut data.backend {
                    direct.process_event(&mut data.state, event);
                }
            })
            .map_err(|error| error.error)
            .context("registering libinput event source")?;
        handle
            .insert_source(notifier, |event, _, data| {
                if let Backend::Direct(direct) = &mut data.backend {
                    direct.session_event(&mut data.state, event);
                }
            })
            .map_err(|error| error.error)
            .context("registering libseat event source")?;

        // A page flip can be lost by a misbehaving driver or an interrupted
        // session. This periodic source is the only unconditional wakeup in the
        // direct backend and recovers a stalled frame.
        handle
            .insert_source(
                Timer::from_duration(Duration::from_secs(1)),
                |_, _, data| {
                    if let Backend::Direct(direct) = &mut data.backend {
                        direct.tick(&mut data.state);
                    }
                    TimeoutAction::ToDuration(Duration::from_secs(1))
                },
            )
            .map_err(|error| error.error)
            .context("registering DRM watchdog")?;

        info!(
            device = %direct.device_path.display(),
            driver = %direct.driver,
            seat = %direct.seat_name,
            "direct DRM backend initialized"
        );
        Ok(direct)
    }

    pub(super) fn ready_to_render(&self, state: &Wisp) -> bool {
        state.shutdown.is_none()
            && self.active
            && self.output.is_some()
            && self.pending_since.is_none()
            && !self.retry_scanout
    }

    fn setup_output(&mut self, state: &mut Wisp) -> Result<()> {
        let connectors = output::scan(&self.drm, true).context("scanning DRM connectors")?;
        let connector = output::select_connector(&connectors, self.connector_filter.as_deref())?;
        self.configure_output(state, connector)
    }

    fn configure_output(
        &mut self,
        state: &mut Wisp,
        connector: &output::ConnectorInfo,
    ) -> Result<()> {
        let drm_mode = output::select_mode_configured(
            connector,
            state
                .config
                .render
                .direct_mode
                .as_deref()
                .map(crate::config::DirectMode::parse)
                .transpose()?,
        )?;
        let crtc = output::select_crtc(&self.drm, connector)?;
        let mode = OutputMode::from(drm_mode);
        let name = connector.name.clone();
        let physical_size = connector.size.unwrap_or((0, 0));
        let subpixel = connector.subpixel;

        let drm_surface = self
            .drm
            .create_surface(crtc, drm_mode, &[connector.handle])
            .with_context(|| format!("creating DRM surface for {name}"))?;
        let allocator = GbmAllocator::new(
            self.gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let render_formats = self
            .renderer
            .borrow()
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let surface =
            GbmBufferedSurface::new(drm_surface, allocator, COLOR_FORMATS, render_formats)
                .map_err(|error| anyhow!("creating GBM swapchain for {name}: {error}"))?;

        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: (physical_size.0 as i32, physical_size.1 as i32).into(),
                subpixel,
                make: "Unknown".into(),
                model: name.clone(),
            },
        );
        let global = output.create_global::<Wisp>(&state.display_handle);
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            None,
            Some((0, 0).into()),
        );
        output.set_preferred(mode);
        state.space.map_output(&output, (0, 0));
        state.output = Some(output.clone());
        state.reconfigure_lock_surface();
        let damage = OutputDamageTracker::from_output(&output);
        self.output = Some(OutputState {
            output,
            global,
            connector: connector.handle,
            crtc,
            mode,
            surface,
            damage,
        });
        state.arrange();
        state.dirty = true;
        info!(
            output = %name,
            width = mode.size.w,
            height = mode.size.h,
            refresh_hz = mode.refresh as f64 / 1000.0,
            "direct output configured"
        );
        direct_pointer_center(state);
        self.update_status(state);
        Ok(())
    }

    fn teardown_output(&mut self, state: &mut Wisp) {
        self.confirm_lock_on_flip = None;
        state.cancel_workspace_slide();
        if let Some(output) = self.output.take() {
            state.space.unmap_output(&output.output);
            // Keep a short binding grace period for clients racing hotplug.
            state
                .display_handle
                .disable_global::<Wisp>(output.global.clone());
            self.retired_globals.push((Instant::now(), output.global));
            if state
                .output
                .as_ref()
                .is_some_and(|current| current == &output.output)
            {
                state.output = None;
            }
            state.arrange();
            state.dirty = true;
            self.pending_since = None;
            info!(output = %output.output.name(), "direct output disconnected");
            self.update_status(state);
        }
    }

    fn on_udev_event(&mut self, state: &mut Wisp, event: UdevEvent) {
        if state.shutdown.is_some() {
            return;
        }
        match event {
            UdevEvent::Changed { device_id } if device_id == self.device_id => {
                // The activation handler rescans changes received while paused.
                if self.active {
                    if let Err(error) = self.refresh_connector(state) {
                        state.fail(error.context("updating direct output"));
                    }
                }
            }
            UdevEvent::Removed { device_id } if device_id == self.device_id => {
                state.fail(anyhow!(
                    "DRM device {} disappeared",
                    self.device_path.display()
                ));
            }
            UdevEvent::Added { path, .. } => {
                debug!(path = %path.display(), "ignoring additional DRM device");
            }
            _ => {}
        }
    }

    fn refresh_connector(&mut self, state: &mut Wisp) -> Result<()> {
        let connectors = output::scan(&self.drm, true).context("rescanning DRM connectors")?;
        let requested = state
            .config
            .render
            .direct_mode
            .as_deref()
            .map(crate::config::DirectMode::parse)
            .transpose()?;
        if let Some(current) = &self.output {
            let unchanged = connectors.iter().any(|connector| {
                connector.handle == current.connector
                    && connector.connected
                    && output::select_mode_configured(connector, requested).ok()
                        == Some(current.surface.pending_mode())
            });
            if !unchanged {
                // Stop scanout before releasing buffers or reusing this CRTC.
                self.reset_scanout(state)?;
                self.teardown_output(state);
            }
        }
        if self.output.is_none() {
            match output::select_connector(&connectors, self.connector_filter.as_deref()) {
                Ok(connector) => self.configure_output(state, connector)?,
                Err(error) => debug!(%error, "no direct output is currently available"),
            }
        }
        Ok(())
    }

    fn session_event(&mut self, state: &mut Wisp, event: SessionEvent) {
        match event {
            SessionEvent::PauseSession => {
                if !self.active {
                    return;
                }
                self.active = false;
                state.dmabuf_session_active = false;
                self.libinput.suspend();
                self.drm.pause();
                self.pending_since = None;
                self.confirm_lock_on_flip = None;
                self.retry_scanout = false;
                state.host_focus(false);
                self.update_status(state);
                info!("direct session paused; display and input released");
            }
            SessionEvent::ActivateSession => {
                if self.active || state.shutdown.is_some() {
                    return;
                }
                // Another compositor may have changed every plane while away.
                // A blocking reset gives us known hardware state before reuse.
                if let Err(error) = self.drm.activate(true) {
                    state.fail(anyhow!("reactivating DRM device: {error}"));
                    return;
                }
                self.active = true;
                self.stall_retries = 0;
                if let Err(error) = self
                    .finish_scanout_reset(state)
                    .and_then(|_| self.refresh_connector(state))
                {
                    state.fail(error.context("restoring direct output after session activation"));
                    return;
                }
                if self.libinput.resume().is_err() {
                    state.fail(anyhow!("resuming libinput after session switch"));
                    return;
                }
                state.host_focus(true);
                state.dmabuf_session_active = true;
                state.dirty = true;
                self.sync_leds(state);
                self.update_status(state);
                info!("direct session activated; display and input resumed");
            }
        }
    }

    fn on_drm_event(&mut self, state: &mut Wisp, event: DrmEvent) {
        if !self.active || state.shutdown.is_some() {
            return;
        }
        match event {
            DrmEvent::VBlank(crtc) => {
                if let Some(output) = &mut self.output {
                    if output.crtc == crtc && self.pending_since.is_some() {
                        match output.surface.frame_submitted() {
                            Ok(Some(())) => {
                                let wait_us =
                                    self.pending_since.unwrap().elapsed().as_micros() as u64;
                                state.stats.flip_wait_us += wait_us;
                                state.stats.flip_over_25ms += u64::from(wait_us > 25_000);
                                state.stats.flip_over_40ms += u64::from(wait_us > 40_000);
                                self.pending_since = None;
                                self.stall_retries = 0;
                                if let Some(queued) = self.confirm_lock_on_flip.take() {
                                    if state.pending_session_lock_id().as_ref() == Some(&queued) {
                                        state.confirm_session_lock();
                                    }
                                }
                            }
                            Ok(None) => {}
                            Err(error) => {
                                state.fail(anyhow!("DRM page flip completion failed: {error}"));
                            }
                        }
                    }
                }
            }
            // A reset may have drained a descriptor already marked ready by
            // calloop; that stale readiness is harmless.
            DrmEvent::Error(DrmError::Access(error))
                if matches!(
                    error.source.kind(),
                    ErrorKind::WouldBlock | ErrorKind::Interrupted
                ) => {}
            DrmEvent::Error(error) => {
                warn!(%error, "DRM event source reported an error");
                state.fail(anyhow!("DRM event error: {error}"));
            }
        }
    }

    fn tick(&mut self, state: &mut Wisp) {
        self.retired_globals.retain(|(retired, global)| {
            if retired.elapsed() < GLOBAL_RETIRE_DELAY {
                return true;
            }
            state.display_handle.remove_global::<Wisp>(global.clone());
            false
        });
        if state.shutdown.is_some() {
            return;
        }
        if self.active
            && (self.retry_scanout
                || self
                    .pending_since
                    .is_some_and(|started| started.elapsed() >= FLIP_TIMEOUT))
        {
            if self.stall_retries >= 3 {
                state.fail(anyhow!(
                    "DRM page flips still stalled after three scanout resets"
                ));
                return;
            }
            self.stall_retries += 1;
            warn!(
                attempt = self.stall_retries,
                "page flip stalled; resetting scanout"
            );
            if let Err(error) = self.reset_scanout(state) {
                state.fail(error.context("recovering stalled DRM scanout"));
            }
        }
        self.sync_leds(state);
    }

    fn reset_scanout(&mut self, state: &mut Wisp) -> Result<()> {
        self.confirm_lock_on_flip = None;
        // A blocking reset may fail with EINVAL when the kernel considers the
        // previous KMS state already clean (e.g., after another compositor
        // released everything). The error is harmless: no obsolete frames
        // are pending and the surface buffers are still reset below.
        if let Err(error) = self.drm.reset_state() {
            let msg = error.to_string();
            if msg.contains("invalid argument") || msg.contains("os error 22") {
                warn!(
                    "DRM reset_state returned EINVAL (previous state likely already clean); continuing recovery"
                );
            } else {
                return Err(error).context("disabling DRM planes for reset");
            }
        }
        self.finish_scanout_reset(state)
    }

    /// Called only after a blocking KMS reset has stopped old scanout. There is
    /// at most one submitted buffer and no queued successor in this backend.
    fn finish_scanout_reset(&mut self, state: &mut Wisp) -> Result<()> {
        loop {
            match self.drm.receive_events() {
                Ok(mut events) => {
                    if events.next().is_none() {
                        break;
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("draining obsolete DRM events"),
            }
        }
        if let Some(output) = &mut self.output {
            // reset_buffers alone does not clear GbmBufferedSurface.pending_fb.
            // Retire it only now, when the kernel no longer uses the old frame.
            output
                .surface
                .frame_submitted()
                .context("retiring old scanout buffer")?;
            output.surface.reset_buffers();
            output.damage = OutputDamageTracker::from_output(&output.output);
        }
        self.pending_since = None;
        self.retry_scanout = false;
        state.dirty = true;
        Ok(())
    }

    fn scanout_error(
        &mut self,
        state: &mut Wisp,
        error: impl Into<SwapBuffersError>,
        operation: &'static str,
    ) -> Result<()> {
        match error.into() {
            SwapBuffersError::TemporaryFailure(error) => {
                // Access can disappear before libseat's pause notification is
                // dispatched. Keep the redraw and let activation/the watchdog
                // recover; do not spin another immediate render timer.
                warn!(%error, operation, "temporarily deferring direct frame");
                self.retry_scanout = true;
                state.dirty = true;
                Ok(())
            }
            error => Err(anyhow!("{operation}: {error}")),
        }
    }

    fn process_event(&mut self, state: &mut Wisp, event: InputEvent<LibinputInputBackend>) {
        match &event {
            InputEvent::DeviceAdded { device } => {
                if <LibinputDevice as InputDevice>::has_capability(
                    device,
                    DeviceCapability::Keyboard,
                ) {
                    let mut device = device.clone();
                    if let Some(keyboard) = state.seat.get_keyboard() {
                        let leds: Led = keyboard.led_state().into();
                        device.led_update(leds);
                    }
                    self.keyboards.push(device);
                }
            }
            InputEvent::DeviceRemoved { device } => {
                let held_input = <LibinputDevice as InputDevice>::has_capability(
                    device,
                    DeviceCapability::Keyboard,
                ) || <LibinputDevice as InputDevice>::has_capability(
                    device,
                    DeviceCapability::Pointer,
                );
                self.keyboards.retain(|keyboard| keyboard != device);
                if held_input {
                    // The kernel does not synthesize releases for a removed
                    // device, so drop any Mod/button state it may have left.
                    state.release_input();
                    state.dirty = true;
                }
            }
            _ => {}
        }
        if !self.active || state.shutdown.is_some() {
            return;
        }
        if let InputEvent::Keyboard { event } = event {
            if let Some(vt) =
                state.keyboard_key(event.key_code(), event.state(), event.time_msec(), true)
            {
                if let Err(error) = self.session.change_vt(vt) {
                    warn!(vt, %error, "could not switch VT");
                }
            }
        } else {
            state.process_input(event);
        }
        self.sync_leds(state);
    }

    fn sync_leds(&mut self, state: &mut Wisp) {
        if !self.active {
            return;
        }
        let Some(leds) = state.pending_led_state.take() else {
            return;
        };
        let leds: Led = leds.into();
        for keyboard in &mut self.keyboards {
            keyboard.led_update(leds);
        }
    }

    fn update_status(&self, state: &mut Wisp) {
        let output = self.output.as_ref().map(|output| {
            serde_json::json!({
                "name": output.output.name(),
                "width": output.mode.size.w,
                "height": output.mode.size.h,
                "refresh_hz": output.mode.refresh as f64 / 1000.0,
            })
        });
        state.backend_details = serde_json::json!({
            "drm_device": self.device_path.display().to_string(),
            "driver": self.driver,
            "seat": self.seat_name,
            "session_active": self.active,
            "output": output,
        });
    }

    pub(super) fn render(&mut self, state: &mut Wisp) -> Result<()> {
        if !self.ready_to_render(state) {
            return Ok(());
        }
        let named = match &state.cursor_status {
            CursorImageStatus::Named(icon) => Some(*icon),
            _ => None,
        };
        self.cursor.set_icon(if state.session_lock.is_some() {
            None
        } else {
            named
        });

        let start = Instant::now();
        let renderer_handle = Rc::clone(&self.renderer);
        let mut renderer = renderer_handle.borrow_mut();
        super::scene::elements(state, &mut renderer, &mut self.elements);
        if let Some(pointer) = state
            .seat
            .get_pointer()
            .filter(|_| state.session_lock.is_none())
        {
            if let Some(element) = self
                .cursor
                .element(&mut renderer, pointer.current_location())
                .context("rendering software cursor")?
            {
                self.elements
                    .insert(0, super::scene::Element::Memory(element));
            }
        }
        state.stats.scene_wall_us += start.elapsed().as_micros() as u64;

        let bind_start = Instant::now();
        let Some(output) = self.output.as_mut() else {
            return Ok(());
        };
        let (mut buffer, age) = match output.surface.next_buffer() {
            Ok(buffer) => buffer,
            Err(error) => return self.scanout_error(state, error, "acquiring a GBM buffer"),
        };
        let mut target = renderer
            .bind(&mut buffer)
            .map_err(|error| anyhow!("binding GBM buffer for rendering: {error}"))?;
        state.stats.bind_wall_us += bind_start.elapsed().as_micros() as u64;
        let compose_start = Instant::now();
        let result = output
            .damage
            .render_output(
                &mut *renderer,
                &mut target,
                usize::from(age),
                &self.elements,
                if state.session_lock.is_some() {
                    [0.0, 0.0, 0.0, 1.0]
                } else {
                    state.config.decoration.background.0
                },
            )
            .map_err(|error| anyhow!("rendering direct frame: {error}"))?;
        let sync = result.sync;
        let damage = result.damage.cloned();
        drop(target);
        state.stats.compose_wall_us += compose_start.elapsed().as_micros() as u64;
        // CPU-only cadence check: scene + bind + compose, before queueing.
        let cpu_us = start.elapsed().as_micros() as u64;
        state.stats.render_cpu_us += cpu_us;
        state.stats.cpu_over_8ms += u64::from(cpu_us > 8_000);
        state.stats.cpu_over_16ms += u64::from(cpu_us > 16_000);

        let present_start = Instant::now();
        if let Some(damage) = damage {
            if let Err(error) = output.surface.queue_buffer(Some(sync), Some(damage), ()) {
                return self.scanout_error(state, error, "queueing GBM buffer for scanout");
            }
            self.pending_since = Some(Instant::now());
            self.confirm_lock_on_flip = state.pending_session_lock_id();
            state.stats.rendered += 1;
        } else {
            state.stats.no_damage += 1;
            state.confirm_session_lock();
        }
        state.stats.present_wall_us += present_start.elapsed().as_micros() as u64;

        super::scene::send_frame_callbacks(state, &output.output);
        state.stats.last_cpu_render_us = start.elapsed().as_micros() as u64;
        state.stats.render_over_8ms += u64::from(state.stats.last_cpu_render_us > 8_000);
        state.stats.render_over_16ms += u64::from(state.stats.last_cpu_render_us > 16_000);
        state.display_handle.flush_clients()?;
        Ok(())
    }
}

fn direct_pointer_center(state: &mut Wisp) {
    let Some(output) = state.output.clone() else {
        return;
    };
    let Some(geometry) = state.space.output_geometry(&output) else {
        return;
    };
    let center = Point::from((
        f64::from(geometry.loc.x) + f64::from(geometry.size.w) / 2.0,
        f64::from(geometry.loc.y) + f64::from(geometry.size.h) / 2.0,
    ));
    state.pointer_motion(center, 0);
}
