mod blur;
mod direct;
mod dmabuf;
mod effects;
mod grabs;
mod handlers;
mod input;
mod navigation;
mod nested;
mod overview;
mod protocols;
mod scene;
mod session_lock;
mod spring;
mod visual;
mod window_fade;
mod workspace_manager;
mod workspace_slide;
mod xwayland;

use std::{
    cell::RefCell,
    collections::{HashSet, VecDeque},
    ffi::OsString,
    io::Write,
    path::PathBuf,
    process::{Child, Command, Stdio},
    rc::Weak,
    sync::Arc,
    sync::mpsc::{self, SyncSender, TrySendError},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde_json::json;
use smithay::{
    backend::renderer::{
        element::{Id, solid::SolidColorBuffer},
        gles::GlesRenderer,
        utils::CommitCounter,
    },
    desktop::{PopupManager, Space, Window, WindowSurfaceType, layer_map_for_output},
    input::{
        Seat, SeatState,
        keyboard::{LedState, XkbConfig},
        pointer::CursorImageStatus,
    },
    output::{Mode as OutputMode, Output},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopSignal, Mode, PostAction,
            channel::Event,
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{IsAlive, Logical, Point, SERIAL_COUNTER, Serial},
    wayland::{
        compositor::{CompositorClientState, CompositorState, with_states},
        cursor_shape::CursorShapeManagerState,
        dmabuf::DmabufState,
        fractional_scale::FractionalScaleManagerState,
        input_method::InputMethodManagerState,
        output::OutputManagerState,
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        session_lock::SessionLockManagerState,
        shell::xdg::{
            ToplevelSurface, XdgShellState, XdgToplevelSurfaceData, decoration::XdgDecorationState,
        },
        shm::ShmState,
        socket::ListeningSocketSource,
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        xdg_activation::XdgActivationState,
    },
    wayland::{
        seat::WaylandFocus,
        shell::wlr_layer::{Layer, WlrLayerShellState},
    },
};
use tracing::{error, info, warn};

use crate::{
    config::{Binding, Config},
    ipc::{IpcServer, Message},
    layout::{self, LayoutKind, Rect},
    protocol::{Action, Request, Response},
};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(25);

struct Shutdown {
    reason: &'static str,
    deadline: Instant,
    timer_scheduled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum BackendKind {
    #[default]
    Nested,
    Direct,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub backend: BackendKind,
    pub drm_device: Option<PathBuf>,
    pub connector: Option<String>,
    pub seat: Option<String>,
    pub status_stdout: bool,
}

enum Backend {
    Nested(Box<nested::Nested>),
    Direct(Box<direct::Direct>),
}

impl Backend {
    fn name(&self) -> &'static str {
        match self {
            Self::Nested(_) => "nested-winit-gles",
            Self::Direct(_) => "direct-drm-gles",
        }
    }

    fn ready_to_render(&self, state: &Wisp) -> bool {
        match self {
            Self::Nested(_) => true,
            Self::Direct(direct) => direct.ready_to_render(state),
        }
    }

    fn render(&mut self, state: &mut Wisp) -> Result<()> {
        match self {
            Self::Nested(nested) => nested.render(state),
            Self::Direct(direct) => direct.render(state),
        }
    }
}

pub struct LoopData {
    state: Wisp,
    backend: Backend,
    /// The initial command and autostart wait for an opt-in XWM to be ready,
    /// so their terminals inherit a usable DISPLAY rather than the host one.
    deferred_command: Option<Vec<String>>,
}

pub struct Wisp {
    pub(crate) start_time: Instant,
    pub(crate) display_handle: DisplayHandle,
    socket_name: OsString,
    ipc_path: PathBuf,
    config_path: PathBuf,
    pub(crate) config: Config,
    pub(crate) bindings: Vec<Binding>,
    pub(crate) suppressed_keys: HashSet<u32>,
    pub(crate) pressed_buttons: HashSet<u32>,
    slide_buttons: HashSet<u32>,
    host_focused: bool,
    pub(crate) space: Space<Window>,
    pub(crate) windows: Vec<ManagedWindow>,
    x11_unmanaged: Vec<Window>,
    next_id: u64,
    focused: Option<u64>,
    scratchpad: Option<u64>,
    active_workspace: u8,
    previous_workspace: Option<u8>,
    workspace_slide: Option<workspace_slide::WorkspaceSlide>,
    /// Last focused window ID per configured workspace; no client references.
    workspace_focus: Vec<Option<u64>>,
    layouts: Vec<LayoutKind>,
    output: Option<Output>,
    children: Vec<Child>,
    shutdown: Option<Shutdown>,
    exit_error: Option<anyhow::Error>,
    pub(crate) dirty: bool,
    frame_scheduled: bool,
    last_frame: Instant,
    overview: bool,
    overview_exiting: bool,
    overview_click: Option<u32>,
    overview_dim_id: Id,
    overview_dim_commit: CommitCounter,
    pub(crate) overview_rects: Vec<(u64, Rect)>,
    pub(crate) backend_name: &'static str,
    pub(crate) backend_details: serde_json::Value,
    status_stdout: Option<SyncSender<String>>,
    last_stdout_status: Option<serde_json::Value>,
    pub(crate) software_cursor: bool,
    pub(crate) pending_led_state: Option<LedState>,
    pub(crate) stats: FrameStats,
    pub(crate) cursor_status: CursorImageStatus,
    pub(crate) loop_signal: LoopSignal,
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    _decoration_state: XdgDecorationState,
    shm_state: ShmState,
    /// Direct-only v4 linux-dmabuf. Nested retains the wl_shm fallback.
    dmabuf_state: DmabufState,
    dmabuf_renderer: Option<Weak<RefCell<GlesRenderer>>>,
    dmabuf_session_active: bool,
    _output_manager_state: OutputManagerState,
    session_lock_state: Option<SessionLockManagerState>,
    session_lock: Option<session_lock::LockedSession>,
    xwayland_shell_state: Option<smithay::wayland::xwayland_shell::XWaylandShellState>,
    x11_wm: Option<smithay::xwayland::X11Wm>,
    x_display: Option<String>,
    x11_focus: Option<smithay::xwayland::X11Surface>,
    x11_loop_handle: Option<smithay::reexports::calloop::LoopHandle<'static, LoopData>>,
    x11_pending_selections: [Option<xwayland::PendingSelection>; 2],
    seat_state: SeatState<Self>,
    data_device_state: DataDeviceState,
    primary_selection_state: PrimarySelectionState,
    activation_state: XdgActivationState,
    recent_input: VecDeque<(Serial, ClientId, Instant)>,
    _cursor_shape_state: CursorShapeManagerState,
    _fractional_scale_state: FractionalScaleManagerState,
    _viewporter_state: ViewporterState,
    _layer_shell_state: WlrLayerShellState,
    /// Canonical desktop handles for layer surfaces, created once per shell
    /// surface: the layer map keys them by identity, so constructing fresh
    /// handles per commit would break map membership. Dead entries are
    /// pruned on mapping activity.
    pub(crate) layer_surfaces: Vec<smithay::desktop::LayerSurface>,
    /// Effect shader programs, compiled against the backend's renderer on the
    /// first render pass. `None` keeps scenes rendering without effects.
    pub(crate) effects: Option<effects::EffectsShaders>,
    toplevel_icon_state: protocols::icons::IconManager,
    _text_input_state: TextInputManagerState,
    _input_method_state: InputMethodManagerState,
    pub(crate) ext_workspace: workspace_manager::ExtWorkspaceManagerState,
    pub(crate) materialized_workspaces: Vec<u8>,
    /// Highest workspace explicitly created by a shell but not yet visited.
    pub(crate) created_workspace: Option<u8>,
    pub(crate) popups: PopupManager,
    pub(crate) seat: Seat<Self>,
}

pub(crate) struct ManagedWindow {
    id: u64,
    window: Window,
    workspace: u8,
    scratchpad_hidden: bool,
    floating: bool,
    fullscreen: bool,
    urgent: bool,
    mapped: bool,
    participating: bool,
    rules_applied: bool,
    floating_rect: Rect,
    rect: Rect,
    spring: Option<spring::WindowSpring>,
    fade: Option<window_fade::WindowFade>,
    borders: [SolidColorBuffer; 4],
    /// Stable damage-tracking identity for the inactive-window dim overlay.
    pub(crate) dim_id: Id,
    pub(crate) dim_commit: CommitCounter,
    pub(crate) dim_strength: f32,
    /// GPU decoration elements (gradient border + shadow) for the effects
    /// path; `None` until a decorated frame actually renders.
    pub(crate) decor: effects::WindowDecor,
}

impl ManagedWindow {
    fn wl_surface(&self) -> Option<WlSurface> {
        self.window.wl_surface().map(|surface| surface.into_owned())
    }

    /// Close the underlying client without assuming all desktop windows use
    /// xdg-shell. X11 windows have a separate WM_DELETE_WINDOW path.
    fn send_close(&self) {
        if let Some(toplevel) = self.window.toplevel() {
            toplevel.send_close();
        } else if let Some(x11) = self.window.x11_surface() {
            if let Err(error) = x11.close() {
                warn!(%error, "closing X11 window");
            }
        }
    }
}

#[derive(Default, serde::Serialize)]
pub(crate) struct FrameStats {
    pub rendered: u64,
    pub no_damage: u64,
    pub client_commits: u64,
    pub skipped_invisible_commits: u64,
    pub host_redraws: u64,
    /// Wall time of the whole last redraw cycle, including nested present
    /// parking and direct queueing. Split CPU-only work is in `render_cpu_us`.
    pub last_cpu_render_us: u64,
    /// Cumulative wall-clock time for the major render phases. These can
    /// include GPU waits and are not CPU or GPU execution-time measurements.
    pub scene_wall_us: u64,
    pub bind_wall_us: u64,
    pub compose_wall_us: u64,
    pub present_wall_us: u64,
    /// Slow render calls (inclusive of nested swap or direct queueing).
    pub render_over_8ms: u64,
    pub render_over_16ms: u64,
    /// CPU-side render work per frame (bind + scene + compose), excluding
    /// present parking and flip waits. A call over 16 ms cannot keep 60 fps.
    pub render_cpu_us: u64,
    pub cpu_over_8ms: u64,
    pub cpu_over_16ms: u64,
    /// Direct page flips taking longer than roughly 1.5/2.5 refresh cycles
    /// on a 60 Hz output. Accumulated at vblank, not at queue time.
    pub flip_over_25ms: u64,
    pub flip_over_40ms: u64,
    pub flip_wait_us: u64,
}

pub fn probe_direct(
    device: Option<&std::path::Path>,
    connector: Option<&str>,
    seat: &str,
) -> Result<()> {
    direct::probe_direct(device, connector, seat)
}

pub fn run(config: Config, path: PathBuf, command: Vec<String>, options: Options) -> Result<()> {
    let mut event_loop: EventLoop<LoopData> = EventLoop::try_new()?;
    let display: Display<Wisp> = Display::new()?;
    let dh = display.handle();
    let listener = ListeningSocketSource::new_auto()?;
    let socket_name = listener.socket_name().to_os_string();
    let (ipc, channel) =
        IpcServer::start(socket_name.to_str().context("non-UTF8 Wayland socket")?)?;
    let (session, notifier, seat_name) = if options.backend == BackendKind::Direct {
        use smithay::backend::session::Session as _;
        let (session, notifier) = smithay::backend::session::libseat::LibSeatSession::new()
            .context("opening libseat session (is seatd or logind available?)")?;
        let seat_name = session.seat();
        if let Some(requested) = &options.seat {
            ensure!(
                requested == &seat_name,
                "requested seat {requested:?}, but libseat provided {seat_name:?}"
            );
        }
        (Some(session), Some(notifier), seat_name)
    } else {
        (None, None, "wisp-nested".to_owned())
    };
    let mut state = Wisp::new(
        config,
        path,
        dh,
        socket_name,
        ipc.path().to_owned(),
        event_loop.get_signal(),
        &seat_name,
    )?;
    let handle = event_loop.handle();
    let backend = match options.backend {
        BackendKind::Nested => {
            let (nested, winit) = nested::Nested::new(&mut state)?;
            handle
                .insert_source(winit, |event, _, data| {
                    if let Backend::Nested(nested) = &mut data.backend {
                        nested.event(&mut data.state, event);
                    }
                })
                .map_err(|error| error.error)
                .context("registering nested event loop")?;
            Backend::Nested(Box::new(nested))
        }
        BackendKind::Direct => Backend::Direct(Box::new(direct::Direct::new(
            &mut state,
            session.context("direct backend requires a libseat session")?,
            notifier.context("direct backend requires a libseat notifier")?,
            &options,
            &handle,
        )?)),
    };
    if options.status_stdout {
        state.start_stdout_status()?;
    }
    let mut data = LoopData {
        state,
        backend,
        deferred_command: None,
    };
    if data.state.config.general.xwayland {
        data.deferred_command = Some(command.clone());
        xwayland::start(&handle, &mut data)?;
    }
    data.state.emit_stdout_status();
    handle.insert_source(listener, |stream, _, data| {
        if data.state.shutdown.is_some() {
            return;
        }
        if let Err(error) = data
            .state
            .display_handle
            .insert_client(stream, Arc::new(ClientState::default()))
        {
            warn!(%error, "could not accept Wayland client");
        }
    })?;
    handle.insert_source(
        Generic::new(display, Interest::READ, Mode::Level),
        |_, display, data| {
            // Generic requires unsafe access to its pinned I/O object. We never move or drop it.
            if let Err(error) = unsafe { display.get_mut().dispatch_clients(&mut data.state) } {
                data.state
                    .fail(anyhow::Error::new(error).context("Wayland dispatch failed"));
                // Retain the display during bounded cleanup, without retrying a
                // broken dispatch source in a busy loop.
                return Ok(PostAction::Disable);
            }
            Ok(PostAction::Continue)
        },
    )?;
    handle
        .insert_source(channel, |event, _, data| {
            match event {
                Event::Msg(Message::Request { request, reply }) => {
                    let response = if matches!(
                        &request,
                        Request::Dispatch {
                            action: Action::Quit {}
                        }
                    ) {
                        // Wait for the worker to write this acknowledgement first.
                        if data.state.session_lock.is_some() {
                            Response::failure("session is locked")
                        } else {
                            Response::success(json!({}))
                        }
                    } else {
                        data.state.request(request)
                    };
                    let _ = reply.try_send(response);
                }
                Event::Msg(Message::QuitAcknowledged) => {
                    if data.state.session_lock.is_none() {
                        data.state.begin_shutdown("IPC quit requested");
                    }
                }
                Event::Closed => {}
            }
        })
        .map_err(|error| error.error)
        .context("registering IPC channel")?;
    info!(
        backend = data.backend.name(),
        seat = %seat_name,
        wayland = ?data.state.socket_name,
        ipc = %ipc.path().display(),
        config = %data.state.config_path.display(),
        "Wisp compositor ready"
    );
    info!(
        "Alt+Return: terminal; Alt+j/k: focus; Alt+Space: float; Alt+left/right drag: move/resize; Alt+Shift+Escape: quit"
    );
    if !data.state.config.general.xwayland {
        if !command.is_empty() {
            data.state.spawn(&command)?;
        }
        data.state.start_autostart();
    }

    let result = event_loop.run(None, &mut data, move |data| {
        data.state.emit_stdout_status();
        data.state.space.refresh();
        data.state.popups.cleanup();
        data.state.reap_children();
        if let Err(error) = data.state.display_handle.flush_clients() {
            data.state
                .fail(anyhow::Error::new(error).context("flushing Wayland clients"));
        }
        if let Some(shutdown) = &mut data.state.shutdown {
            if !shutdown.timer_scheduled {
                shutdown.timer_scheduled = true;
                if let Err(error) = handle.insert_source(Timer::immediate(), |_, _, data| {
                    if data.state.shutdown_finished() {
                        data.state.loop_signal.stop();
                        TimeoutAction::Drop
                    } else {
                        TimeoutAction::ToDuration(SHUTDOWN_POLL_INTERVAL)
                    }
                }) {
                    data.state
                        .fail(anyhow::anyhow!("could not schedule shutdown: {error}"));
                    data.state.loop_signal.stop();
                }
            }
            return;
        }
        if data.state.dirty
            && !data.state.frame_scheduled
            && data.backend.ready_to_render(&data.state)
        {
            let interval = Duration::from_secs_f64(1.0 / data.state.config.render.max_fps as f64);
            let delay = interval.saturating_sub(data.state.last_frame.elapsed());
            data.state.frame_scheduled = true;
            if let Err(error) = handle.insert_source(Timer::from_duration(delay), |_, _, data| {
                data.state.frame_scheduled = false;
                if data.state.shutdown.is_some() {
                    return TimeoutAction::Drop;
                }
                // A direct frame can still be waiting for its vblank. Keep `dirty`
                // set so a commit that arrived during the wait is not lost; the
                // vblank handler or watchdog will make the backend ready again.
                if !data.backend.ready_to_render(&data.state) {
                    return TimeoutAction::Drop;
                }
                data.state.dirty = false;
                data.state.last_frame = Instant::now();
                if let Err(error) = data.backend.render(&mut data.state) {
                    data.state.fail(error.context("rendering failed"));
                }
                data.state.finish_overview_transition();
                data.state.finish_workspace_slide(Instant::now());
                // Keep the on-demand render loop alive only while visible
                // spring geometry is still changing. Direct mode still waits
                // for the page flip before scheduling another frame.
                if data.state.windows.iter().any(|window| {
                    window.spring.is_some() || (window.fade.is_some() && data.state.visible(window))
                }) || data.state.workspace_slide.is_some()
                {
                    data.state.dirty = true;
                }
                TimeoutAction::Drop
            }) {
                data.state
                    .fail(anyhow::anyhow!("could not schedule frame: {error}"));
            }
        }
    });
    if let Err(error) = result {
        data.state
            .fail(anyhow::Error::new(error).context("running compositor event loop"));
    }
    if data.state.shutdown.is_none() {
        data.state.fail(anyhow::anyhow!(
            "compositor event loop stopped unexpectedly"
        ));
    }
    let reason = data.state.shutdown.as_ref().unwrap().reason;
    let remaining_clients = data.state.client_count();
    let remaining_children = data.state.children.len();
    if let Some(error) = data.state.exit_error.take() {
        error!(reason, remaining_clients, remaining_children, error = %format!("{error:#}"), "Wisp exited with an error");
        return Err(error);
    }
    info!(
        reason,
        remaining_clients, remaining_children, "Wisp stopped"
    );
    Ok(())
}

impl Wisp {
    fn start_stdout_status(&mut self) -> Result<()> {
        // A slow bar must never block input or rendering. Keep only one
        // pending snapshot and retry on the next event-loop turn if full.
        let (sender, receiver) = mpsc::sync_channel::<String>(1);
        std::thread::Builder::new()
            .name("wisp-status-stdout".into())
            .spawn(move || {
                let stdout = std::io::stdout();
                let mut output = stdout.lock();
                for line in receiver {
                    if output.write_all(line.as_bytes()).is_err() || output.flush().is_err() {
                        break; // closed pipe: stop streaming, not the compositor
                    }
                }
            })?;
        self.status_stdout = Some(sender);
        Ok(())
    }

    fn stdout_snapshot(&self) -> serde_json::Value {
        json!({
            "event": "state",
            "workspace": self.active_workspace,
            "scratchpad": self.scratchpad,
            "previous_workspace": self.previous_workspace,
            "layout": self.layout(),
            "focused": self.focused,
            "overview": self.overview,
            "workspace_sliding": self.workspace_slide.is_some(),
            "output": self.output.as_ref().map(|output| output.name()),
            "workspaces": self.materialized_workspaces.iter().map(|index| json!({
                "index": index, "active": *index == self.active_workspace,
                "layout": self.layouts[*index as usize - 1],
                "windows": self.windows.iter().filter(|w| w.workspace == *index && w.mapped && !w.scratchpad_hidden).count(),
                "urgent": self.windows.iter().any(|w| w.workspace == *index && w.urgent && !w.scratchpad_hidden),
            })).collect::<Vec<_>>(),
            "windows": self.windows.iter().map(|window| {
                let (app_id, title) = Self::metadata(&window.window);
                json!({"id": window.id, "workspace": window.workspace,
                    "app_id": app_id, "title": title, "urgent": window.urgent,
                    "mapped": window.mapped, "scratchpad": self.scratchpad == Some(window.id),
                    "scratchpad_hidden": window.scratchpad_hidden})
            }).collect::<Vec<_>>(),
        })
    }

    fn emit_stdout_status(&mut self) {
        let Some(sender) = self.status_stdout.clone() else {
            return;
        };
        let snapshot = self.stdout_snapshot();
        if self.last_stdout_status.as_ref() == Some(&snapshot) {
            return;
        }
        let Ok(line) = serde_json::to_string(&snapshot) else {
            return;
        };
        match sender.try_send(format!("{line}\n")) {
            Ok(()) => self.last_stdout_status = Some(snapshot),
            Err(TrySendError::Full(_)) => {} // retry latest state on next turn
            Err(TrySendError::Disconnected(_)) => self.status_stdout = None,
        }
    }

    fn new(
        config: Config,
        config_path: PathBuf,
        dh: DisplayHandle,
        socket_name: OsString,
        ipc_path: PathBuf,
        loop_signal: LoopSignal,
        seat_name: &str,
    ) -> Result<Self> {
        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let decoration_state = XdgDecorationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let session_lock_state = config
            .general
            .session_lock_protocol
            .then(|| SessionLockManagerState::new::<Self, _>(&dh, |_| true));
        let xwayland_shell_state = config
            .general
            .xwayland
            .then(|| smithay::wayland::xwayland_shell::XWaylandShellState::new::<Self>(&dh));
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        let activation_state = XdgActivationState::new::<Self>(&dh);
        let cursor_shape_state = CursorShapeManagerState::new::<Self>(&dh);
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let ext_workspace = workspace_manager::ExtWorkspaceManagerState::new(&dh);
        let toplevel_icon_state = protocols::icons::IconManager::new(&dh);
        let text_input_state = TextInputManagerState::new::<Self>(&dh);
        // Like the rest of this personal prototype, connected clients are
        // trusted. IME providers can read surrounding text and grab keyboard input.
        let input_method_state = InputMethodManagerState::new::<Self, _>(&dh, |_| true);
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, seat_name);
        seat.add_keyboard(
            XkbConfig {
                layout: &config.input.xkb_layout,
                options: config.input.xkb_options.clone(),
                ..Default::default()
            },
            config.input.repeat_delay,
            config.input.repeat_rate,
        )
        .context("creating keyboard keymap")?;
        seat.add_pointer();
        let bindings = config.compile_bindings()?;
        // Workspace layouts cover the configured ceiling; only the active
        // workspace and one spare start out visible to protocol clients.
        let layouts = vec![config.layout.default; config.general.workspaces as usize];
        let workspace_focus = vec![None; config.general.workspaces as usize];
        let materialized_workspaces = (1..=config.general.workspaces.min(2)).collect();
        Ok(Self {
            start_time: Instant::now(),
            display_handle: dh,
            socket_name,
            ipc_path,
            config_path,
            config,
            bindings,
            suppressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
            slide_buttons: HashSet::new(),
            host_focused: true,
            space: Space::default(),
            windows: Vec::new(),
            x11_unmanaged: Vec::new(),
            next_id: 1,
            focused: None,
            scratchpad: None,
            active_workspace: 1,
            previous_workspace: None,
            workspace_slide: None,
            workspace_focus,
            layouts,
            output: None,
            children: Vec::new(),
            shutdown: None,
            exit_error: None,
            dirty: true,
            frame_scheduled: false,
            last_frame: Instant::now() - Duration::from_secs(1),
            overview: false,
            overview_exiting: false,
            overview_click: None,
            overview_dim_id: Id::new(),
            overview_dim_commit: CommitCounter::default(),
            overview_rects: Vec::new(),
            backend_name: "nested-winit-gles",
            backend_details: json!({}),
            status_stdout: None,
            last_stdout_status: None,
            software_cursor: false,
            pending_led_state: None,
            stats: FrameStats::default(),
            effects: None,
            cursor_status: CursorImageStatus::default_named(),
            loop_signal,
            compositor_state,
            xdg_shell_state,
            _decoration_state: decoration_state,
            shm_state,
            dmabuf_state: DmabufState::new(),
            dmabuf_renderer: None,
            dmabuf_session_active: false,
            _output_manager_state: output_manager_state,
            session_lock_state,
            session_lock: None,
            xwayland_shell_state,
            x11_wm: None,
            x_display: None,
            x11_focus: None,
            x11_loop_handle: None,
            x11_pending_selections: [None, None],
            seat_state,
            data_device_state,
            primary_selection_state,
            activation_state,
            recent_input: VecDeque::new(),
            _cursor_shape_state: cursor_shape_state,
            _fractional_scale_state: fractional_scale_state,
            _layer_shell_state: layer_shell_state,
            layer_surfaces: Vec::new(),
            ext_workspace,
            materialized_workspaces,
            created_workspace: None,
            toplevel_icon_state,
            _viewporter_state: viewporter_state,
            _text_input_state: text_input_state,
            _input_method_state: input_method_state,
            popups: PopupManager::default(),
            seat,
        })
    }

    fn client_count(&self) -> usize {
        let mut count = 0;
        self.display_handle
            .backend_handle()
            .with_all_clients(|_| count += 1);
        count
    }

    fn begin_shutdown(&mut self, reason: &'static str) {
        if self.shutdown.is_some() {
            return;
        }
        info!(
            reason,
            windows = self.windows.len(),
            clients = self.client_count(),
            timeout_ms = SHUTDOWN_TIMEOUT.as_millis(),
            "Wisp shutting down"
        );
        self.shutdown = Some(Shutdown {
            reason,
            deadline: Instant::now() + SHUTDOWN_TIMEOUT,
            timer_scheduled: false,
        });
        self.loop_signal.wakeup();
        // Include hidden workspaces, unmapped windows, and floating windows.
        for entry in &self.windows {
            entry.send_close();
        }
        for layer in &self.layer_surfaces {
            if layer.alive() {
                layer.layer_surface().send_close();
            }
        }
    }

    fn fail(&mut self, error: anyhow::Error) {
        if self.exit_error.is_none() {
            error!(error = %format!("{error:#}"), "compositor failure");
            self.exit_error = Some(error);
        }
        self.begin_shutdown("compositor failure");
        self.shutdown.as_mut().unwrap().reason = "compositor failure";
    }

    fn shutdown_finished(&mut self) -> bool {
        self.reap_children();
        let Some(shutdown) = &self.shutdown else {
            return false;
        };
        let remaining_clients = self.client_count();
        if remaining_clients == 0 && self.children.is_empty() {
            return true;
        }
        if Instant::now() >= shutdown.deadline {
            warn!(
                remaining_clients,
                remaining_children = self.children.len(),
                "shutdown grace period expired; disconnecting remaining clients"
            );
            return true;
        }
        // Destroying the last toplevel is insufficient: clients may still need
        // final wl_display.sync round trips before disconnecting their sockets.
        false
    }

    fn viewport(&self) -> Rect {
        self.output
            .as_ref()
            .and_then(|o| self.space.output_geometry(o))
            .map(|g| Rect::new(g.loc.x, g.loc.y, g.size.w, g.size.h))
            .unwrap_or(Rect::new(0, 0, 1280, 800))
    }

    fn layout(&self) -> LayoutKind {
        self.layouts[self.active_workspace as usize - 1]
    }

    fn visible(&self, entry: &ManagedWindow) -> bool {
        entry.workspace == self.active_workspace && entry.participating && !entry.scratchpad_hidden
    }

    fn focusable(&self, entry: &ManagedWindow) -> bool {
        self.visible(entry) && entry.mapped && self.space.element_location(&entry.window).is_some()
    }

    /// Record actual switches from keyboard, IPC, workspace bar, or activation.
    /// Callers arrange and select focus after updating the active workspace.
    fn select_workspace(&mut self, index: u8) -> bool {
        self.created_workspace = None;
        if index == self.active_workspace {
            return false;
        }
        // A second switch must not strand the previous outgoing workspace in
        // Space. The next transition starts from the currently selected one.
        self.cancel_workspace_slide();
        if let Some(entry) = self
            .windows
            .iter_mut()
            .find(|w| Some(w.id) == self.scratchpad)
        {
            entry.scratchpad_hidden = true;
        }
        let from = self.active_workspace;
        self.previous_workspace = Some(self.active_workspace);
        self.active_workspace = index;
        if self.config.layout.workspace_slide
            && !self.overview
            && !self.overview_exiting
            && self.output.is_some()
        {
            self.workspace_slide = Some(workspace_slide::WorkspaceSlide::new(
                from,
                index,
                self.config.layout.workspace_slide_ms,
                Instant::now(),
            ));
            let serial = SERIAL_COUNTER.next_serial();
            let time = self.start_time.elapsed().as_millis() as u32;
            // Keep XKB's physical modifier state intact: Alt+2, Alt+Return
            // must remain two shortcuts if Alt stays held through the slide.
            // Removing keyboard focus isolates clients; only held pointer
            // buttons need synthetic releases before their grab is dropped.
            self.release_held_buttons(serial, time);
            let pointer = self.seat.get_pointer().unwrap();
            pointer.unset_grab(self, serial, time);
            pointer.frame(self);
            let keyboard = self.seat.get_keyboard().unwrap();
            keyboard.unset_grab(self);
            keyboard.set_focus(self, None, serial);
            self.refresh_pointer_focus();
        }
        true
    }

    fn cancel_workspace_slide(&mut self) {
        if let Some(slide) = self.workspace_slide.take() {
            for entry in &self.windows {
                if entry.workspace == slide.from {
                    self.space.unmap_elem(&entry.window);
                }
            }
        }
    }

    fn finish_workspace_slide(&mut self, now: Instant) {
        if self
            .workspace_slide
            .as_ref()
            .is_some_and(|slide| slide.done(now))
        {
            self.cancel_workspace_slide();
            self.arrange();
            self.host_focus(self.host_focused);
            self.refresh_pointer_focus();
        }
    }

    fn add_window(&mut self, surface: ToplevelSurface) {
        let area = self.viewport();
        let width = 800.min(area.width);
        let height = 520.min(area.height);
        let rect = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let window = Window::new_wayland_window(surface);
        self.windows.push(ManagedWindow {
            id: self.next_id,
            window,
            workspace: self.active_workspace,
            scratchpad_hidden: false,
            floating: false,
            fullscreen: false,
            urgent: false,
            mapped: false,
            participating: true,
            rules_applied: false,
            floating_rect: rect,
            rect,
            spring: None,
            fade: None,
            borders: std::array::from_fn(|_| SolidColorBuffer::default()),
            dim_id: Id::new(),
            dim_commit: CommitCounter::default(),
            dim_strength: 0.0,
            decor: effects::WindowDecor::default(),
        });
        self.next_id += 1;
        self.arrange();
        if self.shutdown.is_some() {
            self.windows
                .last()
                .unwrap()
                .window
                .toplevel()
                .unwrap()
                .send_close();
        }
    }

    fn arrange(&mut self) {
        let viewport = self.viewport();
        let layout = self.layout();
        let fullscreen = self
            .windows
            .iter()
            .find(|w| self.visible(w) && w.fullscreen)
            .map(|w| w.id);
        let tiled_count = self
            .windows
            .iter()
            .filter(|w| self.visible(w) && !w.floating)
            .count();
        let tiles = layout::arrange(
            layout,
            layout::tiled_area(
                viewport,
                tiled_count,
                self.config.layout.outer_gap,
                self.config.layout.smart_gaps && layout != LayoutKind::Floating,
            ),
            tiled_count,
            self.config.layout.inner_gap,
            self.config.layout.master_ratio,
        );
        let mut tile = 0;
        for entry in &mut self.windows {
            let visible = entry.workspace == self.active_workspace
                && entry.participating
                && !entry.scratchpad_hidden
                && fullscreen.is_none_or(|id| id == entry.id);
            if !visible {
                entry.spring = None;
                let outgoing = self.workspace_slide.as_ref().is_some_and(|slide| {
                    entry.workspace == slide.from
                        && entry.participating
                        && entry.mapped
                        && !entry.scratchpad_hidden
                });
                if !outgoing {
                    self.space.unmap_elem(&entry.window);
                }
                continue;
            }
            let tiled = !entry.floating && layout != LayoutKind::Floating && fullscreen.is_none();
            let rect = if fullscreen.is_some() {
                viewport
            } else if tiled {
                let rect = tiles[tile];
                tile += 1;
                rect.inset(self.config.decoration.border_width)
            } else {
                entry.floating_rect
            };
            if self.config.layout.spring_resize
                && !self.overview
                && !self.overview_exiting
                && tiled
                && entry.mapped
                && entry.rect != rect
            {
                let now = Instant::now();
                if let Some(spring) = &mut entry.spring {
                    spring.retarget(rect, now, self.config.layout.spring_frequency_hz);
                } else {
                    entry.spring = Some(spring::WindowSpring::new(entry.rect, rect, now));
                }
            } else if !self.overview
                && !self.overview_exiting
                && (!self.config.layout.spring_resize || !tiled)
            {
                entry.spring = None;
            }
            entry.rect = rect;
            entry.window.override_z_index(if tiled { 30 } else { 35 });
            if let Some(surface) = entry.window.toplevel() {
                surface.with_pending_state(|state| {
                    state.size = Some((rect.width, rect.height).into());
                    state.bounds = Some((viewport.width, viewport.height).into());
                    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
                    for edge in [
                        State::TiledLeft,
                        State::TiledRight,
                        State::TiledTop,
                        State::TiledBottom,
                    ] {
                        if tiled {
                            state.states.set(edge);
                        } else {
                            state.states.unset(edge);
                        }
                    }
                    if entry.fullscreen {
                        state.states.set(State::Fullscreen);
                    } else {
                        state.states.unset(State::Fullscreen);
                    }
                });
                if surface.is_initial_configure_sent() {
                    surface.send_pending_configure();
                }
            } else if let Some(x11) = entry.window.x11_surface() {
                let geometry = smithay::utils::Rectangle::new(
                    (rect.x, rect.y).into(),
                    (rect.width, rect.height).into(),
                );
                if let Err(error) = x11.configure(geometry) {
                    warn!(%error, "configuring X11 window");
                }
            }
            self.space
                .map_element(entry.window.clone(), (rect.x, rect.y), false);
        }
        // Keep floating windows above tiled windows, then the focused floating window on top.
        for entry in &self.windows {
            if self.visible(entry) && (entry.floating || layout == LayoutKind::Floating) {
                self.space.raise_element(&entry.window, false);
            }
        }
        if let Some(entry) = self.windows.iter().find(|w| {
            Some(w.id) == self.focused
                && self.visible(w)
                && (w.floating || layout == LayoutKind::Floating || w.fullscreen)
        }) {
            self.space.raise_element(&entry.window, false);
        }
        // Layer surfaces follow the viewport: recompute anchored geometry so
        // bars and wallpaper track resizes. arrange() sends follow-up size
        // configures itself once the initial configure has been acked.
        if let Some(output) = &self.output {
            layer_map_for_output(output).arrange();
        }
        self.refresh_overview_items();
        self.dirty = true;
        self.refresh_pointer_focus();
    }

    fn focus(&mut self, id: Option<u64>) {
        if id.is_some_and(|id| !self.windows.iter().any(|w| w.id == id && self.focusable(w))) {
            return;
        }
        if let Some(id) = id {
            self.workspace_focus[self.active_workspace as usize - 1] = Some(id);
        }
        if let Some(entry) = self.windows.iter_mut().find(|w| Some(w.id) == id) {
            if entry.urgent {
                entry.urgent = false;
                self.dirty = true;
            }
        }
        if self.focused == id {
            return;
        }
        self.focused = id;
        for entry in &self.windows {
            let activated = Some(entry.id) == id && self.session_lock.is_none();
            if let Some(surface) = entry.window.toplevel() {
                entry.window.set_activated(activated);
                if surface.is_initial_configure_sent() {
                    surface.send_pending_configure();
                }
            } else if let Some(x11) = entry.window.x11_surface() {
                if let Err(error) = x11.set_activated(activated) {
                    warn!(%error, "updating X11 window activation");
                }
            }
        }
        let surface = self
            .windows
            .iter()
            .find(|w| Some(w.id) == id)
            .and_then(|w| {
                self.space.raise_element(&w.window, false);
                w.wl_surface()
            });
        let keyboard = self.seat.get_keyboard().unwrap();
        let surface = surface.filter(|_| {
            self.host_focused
                && self.session_lock.is_none()
                && !self.overview
                && !self.overview_exiting
                && self.workspace_slide.is_none()
        });
        if self.session_lock.is_some() {
            self.lock_focus();
        } else {
            keyboard.set_focus(self, surface, SERIAL_COUNTER.next_serial());
        }
        self.dirty = true;
        self.refresh_pointer_focus();
    }

    fn focus_fallback(&mut self) {
        let remembered = self.workspace_focus[self.active_workspace as usize - 1];
        let id = remembered
            .filter(|id| {
                self.windows
                    .iter()
                    .any(|w| w.id == *id && self.focusable(w))
            })
            .or_else(|| {
                self.windows
                    .iter()
                    .rev()
                    .find(|w| self.focusable(w))
                    .map(|w| w.id)
            });
        self.focus(id);
    }

    fn refresh_overview_items(&mut self) {
        self.overview_rects.clear();
        if !self.overview && !self.overview_exiting {
            return;
        }
        let windows: Vec<_> = self
            .windows
            .iter()
            .filter(|w| {
                self.visible(w) && w.mapped && self.space.element_location(&w.window).is_some()
            })
            .collect();
        let targets = overview::thumbnails(
            self.viewport(),
            &windows.iter().map(|window| window.rect).collect::<Vec<_>>(),
        );
        self.overview_rects.extend(
            windows
                .into_iter()
                .zip(targets)
                .map(|(window, rect)| (window.id, rect)),
        );
    }

    fn overview_items(&self) -> &[(u64, Rect)] {
        &self.overview_rects
    }

    fn overview_window_at(&self, pos: Point<f64, Logical>) -> Option<u64> {
        overview::hit(self.overview_items(), pos.x, pos.y)
    }

    fn set_overview(&mut self, active: bool) {
        if self.overview == active {
            return;
        }
        if active && self.workspace_slide.is_some() {
            self.cancel_workspace_slide();
            self.arrange();
        }
        self.overview = active;
        self.overview_exiting = !active;
        self.refresh_overview_items();
        self.overview_dim_commit.increment();
        let now = Instant::now();
        let frequency = self.config.layout.spring_frequency_hz;
        for entry in &mut self.windows {
            if !self.config.layout.overview_animation {
                entry.spring = None;
                continue;
            }
            let thumbnail = self
                .overview_rects
                .iter()
                .find(|(id, _)| *id == entry.id)
                .map(|(_, rect)| *rect);
            let Some(thumbnail) = thumbnail else {
                entry.spring = None;
                continue;
            };
            let target = if active { thumbnail } else { entry.rect };
            if let Some(spring) = &mut entry.spring {
                spring.retarget(target, now, frequency);
            } else {
                let start = if active { entry.rect } else { thumbnail };
                entry.spring = Some(spring::WindowSpring::new(start, target, now));
            }
        }
        if active {
            let serial = SERIAL_COUNTER.next_serial();
            let time = self.start_time.elapsed().as_millis() as u32;
            self.release_held_input(serial, time);
            let pointer = self.seat.get_pointer().unwrap();
            pointer.unset_grab(self, serial, time);
            pointer.frame(self);
            let keyboard = self.seat.get_keyboard().unwrap();
            keyboard.unset_grab(self);
            keyboard.set_focus(self, None, serial);
        }
        self.finish_overview_transition();
        self.dirty = true;
        self.refresh_pointer_focus();
    }

    fn finish_overview_transition(&mut self) {
        if self.overview_exiting && self.windows.iter().all(|entry| entry.spring.is_none()) {
            self.overview_exiting = false;
            self.overview_rects.clear();
            if self.host_focused {
                self.host_focus(true);
            }
            // Remove the dim overlay and restore normal layer stacking.
            self.dirty = true;
        }
    }

    fn host_focus(&mut self, focused: bool) {
        self.host_focused = focused;
        let keyboard = self.seat.get_keyboard().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.start_time.elapsed().as_millis() as u32;
        if focused {
            let surface = self
                .windows
                .iter()
                .find(|w| {
                    Some(w.id) == self.focused && self.space.element_location(&w.window).is_some()
                })
                .and_then(ManagedWindow::wl_surface);
            keyboard.set_focus(
                self,
                surface.filter(|_| {
                    self.session_lock.is_none()
                        && !self.overview
                        && !self.overview_exiting
                        && self.workspace_slide.is_none()
                }),
                serial,
            );
        } else {
            self.release_held_input(serial, time);
            let keyboard = self.seat.get_keyboard().unwrap();
            let pointer = self.seat.get_pointer().unwrap();
            pointer.unset_grab(self, serial, time);
            pointer.frame(self);
            keyboard.unset_grab(self);
            keyboard.set_focus(self, None, serial);
        }
        if focused && self.session_lock.is_some() {
            self.lock_focus();
        }
    }

    /// Release keys and buttons without changing focus. Used on VT switches and
    /// when an input device disappears, where the kernel will not synthesize
    /// releases for us.
    fn release_held_input(&mut self, serial: Serial, time: u32) {
        use smithay::{backend::input::KeyState, input::keyboard::FilterResult};
        let keyboard = self.seat.get_keyboard().unwrap();
        for key in keyboard.pressed_keys() {
            keyboard.input::<(), _>(self, key, KeyState::Released, serial, time, |data, _, _| {
                if data.suppressed_keys.remove(&key.raw()) {
                    FilterResult::Intercept(())
                } else {
                    FilterResult::Forward
                }
            });
        }
        self.suppressed_keys.clear();
        self.release_held_buttons(serial, time);
    }

    fn release_held_buttons(&mut self, serial: Serial, time: u32) {
        use smithay::{backend::input::ButtonState, input::pointer::ButtonEvent};
        let pointer = self.seat.get_pointer().unwrap();
        for button in std::mem::take(&mut self.pressed_buttons) {
            pointer.button(
                self,
                &ButtonEvent {
                    button,
                    state: ButtonState::Released,
                    serial,
                    time,
                },
            );
        }
        pointer.frame(self);
    }

    fn release_input(&mut self) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.start_time.elapsed().as_millis() as u32;
        self.release_held_input(serial, time);
    }

    fn cycle_focus(&mut self, backwards: bool) {
        if self.overview_exiting {
            return;
        }
        let next = if self.overview {
            overview::cycle(self.overview_items(), self.focused, backwards)
        } else {
            navigation::cycle(
                self.windows
                    .iter()
                    .filter(|w| self.focusable(w))
                    .map(|w| w.id),
                self.focused,
                backwards,
            )
        };
        if let Some(id) = next {
            self.focus(Some(id));
        }
    }

    fn remove_window(&mut self, surface: &WlSurface) {
        if let Some(index) = self.windows.iter().position(|w| {
            w.window
                .toplevel()
                .is_some_and(|toplevel| toplevel.wl_surface() == surface)
        }) {
            let entry = self.windows.remove(index);
            if self.scratchpad == Some(entry.id) {
                self.scratchpad = None;
            }
            for remembered in &mut self.workspace_focus {
                if *remembered == Some(entry.id) {
                    *remembered = None;
                }
            }
            self.space.unmap_elem(&entry.window);
            self.arrange();
            if self.focused == Some(entry.id) {
                self.focus_fallback();
            }
            // Urgency and per-workspace membership reported to clients changed.
            self.ext_workspace_notify();
        }
    }

    fn metadata(window: &Window) -> (String, String) {
        let Some(toplevel) = window.toplevel() else {
            return window.x11_surface().map_or_else(
                || (String::new(), String::new()),
                |x11| (x11.class(), x11.title()),
            );
        };
        with_states(toplevel.wl_surface(), |states| {
            let data = states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap();
            (
                data.app_id.clone().unwrap_or_default(),
                data.title.clone().unwrap_or_default(),
            )
        })
    }

    fn apply_rules(&mut self, index: usize) {
        let (app_id, title) = Self::metadata(&self.windows[index].window);
        let area = self.viewport();
        let entry = &mut self.windows[index];
        for rule in &self.config.rules {
            if rule.app_id.as_ref().is_some_and(|id| *id != app_id)
                || rule.title.as_ref().is_some_and(|t| !title.contains(t))
            {
                continue;
            }
            if let Some(floating) = rule.floating {
                entry.floating = floating;
            }
            if let Some(workspace) = rule.workspace {
                entry.workspace = workspace;
            }
            if let Some([width, height]) = rule.size {
                entry.floating_rect = Rect::new(
                    area.x + (area.width - width) / 2,
                    area.y + (area.height - height) / 2,
                    width,
                    height,
                );
            }
        }
        entry.rules_applied = true;
    }

    fn start_autostart(&mut self) {
        // Startup-only copy allows spawn() to track children without borrowing
        // the live config. Reload never calls this function.
        for argv in self.config.general.autostart.clone() {
            if let Err(error) = self.spawn(&argv) {
                warn!(error = %format!("{error:#}"), "autostart command failed");
            }
        }
    }

    fn spawn(&mut self, argv: &[String]) -> Result<()> {
        Action::Spawn {
            argv: argv.to_vec(),
        }
        .validate(self.config.general.workspaces)?;
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]);
        if let Some(display) = &self.x_display {
            command.env("DISPLAY", display);
        } else {
            command.env_remove("DISPLAY");
        }
        let child = command
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("WISP_SOCKET", &self.ipc_path)
            .env("XDG_CURRENT_DESKTOP", "Wisp")
            .env("XDG_SESSION_TYPE", "wayland")
            .env_remove("WAYLAND_SOCKET")
            .stdin(Stdio::null())
            .spawn()
            .with_context(|| format!("starting {}", argv[0]))?;
        self.children.push(child);
        Ok(())
    }

    fn reap_children(&mut self) {
        self.children.retain_mut(|child| match child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(error) => {
                warn!(%error, "reaping child");
                false
            }
        });
    }

    fn reload(&mut self) -> Result<()> {
        // Validate everything (including XKB) before replacing live configuration.
        let mut config = Config::load(&self.config_path, true)?;
        // These globals/processes are created at startup, not on reload.
        // Preserve the actual running state instead of suggesting that
        // reloading can advertise or remove the lock/X11 protocol.
        if config.general.session_lock_protocol != self.config.general.session_lock_protocol {
            warn!("session_lock_protocol requires a restart; keeping the running value");
            config.general.session_lock_protocol = self.config.general.session_lock_protocol;
        }
        if config.general.xwayland != self.config.general.xwayland {
            warn!("xwayland requires a restart; keeping the running value");
            config.general.xwayland = self.config.general.xwayland;
        }
        let bindings = config.compile_bindings()?;
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_xkb_config(
            self,
            XkbConfig {
                layout: &config.input.xkb_layout,
                options: config.input.xkb_options.clone(),
                ..Default::default()
            },
        )?;
        keyboard.change_repeat_info(config.input.repeat_rate, config.input.repeat_delay);
        self.cancel_workspace_slide();
        self.layouts
            .resize(config.general.workspaces as usize, config.layout.default);
        self.active_workspace = self.active_workspace.min(config.general.workspaces);
        self.previous_workspace = self
            .previous_workspace
            .filter(|index| *index <= config.general.workspaces && *index != self.active_workspace);
        self.workspace_focus
            .resize(config.general.workspaces as usize, None);
        for entry in &mut self.windows {
            entry.workspace = entry.workspace.min(config.general.workspaces);
        }
        // Materialized workspaces must stay in the configured range: workspaces
        // dropped here are removed from clients with a `removed` event.
        self.materialized_workspaces
            .retain(|index| *index <= config.general.workspaces);
        self.created_workspace = self
            .created_workspace
            .filter(|index| *index <= config.general.workspaces);
        self.bindings = bindings;
        self.config = config;
        self.sync_nested_output_refresh();
        self.arrange();
        // Turning off overview motion during a transition must also finish
        // input isolation, rather than waiting for now-disabled springs.
        if !self.config.layout.overview_animation && (self.overview || self.overview_exiting) {
            for entry in &mut self.windows {
                entry.spring = None;
            }
            self.finish_overview_transition();
        }
        self.focus_fallback();
        self.ext_workspace_notify();
        info!("configuration reloaded");
        Ok(())
    }

    /// Nested clients see our configured cadence in wl_output; on a reload it
    /// must change with the scheduler. Direct mode must keep the real DRM
    /// connector refresh, which is unrelated to the rendering cap.
    fn sync_nested_output_refresh(&self) {
        if self.backend_name != "nested-winit-gles" {
            return;
        }
        if let Some((output, mode)) = self
            .output
            .as_ref()
            .and_then(|output| output.current_mode().map(|mode| (output, mode)))
        {
            let refresh = self.config.render.max_fps as i32 * 1000;
            if mode.refresh != refresh {
                output.change_current_state(
                    Some(OutputMode {
                        size: mode.size,
                        refresh,
                    }),
                    None,
                    None,
                    None,
                );
            }
        }
    }

    pub(crate) fn dispatch(&mut self, action: Action) -> Result<()> {
        ensure!(self.session_lock.is_none(), "session is locked");
        ensure!(
            self.shutdown.is_none() || matches!(action, Action::Quit {}),
            "compositor is shutting down"
        );
        action.validate(self.config.general.workspaces)?;
        match action {
            Action::Terminal {} => self.spawn(&self.config.general.terminal.clone())?,
            Action::Spawn { argv } => self.spawn(&argv)?,
            Action::FocusNext {} => self.cycle_focus(false),
            Action::FocusPrev {} => self.cycle_focus(true),
            Action::FocusDirection { direction } => self.focus_direction(direction),
            Action::SwapDirection { direction } => self.swap_direction(direction),
            Action::SwapNext {} => {
                if let Some(index) = self.windows.iter().position(|w| Some(w.id) == self.focused) {
                    let ids: Vec<_> = self
                        .windows
                        .iter()
                        .enumerate()
                        .filter(|(_, w)| self.visible(w) && !w.floating)
                        .map(|(i, _)| i)
                        .collect();
                    if let Some(position) = ids.iter().position(|i| *i == index) {
                        self.windows.swap(index, ids[(position + 1) % ids.len()]);
                        self.arrange();
                    }
                }
            }
            Action::ToggleFloating {} => {
                if let Some(entry) = self.windows.iter_mut().find(|w| Some(w.id) == self.focused) {
                    entry.floating = !entry.floating;
                    self.arrange();
                }
            }
            Action::ToggleFullscreen {} => {
                if let Some(id) = self.focused {
                    let fullscreen = !self
                        .windows
                        .iter()
                        .find(|w| w.id == id)
                        .is_some_and(|w| w.fullscreen);
                    for entry in &mut self.windows {
                        if entry.workspace == self.active_workspace {
                            entry.fullscreen = entry.id == id && fullscreen;
                        }
                    }
                    self.arrange();
                }
            }
            Action::ToggleOverview {} => self.set_overview(!self.overview),
            Action::ToggleBlur {} => {
                self.config.decoration.blur.enabled = !self.config.decoration.blur.enabled;
                self.dirty = true;
            }
            Action::ToggleShadows {} => {
                self.config.decoration.shadow.enabled = !self.config.decoration.shadow.enabled;
                self.dirty = true;
            }
            Action::SendToScratchpad {} => {
                let id = self.focused.context("no focused window for scratchpad")?;
                ensure!(
                    self.scratchpad.is_none() || self.scratchpad == Some(id),
                    "scratchpad already has a window"
                );
                let entry = self
                    .windows
                    .iter_mut()
                    .find(|w| w.id == id)
                    .context("focused window disappeared")?;
                entry.scratchpad_hidden = true;
                entry.floating = true;
                self.scratchpad = Some(id);
                self.arrange();
                self.focus_fallback();
                self.ext_workspace_notify();
            }
            Action::ToggleScratchpad {} => {
                if let Some(id) = self.scratchpad {
                    if let Some(entry) = self.windows.iter_mut().find(|w| w.id == id) {
                        entry.scratchpad_hidden = !entry.scratchpad_hidden;
                        if !entry.scratchpad_hidden {
                            entry.workspace = self.active_workspace;
                            entry.floating = true;
                        }
                        let showing = !entry.scratchpad_hidden;
                        self.arrange();
                        if showing {
                            self.focus(Some(id));
                        } else {
                            self.focus_fallback();
                        }
                        self.ext_workspace_notify();
                    } else {
                        self.scratchpad = None;
                    }
                }
            }
            Action::Close {} => {
                if let Some(entry) = self.windows.iter().find(|w| Some(w.id) == self.focused) {
                    entry.send_close();
                }
            }
            Action::Workspace { index } => {
                if self.select_workspace(index) {
                    self.arrange();
                    self.focus_fallback();
                }
                self.ext_workspace_notify();
            }
            Action::WorkspaceNext {} => {
                if self.active_workspace < self.config.general.workspaces {
                    self.dispatch(Action::Workspace {
                        index: self.active_workspace + 1,
                    })?;
                }
            }
            Action::WorkspacePrev {} => {
                if self.active_workspace > 1 {
                    self.dispatch(Action::Workspace {
                        index: self.active_workspace - 1,
                    })?;
                }
            }
            Action::WorkspaceBackAndForth {} => {
                if let Some(index) = self.previous_workspace {
                    self.dispatch(Action::Workspace { index })?;
                }
            }
            Action::MoveToWorkspace { index } => {
                if let Some(entry) = self.windows.iter_mut().find(|w| Some(w.id) == self.focused) {
                    entry.workspace = index;
                    if self.scratchpad == Some(entry.id) {
                        self.scratchpad = None;
                        entry.scratchpad_hidden = false;
                    }
                    self.arrange();
                    self.focus_fallback();
                }
                self.ext_workspace_notify();
            }
            Action::Layout { kind } => {
                self.layouts[self.active_workspace as usize - 1] = kind;
                self.arrange();
            }
            Action::MasterRatio { ratio } => {
                self.config.layout.master_ratio = ratio;
                self.arrange();
            }
            Action::Reload {} => self.reload()?,
            Action::Quit {} => self.begin_shutdown("quit action requested"),
        }
        Ok(())
    }

    fn request(&mut self, request: Request) -> Response {
        match request {
            Request::Status {} => Response::success(json!({
                "name": "wisp", "version": env!("CARGO_PKG_VERSION"), "backend": self.backend_name,
                "backend_details": self.backend_details,
                "pid": std::process::id(),
                "wayland_display": self.socket_name.to_string_lossy(), "ipc_socket": self.ipc_path,
                "workspace": self.active_workspace, "layout": self.layout(), "focused": self.focused,
                "host_focused": self.host_focused,
                "scratchpad": self.scratchpad,
                "previous_workspace": self.previous_workspace,
                "overview": self.overview, "overview_exiting": self.overview_exiting,
                "effects": {
                    "blur_enabled": self.config.decoration.blur.enabled,
                    "shadows_enabled": self.config.decoration.shadow.enabled,
                    "overview_animation": self.config.layout.overview_animation,
                },
                "viewport": self.viewport(), "max_fps": self.config.render.max_fps,
                "output_refresh_mhz": self.output.as_ref().and_then(Output::current_mode).map(|mode| mode.refresh),
                "frames": self.stats,
                "uptime_seconds": self.start_time.elapsed().as_secs(),
                "clients": self.client_count(), "layers": self.mapped_layers(), "shutting_down": self.shutdown.is_some(),
            })),
            Request::Windows {} => Response::success(self.windows.iter().map(|w| {
                let (app_id, title) = Self::metadata(&w.window);
                let (icon_name, icon_buffers) = Self::icon_metadata(&w.window);
                json!({ "id": w.id, "app_id": app_id, "title": title, "workspace": w.workspace,
                    "floating": w.floating, "fullscreen": w.fullscreen, "focused": Some(w.id) == self.focused,
                    "urgent": w.urgent, "icon_name": icon_name, "icon_buffers": icon_buffers,
                    "mapped": w.mapped, "scratchpad": self.scratchpad == Some(w.id),
                    "scratchpad_hidden": w.scratchpad_hidden, "geometry": w.rect })
            }).collect::<Vec<_>>()),
            Request::Workspaces {} => Response::success(self.materialized_workspaces.iter().copied().map(|index| json!({
                "index": index, "active": index == self.active_workspace, "layout": self.layouts[index as usize - 1],
                "windows": self.windows.iter().filter(|w| w.workspace == index && w.mapped && !w.scratchpad_hidden).count(),
                "urgent": self.windows.iter().any(|w| w.workspace == index && w.urgent && !w.scratchpad_hidden),
            })).collect::<Vec<_>>()),
            Request::Dispatch { action } => match self.dispatch(action) { Ok(()) => Response::success(json!({})), Err(error) => Response::failure(format!("{error:#}")) },
        }
    }

    pub(crate) fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        if self.session_lock.is_some() {
            return self.lock_surface_under(pos);
        }
        if self.overview || self.overview_exiting || self.workspace_slide.is_some() {
            return None;
        }
        // Panel and overlay surfaces sit above windows and own the pointer
        // wherever they are mapped.
        if let Some(layer) = self.layer_surface_under(pos, &[Layer::Overlay, Layer::Top]) {
            return Some(layer);
        }
        let window = self
            .space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, (p + location).to_f64()))
            });
        if window.is_some() {
            return window;
        }
        // Wallpaper and dock surfaces stay beneath windows.
        self.layer_surface_under(pos, &[Layer::Bottom, Layer::Background])
    }

    /// Resolve a mapped layer surface at `pos` among the listed layers, in
    /// given order. Layers are checked in the order supplied so callers can
    /// also list sub-window kinds; smithay's map already picks the topmost
    /// surface of each kind.
    fn layer_surface_under(
        &self,
        pos: Point<f64, Logical>,
        kinds: &[Layer],
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let output = self.output.as_ref()?;
        let map = layer_map_for_output(output);
        for kind in kinds {
            let Some(layer) = map.layer_under(*kind, pos) else {
                continue;
            };
            let Some(geometry) = map.layer_geometry(layer) else {
                continue;
            };
            if let Some((surface, local)) =
                layer.surface_under(pos - geometry.loc.to_f64(), WindowSurfaceType::ALL)
            {
                return Some((surface, (local + geometry.loc).to_f64()));
            }
        }
        None
    }

    fn mapped_layers(&self) -> usize {
        self.output
            .as_ref()
            .map(|output| layer_map_for_output(output).len())
            .unwrap_or(0)
    }

    fn window_under(&self, pos: Point<f64, Logical>) -> Option<u64> {
        if self.workspace_slide.is_some() {
            return None;
        }
        let (window, _) = self.space.element_under(pos)?;
        self.windows
            .iter()
            .find(|w| &w.window == window)
            .map(|w| w.id)
    }

    fn refresh_pointer_focus(&mut self) {
        let pointer = self.seat.get_pointer().unwrap();
        if pointer.is_grabbed() {
            return;
        }
        let location = pointer.current_location();
        let under = self.surface_under(location);
        pointer.motion(
            self,
            under,
            &smithay::input::pointer::MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: self.start_time.elapsed().as_millis() as u32,
            },
        );
        pointer.frame(self);
    }
}

#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::output::{PhysicalProperties, Subpixel};
    use std::os::unix::net::UnixStream;

    #[test]
    fn slide_starts_only_when_enabled_and_cleans_up_on_completion_or_replacement() {
        let (_display, mut state) = test_state();
        state.dispatch(Action::Workspace { index: 2 }).unwrap();
        assert!(state.workspace_slide.is_none());
        let output = Output::new(
            "WISP-TEST".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Wisp".into(),
                model: "Test".into(),
            },
        );
        output.change_current_state(
            Some(OutputMode {
                size: (800, 600).into(),
                refresh: 60_000,
            }),
            None,
            None,
            None,
        );
        state.space.map_output(&output, (0, 0));
        state.output = Some(output);
        state.config.layout.workspace_slide = true;
        state.dispatch(Action::Workspace { index: 3 }).unwrap();
        assert_eq!(state.workspace_slide.as_ref().unwrap().from, 2);
        assert!(state.surface_under((10.0, 10.0).into()).is_none());
        state.dispatch(Action::Workspace { index: 4 }).unwrap();
        assert_eq!(state.workspace_slide.as_ref().unwrap().from, 3);
        state.finish_workspace_slide(Instant::now() + Duration::from_secs(1));
        assert!(state.workspace_slide.is_none());
        assert!(state.dirty);
        state.dispatch(Action::SendToScratchpad {}).unwrap_err();
        state.dispatch(Action::ToggleScratchpad {}).unwrap();
    }

    #[test]
    fn stdout_snapshot_is_stable_without_frame_counters() {
        let (_display, mut state) = test_state();
        let initial = state.stdout_snapshot();
        state.stats.rendered += 1;
        assert_eq!(initial, state.stdout_snapshot());
        state.dispatch(Action::Workspace { index: 2 }).unwrap();
        assert_ne!(initial, state.stdout_snapshot());
    }

    #[test]
    fn overview_toggles_without_windows_or_idle_animation() {
        let (_display, mut state) = test_state();
        assert!(!state.overview);
        state.dispatch(Action::ToggleOverview {}).unwrap();
        assert_eq!(
            state.request(Request::Status {}).data.unwrap()["overview"],
            true
        );
        assert!(state.overview_items().is_empty());
        assert!(state.dirty);
        state.dispatch(Action::ToggleOverview {}).unwrap();
        assert!(!state.overview);
        assert!(!state.overview_exiting);
    }

    #[test]
    fn relative_workspace_actions_stop_at_boundaries_without_wrapping() {
        let (_display, mut state) = test_state();
        state.dispatch(Action::WorkspacePrev {}).unwrap();
        assert_eq!(state.active_workspace, 1);
        state.dispatch(Action::WorkspaceNext {}).unwrap();
        assert_eq!(state.active_workspace, 2);
        state.dispatch(Action::WorkspacePrev {}).unwrap();
        assert_eq!(state.active_workspace, 1);
        state
            .dispatch(Action::Workspace { index: u8::MAX })
            .unwrap();
        state.dispatch(Action::WorkspaceNext {}).unwrap();
        assert_eq!(state.active_workspace, u8::MAX);
        state.dispatch(Action::WorkspacePrev {}).unwrap();
        assert_eq!(state.active_workspace, u8::MAX - 1);

        state.dispatch(Action::Workspace { index: 1 }).unwrap();
        state.config.general.workspaces = 1;
        state.dispatch(Action::WorkspaceNext {}).unwrap();
        state.dispatch(Action::WorkspacePrev {}).unwrap();
        assert_eq!(state.active_workspace, 1);
    }

    #[test]
    fn workspace_history_tracks_real_switches_and_is_pruned_by_reload() {
        let (_display, mut state) = test_state();
        state.dispatch(Action::WorkspaceBackAndForth {}).unwrap();
        assert_eq!(state.active_workspace, 1);
        assert_eq!(state.previous_workspace, None);
        state.dispatch(Action::Workspace { index: 4 }).unwrap();
        state.dispatch(Action::Workspace { index: 4 }).unwrap();
        assert_eq!(state.previous_workspace, Some(1));
        state.dispatch(Action::WorkspaceBackAndForth {}).unwrap();
        assert_eq!(state.active_workspace, 1);
        assert_eq!(state.previous_workspace, Some(4));

        let config = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            config.path(),
            "[general]\nworkspaces = 2\nautostart = [[\"/bin/true\"]]\n",
        )
        .unwrap();
        state.config_path = config.path().to_path_buf();
        state.reload().unwrap();
        assert_eq!(state.previous_workspace, None);
        assert_eq!(state.workspace_focus.len(), 2);
        assert!(
            state.children.is_empty(),
            "reload must not launch startup commands"
        );
        state.dispatch(Action::WorkspaceBackAndForth {}).unwrap();
        assert_eq!(state.active_workspace, 1);
    }

    #[test]
    fn autostart_continues_after_missing_program_and_sets_only_child_environment() {
        let (_display, mut state) = test_state();
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("environment");
        let before = (
            std::env::var_os("WAYLAND_DISPLAY"),
            std::env::var_os("WISP_SOCKET"),
        );
        state.config.general.autostart = vec![
            vec![
                directory
                    .path()
                    .join("missing-program")
                    .to_str()
                    .unwrap()
                    .to_owned(),
            ],
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf '%s\\n' \"$WAYLAND_DISPLAY\" \"$WISP_SOCKET\" > \"$1\"".into(),
                "wisp-autostart-test".into(),
                output.to_str().unwrap().to_owned(),
            ],
        ];
        state.start_autostart();
        assert_eq!(state.children.len(), 1);
        assert!(state.children[0].wait().unwrap().success());
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            "wayland-test\nunused.sock\n"
        );
        assert_eq!(
            (
                std::env::var_os("WAYLAND_DISPLAY"),
                std::env::var_os("WISP_SOCKET")
            ),
            before
        );
    }

    #[test]
    fn nested_reload_syncs_advertised_refresh_but_direct_does_not() {
        let (_display, mut state) = test_state();
        let output = Output::new(
            "WISP-TEST".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Wisp".into(),
                model: "Test".into(),
            },
        );
        output.change_current_state(
            Some(OutputMode {
                size: (800, 600).into(),
                refresh: 144_000,
            }),
            None,
            None,
            Some((0, 0).into()),
        );
        state.output = Some(output.clone());
        state.backend_name = "nested-winit-gles";
        state.sync_nested_output_refresh();
        assert_eq!(output.current_mode().unwrap().refresh, 60_000);
        state.backend_name = "direct-drm-gles";
        state.config.render.max_fps = 90;
        state.sync_nested_output_refresh();
        assert_eq!(output.current_mode().unwrap().refresh, 60_000);
    }

    pub(super) fn test_state() -> (Display<Wisp>, Wisp) {
        let display = Display::new().unwrap();
        let event_loop = EventLoop::<LoopData>::try_new().unwrap();
        let state = Wisp::new(
            Config::default(),
            PathBuf::from("unused.toml"),
            display.handle(),
            OsString::from("wayland-test"),
            PathBuf::from("unused.sock"),
            event_loop.get_signal(),
            "wisp-test",
        )
        .unwrap();
        (display, state)
    }

    #[test]
    fn ipc_workspaces_follow_dynamic_frontier_without_a_bar_client() {
        let (_display, mut state) = test_state();
        assert_eq!(state.materialized_workspaces, [1, 2]);
        assert_eq!(
            state
                .request(Request::Workspaces {})
                .data
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        state.dispatch(Action::Workspace { index: 10 }).unwrap();
        assert_eq!(
            state
                .request(Request::Workspaces {})
                .data
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            11
        );
        state.dispatch(Action::Workspace { index: 1 }).unwrap();
        assert_eq!(
            state
                .request(Request::Workspaces {})
                .data
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn status_exposes_cumulative_render_phase_timings() {
        let (_display, mut state) = test_state();
        let response = state.request(Request::Status {});
        let frames = &response.data.unwrap()["frames"];
        for phase in [
            "scene_wall_us",
            "bind_wall_us",
            "compose_wall_us",
            "present_wall_us",
        ] {
            assert_eq!(frames[phase], 0, "missing phase {phase}");
        }
    }

    #[test]
    fn shutdown_waits_for_clients_even_without_windows() {
        let (mut display, mut state) = test_state();
        let (server, client) = UnixStream::pair().unwrap();
        state
            .display_handle
            .insert_client(server, Arc::new(ClientState::default()))
            .unwrap();
        assert_eq!(state.client_count(), 1);
        assert!(state.windows.is_empty());
        state.begin_shutdown("test quit");
        assert!(!state.shutdown_finished());
        drop(client);
        display.dispatch_clients(&mut state).unwrap();
        assert_eq!(state.client_count(), 0);
        assert!(state.shutdown_finished());
    }

    #[test]
    fn shutdown_deadline_bounds_unresponsive_clients() {
        let (_display, mut state) = test_state();
        let (server, _client) = UnixStream::pair().unwrap();
        state
            .display_handle
            .insert_client(server, Arc::new(ClientState::default()))
            .unwrap();
        state.begin_shutdown("test quit");
        state.shutdown.as_mut().unwrap().deadline = Instant::now();
        assert!(state.shutdown_finished());
        assert_eq!(state.client_count(), 1);
    }

    #[test]
    fn repeated_quit_preserves_deadline_and_rejects_other_actions() {
        let (_display, mut state) = test_state();
        state.begin_shutdown("first quit");
        let deadline = state.shutdown.as_ref().unwrap().deadline;
        state.dispatch(Action::Quit {}).unwrap();
        assert_eq!(state.shutdown.as_ref().unwrap().deadline, deadline);
        assert_eq!(state.shutdown.as_ref().unwrap().reason, "first quit");
        let error = state.dispatch(Action::Terminal {}).unwrap_err();
        assert_eq!(error.to_string(), "compositor is shutting down");
        assert!(state.children.is_empty());
    }

    #[test]
    fn runtime_failure_preserves_first_error_even_during_clean_quit() {
        let (_display, mut state) = test_state();
        state.begin_shutdown("test quit");
        let deadline = state.shutdown.as_ref().unwrap().deadline;
        state.fail(anyhow::anyhow!("renderer failed"));
        state.fail(anyhow::anyhow!("another failure"));
        assert_eq!(
            state.shutdown.as_ref().unwrap().reason,
            "compositor failure"
        );
        assert_eq!(state.shutdown.as_ref().unwrap().deadline, deadline);
        assert_eq!(state.exit_error.unwrap().to_string(), "renderer failed");
    }
}
