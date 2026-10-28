//! Real protocol round trips over private socket pairs; no host display/input.
use std::{
    os::{fd::AsFd, unix::net::UnixStream},
    path::Path,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use smithay::{
    backend::input::KeyState,
    desktop::layer_map_for_output,
    input::{keyboard::FilterResult, pointer::MotionEvent},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::wayland_server::{Client, protocol::wl_surface::WlSurface},
    utils::{SERIAL_COUNTER, Transform},
    wayland::{compositor::with_states, viewporter::ViewportCachedState},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
    event_created_child,
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_subcompositor, wl_subsurface, wl_surface,
    },
};
use wayland_protocols::{
    ext::session_lock::v1::client::{
        ext_session_lock_manager_v1, ext_session_lock_surface_v1, ext_session_lock_v1,
    },
    ext::workspace::v1::client::{
        ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
        ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
        ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
    },
    wp::{
        cursor_shape::v1::client::{wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1},
        fractional_scale::v1::client::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1},
        linux_dmabuf::zv1::client::{
            zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1, zwp_linux_dmabuf_v1,
        },
        text_input::zv3::client::{zwp_text_input_manager_v3, zwp_text_input_v3},
        viewporter::client::{wp_viewport, wp_viewporter},
    },
    xdg::{
        activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1},
        shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
        toplevel_icon::v1::client::{xdg_toplevel_icon_manager_v1, xdg_toplevel_icon_v1},
    },
};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_keyboard_grab_v2, zwp_input_method_manager_v2, zwp_input_method_v2,
    zwp_input_popup_surface_v2,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use super::super::{ClientState, Wisp, tests::test_state};
use crate::protocol::Action;

type Job = Box<dyn FnOnce(&mut Wisp, &Client) + Send>;
struct Control(mpsc::Sender<Job>);

impl Control {
    fn call<T: Send + 'static>(
        &self,
        callback: impl FnOnce(&mut Wisp, &Client) -> T + Send + 'static,
    ) -> T {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.0
            .send(Box::new(move |state, client| {
                let _ = sender.send(callback(state, client));
            }))
            .unwrap();
        receiver.recv_timeout(Duration::from_secs(5)).unwrap()
    }
}

fn roundtrip_test(test: impl FnOnce(&mut TestClient, &Control) + Send + 'static) {
    let (mut display, mut state) = test_state();
    run_roundtrip(&mut display, &mut state, test);
}

fn run_roundtrip(
    display: &mut smithay::reexports::wayland_server::Display<Wisp>,
    state: &mut Wisp,
    test: impl FnOnce(&mut TestClient, &Control) + Send + 'static,
) {
    let (server, peer) = UnixStream::pair().unwrap();
    let client = state
        .display_handle
        .insert_client(server, Arc::new(ClientState::default()))
        .unwrap();
    let (sender, receiver) = mpsc::channel::<Job>();
    let worker = thread::spawn(move || test(&mut TestClient::new(peer), &Control(sender)));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !worker.is_finished() {
        display.dispatch_clients(state).unwrap();
        while let Ok(job) = receiver.try_recv() {
            job(state, &client);
        }
        state.display_handle.flush_clients().unwrap();
        assert!(Instant::now() < deadline, "protocol round trip timed out");
        thread::sleep(Duration::from_millis(1));
    }
    worker.join().unwrap();
}

#[test]
fn dmabuf_v4_advertises_feedback_only_when_explicitly_initialized_and_shm_remains() {
    use smithay::{
        backend::allocator::{Format, Fourcc, Modifier},
        wayland::dmabuf::DmabufFeedbackBuilder,
    };

    roundtrip_test(|client, _| {
        assert!(
            !client
                .events
                .globals
                .iter()
                .any(|(_, interface, _)| { interface == "zwp_linux_dmabuf_v1" })
        );
        let _shm = client.buffer(4, 4);
    });

    let (mut display, mut state) = super::super::tests::test_state();
    let feedback = DmabufFeedbackBuilder::new(
        0,
        [Format {
            code: Fourcc::Argb8888,
            modifier: Modifier::Linear,
        }],
    )
    .build()
    .unwrap();
    state
        .dmabuf_state
        .create_global_with_default_feedback::<Wisp>(&state.display_handle, &feedback);
    run_roundtrip(&mut display, &mut state, |client, _| {
        assert!(client.events.globals.iter().any(|(_, interface, version)| {
            interface == "zwp_linux_dmabuf_v1" && *version >= 4
        }));
        let dmabuf = client.bind::<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>(4);
        let _feedback = dmabuf.get_default_feedback(&client.queue.handle(), ());
        let surface = client.surface();
        let _surface_feedback = dmabuf.get_surface_feedback(&surface, &client.queue.handle(), ());
        client.roundtrip();
        assert_eq!(client.events.dmabuf_feedback_done, 2);
        let _legacy = client.bind::<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>(3);
        client.roundtrip();
        assert_eq!(client.events.dmabuf_modifiers, 1);
        // A plain file descriptor is deliberately not a GPU DMA-BUF. Because
        // this test server has no renderer, even advertised metadata must be
        // rejected nonfatally via `failed` rather than creating a wl_buffer.
        let file = if Path::new("/tmp/opencode").is_dir() {
            tempfile::tempfile_in("/tmp/opencode").unwrap()
        } else {
            tempfile::tempfile().unwrap()
        };
        file.set_len(16).unwrap();
        let params = dmabuf.create_params(&client.queue.handle(), ());
        params.add(file.as_fd(), 0, 0, 8, 0, 0);
        params.create(
            2,
            2,
            Fourcc::Argb8888 as u32,
            zwp_linux_buffer_params_v1::Flags::empty(),
        );
        client.roundtrip();
        assert!(client.events.dmabuf_failed);
        params.destroy();
        let _shm = client.buffer(4, 4);
    });
}

struct TestClient {
    connection: Connection,
    registry: wl_registry::WlRegistry,
    queue: EventQueue<Events>,
    events: Events,
}

impl TestClient {
    fn new(peer: UnixStream) -> Self {
        let connection = Connection::from_socket(peer).unwrap();
        let queue = connection.new_event_queue();
        let registry = connection.display().get_registry(&queue.handle(), ());
        let mut client = Self {
            connection,
            registry,
            queue,
            events: Events::default(),
        };
        client.roundtrip();
        client
    }

    fn roundtrip(&mut self) {
        self.queue.roundtrip(&mut self.events).unwrap();
    }

    fn expect_protocol_error(&mut self, code: u32) {
        assert!(self.queue.roundtrip(&mut self.events).is_err());
        let error = self
            .connection
            .protocol_error()
            .expect("expected a protocol error, not an I/O failure");
        // The client may already have destroyed the offending object in the
        // same batch, in which case libwayland cannot recover its interface.
        assert_eq!(error.code, code);
    }

    fn bind<I: Proxy + 'static>(&self, version: u32) -> I
    where
        Events: Dispatch<I, ()>,
    {
        let (name, _, available) = self
            .events
            .globals
            .iter()
            .find(|(_, interface, _)| interface == I::interface().name)
            .unwrap_or_else(|| panic!("missing global: {}", I::interface().name));
        assert!(*available >= version);
        self.registry.bind(*name, version, &self.queue.handle(), ())
    }

    fn surface(&self) -> wl_surface::WlSurface {
        self.bind::<wl_compositor::WlCompositor>(4)
            .create_surface(&self.queue.handle(), ())
    }

    fn buffer(&self, width: i32, height: i32) -> wl_buffer::WlBuffer {
        let file = if Path::new("/tmp/opencode").is_dir() {
            tempfile::tempfile_in("/tmp/opencode").unwrap()
        } else {
            tempfile::tempfile().unwrap()
        };
        let size = width * height * 4;
        file.set_len(size as u64).unwrap();
        let pool =
            self.bind::<wl_shm::WlShm>(1)
                .create_pool(file.as_fd(), size, &self.queue.handle(), ());
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            width * 4,
            wl_shm::Format::Argb8888,
            &self.queue.handle(),
            (),
        );
        pool.destroy();
        buffer
    }

    fn window(&mut self) -> (wl_surface::WlSurface, xdg_toplevel::XdgToplevel) {
        let surface = self.surface();
        let xdg = self.bind::<xdg_wm_base::XdgWmBase>(6).get_xdg_surface(
            &surface,
            &self.queue.handle(),
            (),
        );
        let toplevel = xdg.get_toplevel(&self.queue.handle(), ());
        surface.commit();
        self.roundtrip();
        let buffer = self.buffer(64, 32);
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        self.roundtrip();
        (surface, toplevel)
    }

    fn activation_token(
        &mut self,
        activation: &xdg_activation_v1::XdgActivationV1,
        surface: &wl_surface::WlSurface,
        serial: Option<(u32, &wl_seat::WlSeat)>,
    ) -> String {
        let token = activation.get_activation_token(&self.queue.handle(), ());
        token.set_surface(surface);
        if let Some((serial, seat)) = serial {
            token.set_serial(serial, seat);
        }
        token.commit();
        self.roundtrip();
        token.destroy();
        self.events.tokens.last().unwrap().clone()
    }
}

#[derive(Default)]
struct Events {
    globals: Vec<(u32, String, u32)>,
    scales: Vec<u32>,
    icon_done: bool,
    keyboard_enters: Vec<u32>,
    pointer_enter: Option<u32>,
    tokens: Vec<String>,
    text_enters: usize,
    text_leaves: usize,
    committed: Vec<Option<String>>,
    preedit: Vec<Option<String>>,
    text_done: usize,
    ime_active: bool,
    ime_done: u32,
    surrounding: Option<String>,
    cursor_rectangle: Option<(i32, i32, i32, i32)>,
    grabbed_keys: usize,
    ext_manager_done: usize,
    ext_groups: Vec<ExtWorkspaceGroupHandleV1>,
    ext_handles: Vec<ExtWorkspaceHandleV1>,
    ext_group_caps: Vec<u32>,
    ext_output_enters: Vec<wl_output::WlOutput>,
    ext_output_leaves: usize,
    ext_workspace_enters: usize,
    ext_workspace_leaves: usize,
    ext_ids: Vec<String>,
    ext_names: Vec<String>,
    ext_coords: Vec<Vec<u32>>,
    ext_states: Vec<u32>,
    ext_caps: Vec<u32>,
    ext_removed: usize,
    layer_configures: Vec<(u32, u32, u32)>,
    dmabuf_failed: bool,
    dmabuf_feedback_done: usize,
    dmabuf_modifiers: usize,
    lock_confirmed: usize,
    lock_finished: usize,
    lock_configures: Vec<(u32, u32, u32)>,
}

macro_rules! on_event {
    ($interface:path, $handler:expr) => {
        impl Dispatch<$interface, ()> for Events {
            fn event(
                state: &mut Self,
                proxy: &$interface,
                event: <$interface as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                let handler: fn(&mut Self, &$interface, <$interface as Proxy>::Event) = $handler;
                handler(state, proxy, event);
            }
        }
    };
}

on_event!(wl_registry::WlRegistry, |state, _, event| {
    if let wl_registry::Event::Global {
        name,
        interface,
        version,
    } = event
    {
        state.globals.push((name, interface, version));
    }
});
on_event!(wl_output::WlOutput, |_, _, _| {});
delegate_noop!(Events: ignore ext_session_lock_manager_v1::ExtSessionLockManagerV1);
on_event!(ext_session_lock_v1::ExtSessionLockV1, |state, _, event| {
    match event {
        ext_session_lock_v1::Event::Locked => state.lock_confirmed += 1,
        ext_session_lock_v1::Event::Finished => state.lock_finished += 1,
        _ => {}
    }
});
on_event!(
    ext_session_lock_surface_v1::ExtSessionLockSurfaceV1,
    |state, _, event| {
        if let ext_session_lock_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            state.lock_configures.push((serial, width, height));
        }
    }
);
delegate_noop!(Events: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(Events: ignore wl_subsurface::WlSubsurface);
on_event!(zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, |state, _, event| {
    if let zwp_linux_dmabuf_v1::Event::Modifier { .. } = event {
        state.dmabuf_modifiers += 1;
    }
});
on_event!(
    zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
    |state, _, event| {
        if let zwp_linux_dmabuf_feedback_v1::Event::Done = event {
            state.dmabuf_feedback_done += 1;
        }
    }
);
on_event!(
    zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    |state, _, event| {
        if let zwp_linux_buffer_params_v1::Event::Failed = event {
            state.dmabuf_failed = true;
        }
    }
);

delegate_noop!(Events: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for Events {
    fn event(
        state: &mut Self,
        _: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            state.layer_configures.push((serial, width, height));
        }
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for Events {
    fn event(
        state: &mut Self,
        _: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                state.ext_groups.push(workspace_group)
            }
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                state.ext_handles.push(workspace)
            }
            ext_workspace_manager_v1::Event::Done => state.ext_manager_done += 1,
            _ => {}
        }
    }

    // Objects created by manager events carry no user data (opcodes per the
    // v1 interface: 0 = workspace_group, 1 = workspace).
    event_created_child!(Events, ExtWorkspaceManagerV1, [
        0 => (ExtWorkspaceGroupHandleV1, ()),
        1 => (ExtWorkspaceHandleV1, ()),
    ]);
}

on_event!(ExtWorkspaceGroupHandleV1, |events, _, event| match event {
    ext_workspace_group_handle_v1::Event::Capabilities {
        capabilities: WEnum::Value(caps),
    } => events.ext_group_caps.push(caps.bits()),
    ext_workspace_group_handle_v1::Event::OutputEnter { output } => {
        events.ext_output_enters.push(output)
    }
    ext_workspace_group_handle_v1::Event::OutputLeave { .. } => events.ext_output_leaves += 1,
    ext_workspace_group_handle_v1::Event::WorkspaceEnter { .. } => {
        events.ext_workspace_enters += 1
    }
    ext_workspace_group_handle_v1::Event::WorkspaceLeave { .. } => {
        events.ext_workspace_leaves += 1
    }
    ext_workspace_group_handle_v1::Event::Removed => events.ext_removed += 1,
    _ => {}
});

on_event!(ExtWorkspaceHandleV1, |events, _, event| match event {
    ext_workspace_handle_v1::Event::Id { id } => events.ext_ids.push(id),
    ext_workspace_handle_v1::Event::Name { name } => events.ext_names.push(name),
    ext_workspace_handle_v1::Event::Coordinates { coordinates } => {
        events.ext_coords.push(
            coordinates
                .chunks_exact(4)
                .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                .collect(),
        );
    }
    ext_workspace_handle_v1::Event::State {
        state: WEnum::Value(flags),
    } => events.ext_states.push(flags.bits()),
    ext_workspace_handle_v1::Event::Capabilities {
        capabilities: WEnum::Value(caps),
    } => events.ext_caps.push(caps.bits()),
    ext_workspace_handle_v1::Event::Removed => events.ext_removed += 1,
    _ => {}
});
on_event!(
    wp_fractional_scale_v1::WpFractionalScaleV1,
    |state, _, event| {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.scales.push(scale);
        }
    }
);
on_event!(
    xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1,
    |state, _, event| {
        if let xdg_toplevel_icon_manager_v1::Event::Done = event {
            state.icon_done = true;
        }
    }
);
on_event!(xdg_wm_base::XdgWmBase, |_, proxy, event| {
    if let xdg_wm_base::Event::Ping { serial } = event {
        proxy.pong(serial);
    }
});
on_event!(xdg_surface::XdgSurface, |_, proxy, event| {
    if let xdg_surface::Event::Configure { serial } = event {
        proxy.ack_configure(serial);
    }
});
on_event!(wl_keyboard::WlKeyboard, |state, _, event| {
    if let wl_keyboard::Event::Enter { serial, .. } = event {
        state.keyboard_enters.push(serial);
    }
});
on_event!(wl_pointer::WlPointer, |state, _, event| {
    if let wl_pointer::Event::Enter { serial, .. } = event {
        state.pointer_enter = Some(serial);
    }
});
on_event!(
    xdg_activation_token_v1::XdgActivationTokenV1,
    |state, _, event| {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            state.tokens.push(token);
        }
    }
);
on_event!(zwp_text_input_v3::ZwpTextInputV3, |state, _, event| {
    match event {
        zwp_text_input_v3::Event::Enter { .. } => state.text_enters += 1,
        zwp_text_input_v3::Event::Leave { .. } => state.text_leaves += 1,
        zwp_text_input_v3::Event::CommitString { text } => state.committed.push(text),
        zwp_text_input_v3::Event::PreeditString { text, .. } => state.preedit.push(text),
        zwp_text_input_v3::Event::Done { .. } => state.text_done += 1,
        _ => {}
    }
});
on_event!(zwp_input_method_v2::ZwpInputMethodV2, |state, _, event| {
    match event {
        zwp_input_method_v2::Event::Activate => state.ime_active = true,
        zwp_input_method_v2::Event::Deactivate => state.ime_active = false,
        zwp_input_method_v2::Event::Done => state.ime_done += 1,
        zwp_input_method_v2::Event::SurroundingText { text, .. } => state.surrounding = Some(text),
        _ => {}
    }
});
on_event!(
    zwp_input_popup_surface_v2::ZwpInputPopupSurfaceV2,
    |state, _, event| {
        if let zwp_input_popup_surface_v2::Event::TextInputRectangle {
            x,
            y,
            width,
            height,
        } = event
        {
            state.cursor_rectangle = Some((x, y, width, height));
        }
    }
);
on_event!(
    zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2,
    |state, _, event| {
        if let zwp_input_method_keyboard_grab_v2::Event::Key { .. } = event {
            state.grabbed_keys += 1;
        }
    }
);

delegate_noop!(Events: ignore wl_compositor::WlCompositor);
delegate_noop!(Events: ignore wl_surface::WlSurface);
delegate_noop!(Events: ignore wl_seat::WlSeat);
delegate_noop!(Events: ignore wl_shm::WlShm);
delegate_noop!(Events: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Events: ignore wl_buffer::WlBuffer);
delegate_noop!(Events: ignore xdg_toplevel::XdgToplevel);
delegate_noop!(Events: ignore wp_viewporter::WpViewporter);
delegate_noop!(Events: ignore wp_viewport::WpViewport);
delegate_noop!(Events: ignore wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
delegate_noop!(Events: ignore wp_cursor_shape_manager_v1::WpCursorShapeManagerV1);
delegate_noop!(Events: ignore wp_cursor_shape_device_v1::WpCursorShapeDeviceV1);
delegate_noop!(Events: ignore xdg_activation_v1::XdgActivationV1);
delegate_noop!(Events: ignore xdg_toplevel_icon_v1::XdgToplevelIconV1);
delegate_noop!(Events: ignore zwp_text_input_manager_v3::ZwpTextInputManagerV3);
delegate_noop!(Events: ignore zwp_input_method_manager_v2::ZwpInputMethodManagerV2);

#[test]
fn fractional_scale_and_viewport_are_committed_and_reported() {
    roundtrip_test(|client, control| {
        let surface = client.surface();
        let manager = client.bind::<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>(1);
        let fractional = manager.get_fractional_scale(&surface, &client.queue.handle(), ());
        let viewporter = client.bind::<wp_viewporter::WpViewporter>(1);
        let viewport = viewporter.get_viewport(&surface, &client.queue.handle(), ());
        viewport.set_destination(32, 16);
        let buffer = client.buffer(64, 32);
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        client.roundtrip();
        assert_eq!(client.events.scales, [120]);
        let id = surface.id().protocol_id();
        assert!(control.call(move |state, peer| {
            let surface = peer
                .object_from_protocol_id::<WlSurface>(&state.display_handle, id)
                .unwrap();
            with_states(&surface, |states| {
                states
                    .cached_state
                    .get::<ViewportCachedState>()
                    .current()
                    .dst
                    == Some((32, 16).into())
            })
        }));
        fractional.destroy();
        viewport.destroy();
        surface.destroy();
        client.roundtrip();
    });
}

#[test]
fn toplevel_icons_are_double_buffered_and_can_be_unset() {
    roundtrip_test(|client, control| {
        let (surface, toplevel) = client.window();
        let manager = client.bind::<xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1>(1);
        let icon = manager.create_icon(&client.queue.handle(), ());
        icon.set_name("wisp-test-icon".into());
        let pixels = client.buffer(16, 16);
        icon.add_buffer(&pixels, 1);
        manager.set_icon(&toplevel, Some(&icon));
        client.roundtrip();
        assert!(client.events.icon_done);
        assert_eq!(
            control.call(|state, _| Wisp::icon_metadata(&state.windows[0].window)),
            (None, 0)
        );
        surface.commit();
        client.roundtrip();
        assert_eq!(
            control.call(|state, _| Wisp::icon_metadata(&state.windows[0].window)),
            (Some("wisp-test-icon".into()), 1)
        );
        // Both destructors in one batch are valid; assigned metadata must
        // survive them, including a later surface commit.
        icon.destroy();
        pixels.destroy();
        surface.commit();
        client.roundtrip();
        assert_eq!(
            control.call(|state, _| Wisp::icon_metadata(&state.windows[0].window)),
            (Some("wisp-test-icon".into()), 1)
        );
        manager.set_icon(&toplevel, None);
        surface.commit();
        client.roundtrip();
        assert_eq!(
            control.call(|state, _| Wisp::icon_metadata(&state.windows[0].window)),
            (None, 0)
        );
    });
}

#[test]
fn icons_can_be_destroyed_before_the_first_surface_commit() {
    roundtrip_test(|client, control| {
        let (surface, toplevel) = client.window();
        let manager = client.bind::<xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1>(1);
        let icon = manager.create_icon(&client.queue.handle(), ());
        icon.set_name("pending-icon".into());
        manager.set_icon(&toplevel, Some(&icon));
        icon.destroy();
        surface.commit();
        client.roundtrip();
        assert_eq!(
            control.call(|state, _| Wisp::icon_metadata(&state.windows[0].window)),
            (Some("pending-icon".into()), 0)
        );
    });
}

#[test]
fn icon_buffer_replacement_releases_only_the_replaced_buffer() {
    roundtrip_test(|client, control| {
        let (surface, toplevel) = client.window();
        let manager = client.bind::<xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1>(1);
        let icon = manager.create_icon(&client.queue.handle(), ());
        let old = client.buffer(16, 16);
        let replacement = client.buffer(16, 16);
        let large = client.buffer(32, 32);
        icon.add_buffer(&old, 1);
        icon.add_buffer(&replacement, 1);
        icon.add_buffer(&large, 2);
        old.destroy();
        manager.set_icon(&toplevel, Some(&icon));
        surface.commit();
        client.roundtrip();
        assert_eq!(
            control.call(|state, _| Wisp::icon_metadata(&state.windows[0].window)),
            (None, 2)
        );
        icon.destroy();
        replacement.destroy();
        large.destroy();
        client.roundtrip();
    });
}

#[test]
fn icons_reject_premature_buffer_destruction_even_when_shared() {
    for shared in [false, true] {
        roundtrip_test(move |client, _| {
            let manager = client.bind::<xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1>(1);
            let icon = manager.create_icon(&client.queue.handle(), ());
            let pixels = client.buffer(16, 16);
            icon.add_buffer(&pixels, 1);
            if shared {
                let other = manager.create_icon(&client.queue.handle(), ());
                other.add_buffer(&pixels, 1);
                other.destroy();
            }
            client.roundtrip();
            pixels.destroy();
            // Keep this destructor in the same batch: invalid ordering must
            // still fail, not just the buffer-only variant.
            icon.destroy();
            client.expect_protocol_error(xdg_toplevel_icon_v1::Error::NoBuffer as u32);
        });
    }
}

#[test]
fn icons_reject_mutation_after_assignment_without_panicking() {
    for add_buffer in [false, true] {
        roundtrip_test(move |client, _| {
            let (_, toplevel) = client.window();
            let manager = client.bind::<xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1>(1);
            let icon = manager.create_icon(&client.queue.handle(), ());
            icon.set_name("immutable".into());
            manager.set_icon(&toplevel, Some(&icon));
            client.roundtrip();
            if add_buffer {
                icon.add_buffer(&client.buffer(16, 16), 1);
            } else {
                icon.set_name("invalid change".into());
            }
            client.expect_protocol_error(xdg_toplevel_icon_v1::Error::Immutable as u32);
        });
    }
}

#[test]
fn icons_reject_non_square_buffers_and_invalid_scale() {
    for (width, height, scale) in [(16, 8, 1), (16, 16, 0), (16, 16, -1)] {
        roundtrip_test(move |client, _| {
            let manager = client.bind::<xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1>(1);
            let icon = manager.create_icon(&client.queue.handle(), ());
            icon.add_buffer(&client.buffer(width, height), scale);
            client.expect_protocol_error(xdg_toplevel_icon_v1::Error::InvalidBuffer as u32);
        });
    }
}

#[test]
fn cursor_shapes_require_pointer_focus_and_an_enter_serial() {
    roundtrip_test(|client, control| {
        let surface = client.surface();
        let seat = client.bind::<wl_seat::WlSeat>(7);
        let pointer = seat.get_pointer(&client.queue.handle(), ());
        let manager = client.bind::<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>(1);
        let shape = manager.get_pointer(&pointer, &client.queue.handle(), ());
        shape.set_shape(0, wp_cursor_shape_device_v1::Shape::Text);
        client.roundtrip();
        assert!(control.call(|state, _| matches!(
            state.cursor_status,
            smithay::input::pointer::CursorImageStatus::Named(
                smithay::input::pointer::CursorIcon::Default
            )
        )));
        let id = surface.id().protocol_id();
        control.call(move |state, peer| {
            let surface = peer
                .object_from_protocol_id::<WlSurface>(&state.display_handle, id)
                .unwrap();
            let pointer = state.seat.get_pointer().unwrap();
            pointer.motion(
                state,
                Some((surface, (0.0, 0.0).into())),
                &MotionEvent {
                    location: (4.0, 4.0).into(),
                    serial: SERIAL_COUNTER.next_serial(),
                    time: 0,
                },
            );
            pointer.frame(state);
        });
        client.roundtrip();
        shape.set_shape(
            client.events.pointer_enter.unwrap(),
            wp_cursor_shape_device_v1::Shape::Text,
        );
        client.roundtrip();
        assert!(control.call(|state, _| matches!(
            state.cursor_status,
            smithay::input::pointer::CursorImageStatus::Named(
                smithay::input::pointer::CursorIcon::Text
            )
        )));
    });
}

#[test]
fn activation_is_one_shot_and_untrusted_requests_only_mark_attention() {
    roundtrip_test(|client, control| {
        let seat = client.bind::<wl_seat::WlSeat>(7);
        let _keyboard = seat.get_keyboard(&client.queue.handle(), ());
        let (first, _) = client.window();
        let (second, _) = client.window();
        let activation = client.bind::<xdg_activation_v1::XdgActivationV1>(1);
        control.call(|state, _| {
            state.windows[0].workspace = 2;
            state.arrange();
        });
        let attention = client.activation_token(&activation, &first, None);
        activation.activate(attention.clone(), &first);
        client.roundtrip();
        assert!(control.call(|state, _| state.active_workspace == 1
            && state.focused == Some(2)
            && state.windows[0].urgent
            && state.activation_state.tokens().count() == 0));
        control.call(|state, _| {
            state.dispatch(Action::Workspace { index: 2 }).unwrap();
            state.dispatch(Action::Workspace { index: 1 }).unwrap();
        });
        activation.activate(attention, &first);
        client.roundtrip();
        assert!(control.call(|state, _| !state.windows[0].urgent && state.active_workspace == 1));
        let forged = client.activation_token(&activation, &second, Some((u32::MAX, &seat)));
        activation.activate(forged, &first);
        client.roundtrip();
        assert_eq!(control.call(|state, _| state.active_workspace), 1);
        let serial = *client.events.keyboard_enters.last().unwrap();
        let genuine = client.activation_token(&activation, &second, Some((serial, &seat)));
        activation.activate(genuine, &first);
        client.roundtrip();
        assert!(control.call(|state, _| state.active_workspace == 2
            && state.focused == Some(1)
            && !state.windows[0].urgent));
    });
}

#[test]
fn activation_tokens_expire_and_storage_is_bounded() {
    roundtrip_test(|client, control| {
        let (first, _) = client.window();
        let _ = client.window();
        let activation = client.bind::<xdg_activation_v1::XdgActivationV1>(1);
        let expired = control.call(|state, peer| {
            let data = smithay::wayland::xdg_activation::XdgActivationTokenData {
                client_id: Some(peer.id()),
                timestamp: Instant::now() - super::ACTIVATION_LIFETIME * 2,
                ..Default::default()
            };
            data.user_data
                .insert_if_missing(|| super::ActivationGrant { can_focus: true });
            state
                .activation_state
                .create_external_token(data)
                .0
                .as_str()
                .to_owned()
        });
        activation.activate(expired, &first);
        client.roundtrip();
        assert!(control.call(|state, _| state.focused == Some(2)
            && !state.windows[0].urgent
            && state.activation_state.tokens().count() == 0));

        // An unused expired token must be pruned as well, without waiting for
        // the client to try to activate it.
        control.call(|state, _| {
            state.activation_state.create_external_token(
                smithay::wayland::xdg_activation::XdgActivationTokenData {
                    timestamp: Instant::now() - super::ACTIVATION_LIFETIME * 2,
                    ..Default::default()
                },
            );
        });
        client.activation_token(&activation, &first, None);
        assert_eq!(
            control.call(|state, _| state.activation_state.tokens().count()),
            1
        );
        for _ in 0..super::MAX_ACTIVATION_TOKENS + 5 {
            client.activation_token(&activation, &first, None);
        }
        let count = control.call(|state, _| state.activation_state.tokens().count());
        assert!(count > 0 && count <= super::MAX_ACTIVATION_TOKENS);
    });
}

#[test]
fn text_input_and_separate_ime_exchange_text_grabs_and_popup_lifecycle() {
    roundtrip_test(|app, control| {
        let provider_stream = control.call(|state, _| {
            let (server, peer) = UnixStream::pair().unwrap();
            state
                .display_handle
                .insert_client(server, Arc::new(ClientState::default()))
                .unwrap();
            peer
        });
        let mut provider = TestClient::new(provider_stream);
        let provider_seat = provider.bind::<wl_seat::WlSeat>(7);
        let manager = provider.bind::<zwp_input_method_manager_v2::ZwpInputMethodManagerV2>(1);
        let ime = manager.get_input_method(&provider_seat, &provider.queue.handle(), ());
        provider.roundtrip();
        let app_seat = app.bind::<wl_seat::WlSeat>(7);
        let text_manager = app.bind::<zwp_text_input_manager_v3::ZwpTextInputManagerV3>(1);
        let text = text_manager.get_text_input(&app_seat, &app.queue.handle(), ());
        let (surface, _) = app.window();
        app.roundtrip();
        assert_eq!(app.events.text_enters, 1);
        text.enable();
        text.set_surrounding_text("hello".into(), 5, 5);
        text.set_cursor_rectangle(20, 30, 4, 16);
        text.commit();
        app.roundtrip();
        provider.roundtrip();
        assert!(provider.events.ime_active);
        assert_eq!(provider.events.surrounding.as_deref(), Some("hello"));
        ime.set_preedit_string("候補".into(), 0, 6);
        ime.commit_string("漢字".into());
        ime.commit(provider.events.ime_done);
        provider.roundtrip();
        app.roundtrip();
        assert_eq!(app.events.committed, [Some("漢字".into())]);
        assert_eq!(app.events.preedit, [Some("候補".into())]);
        assert_eq!(app.events.text_done, 1);
        let popup_surface = provider.surface();
        let popup = ime.get_input_popup_surface(&popup_surface, &provider.queue.handle(), ());
        let buffer = provider.buffer(16, 8);
        popup_surface.attach(Some(&buffer), 0, 0);
        popup_surface.commit();
        provider.roundtrip();
        text.set_cursor_rectangle(40, 50, 4, 16);
        text.commit();
        app.roundtrip();
        provider.roundtrip();
        assert_eq!(provider.events.cursor_rectangle, Some((40, 50, 4, 16)));
        let root_id = surface.id().protocol_id();
        assert_eq!(
            control.call(move |state, peer| {
                let root = peer
                    .object_from_protocol_id::<WlSurface>(&state.display_handle, root_id)
                    .unwrap();
                smithay::desktop::PopupManager::popups_for_surface(&root).count()
            }),
            1
        );
        let grab = ime.grab_keyboard(&provider.queue.handle(), ());
        provider.roundtrip();
        control.call(|state, _| {
            let keyboard = state.seat.get_keyboard().unwrap();
            keyboard.input::<(), _>(
                state,
                38u32.into(),
                KeyState::Pressed,
                SERIAL_COUNTER.next_serial(),
                1,
                |_, _, _| FilterResult::Forward,
            );
            keyboard.input::<(), _>(
                state,
                38u32.into(),
                KeyState::Released,
                SERIAL_COUNTER.next_serial(),
                2,
                |_, _, _| FilterResult::Forward,
            );
        });
        provider.roundtrip();
        assert_eq!(provider.events.grabbed_keys, 2);
        grab.release();
        provider.roundtrip();
        control.call(|state, _| state.focus(None));
        app.roundtrip();
        provider.roundtrip();
        assert_eq!(app.events.text_leaves, 1);
        assert!(!provider.events.ime_active);
        assert_eq!(
            control.call(move |state, peer| {
                let root = peer
                    .object_from_protocol_id::<WlSurface>(&state.display_handle, root_id)
                    .unwrap();
                smithay::desktop::PopupManager::popups_for_surface(&root).count()
            }),
            0
        );
        ime.commit_string("must not reach an unfocused client".into());
        ime.commit(provider.events.ime_done);
        provider.roundtrip();
        app.roundtrip();
        assert_eq!(app.events.committed.len(), 1);
        popup.destroy();
        popup_surface.destroy();
        ime.destroy();
        text.destroy();
        provider.roundtrip();
        app.roundtrip();
    });
}

/// Creates a `wl_output` global server-side, like every backend's screen setup.
fn spawn_output(control: &Control) {
    control.call(|state, _| {
        let output = Output::new(
            "WISP-1".to_owned(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Wisp".to_owned(),
                model: "Test".to_owned(),
            },
        );
        output.create_global::<Wisp>(&state.display_handle);
    });
}

/// Like `spawn_output`, but fully attaches the output the way a backend does:
/// mode, transform and space placement plus the compositor's `state.output`,
/// so the layer map, scene geometry and hit testing have a lone output.
fn spawn_mapped_output(control: &Control, size: (i32, i32)) {
    control.call(move |state, _| {
        let output = Output::new(
            "WISP-1".to_owned(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Wisp".to_owned(),
                model: "Test".to_owned(),
            },
        );
        output.create_global::<Wisp>(&state.display_handle);
        let mode = Mode {
            size: size.into(),
            refresh: 60_000,
        };
        output.change_current_state(
            Some(mode),
            Some(Transform::Flipped180),
            None,
            Some((0, 0).into()),
        );
        output.set_preferred(mode);
        state.space.map_output(&output, (0, 0));
        state.output = Some(output);
    });
}

#[test]
fn session_lock_is_absent_by_default_and_unlocks_only_after_confirmation() {
    let (mut display, mut state) = test_state();
    run_roundtrip(&mut display, &mut state, |client, _| {
        assert!(
            !client
                .events
                .globals
                .iter()
                .any(|(_, name, _)| name == "ext_session_lock_manager_v1")
        );
    });

    let (mut display, mut state) = test_state();
    state.session_lock_state = Some(
        smithay::wayland::session_lock::SessionLockManagerState::new::<Wisp, _>(
            &state.display_handle,
            |_| true,
        ),
    );
    run_roundtrip(&mut display, &mut state, |client, control| {
        spawn_mapped_output(control, (800, 600));
        client.roundtrip();
        let output = client.bind::<wl_output::WlOutput>(4);
        let manager = client.bind::<ext_session_lock_manager_v1::ExtSessionLockManagerV1>(1);
        let lock = manager.lock(&client.queue.handle(), ());
        client.roundtrip();
        assert_eq!(client.events.lock_confirmed, 0);
        control.call(|state, _| {
            state.x11_pending_selections[0] =
                Some(super::super::xwayland::PendingSelection::Wayland(Some(
                    vec!["text/plain".to_string()],
                )));
        });
        control.call(|state, _| {
            // A locked session must not offer the stored desktop selection to
            // X11 before confirmation; the restore must be a locked no-op.
            state.restore_x11_selections();
            state.x11_pending_selections[0].is_some()
        });
        assert!(control.call(|state, _| state.session_lock_pending()
            && state.surface_under((20.0, 20.0).into()).is_none()
            && state.dispatch(Action::Workspace { index: 2 }).is_err()));
        assert!(control.call(|state, _| {
            state.keyboard_key(64u32.into(), KeyState::Pressed, 1, false);
            state.keyboard_key(11u32.into(), KeyState::Pressed, 2, false);
            let unchanged = state.active_workspace == 1;
            state.keyboard_key(11u32.into(), KeyState::Released, 3, false);
            state.keyboard_key(64u32.into(), KeyState::Released, 4, false);
            unchanged
        }));
        let surface = client.surface();
        let lock_surface = lock.get_lock_surface(&surface, &output, &client.queue.handle(), ());
        client.roundtrip();
        let (serial, width, height) = *client.events.lock_configures.last().unwrap();
        assert_eq!((width, height), (800, 600));
        lock_surface.ack_configure(serial);
        surface.attach(Some(&client.buffer(800, 600)), 0, 0);
        surface.commit();
        client.roundtrip();
        assert!(control.call(|state, _| {
            let lock_surface = state.lock_surface().unwrap().wl_surface().clone();
            state.focused = Some(u64::MAX);
            state.focus(None); // a desktop window disappearing must not steal lock focus
            state.seat.get_keyboard().unwrap().current_focus() == Some(lock_surface)
        }));
        // Simulate completion of the black frame; a real backend confirms
        // only after nested submit or the matching direct page flip.
        control.call(|state, _| state.confirm_session_lock());
        client.roundtrip();
        assert_eq!(client.events.lock_confirmed, 1);
        lock.unlock_and_destroy();
        client.roundtrip();
        assert!(control.call(|state, _| {
            state.session_lock.is_none()
                    // Unlock must consume the stored pending selection.
                    && state.x11_pending_selections[0].is_none()
        }));
    });
}

#[test]
fn rejected_second_lock_cannot_unlock_the_first() {
    let (mut display, mut state) = test_state();
    state.session_lock_state = Some(
        smithay::wayland::session_lock::SessionLockManagerState::new::<Wisp, _>(
            &state.display_handle,
            |_| true,
        ),
    );
    run_roundtrip(&mut display, &mut state, |client, control| {
        let manager = client.bind::<ext_session_lock_manager_v1::ExtSessionLockManagerV1>(1);
        let _owner = manager.lock(&client.queue.handle(), ());
        client.roundtrip();
        let rejected = manager.lock(&client.queue.handle(), ());
        client.roundtrip();
        assert_eq!(client.events.lock_finished, 1);
        rejected.unlock_and_destroy();
        client.expect_protocol_error(1); // invalid_unlock on second object
        assert!(control.call(|state, _| state.session_lock_pending()
            && state.dispatch(Action::Workspace { index: 2 }).is_err()));
    });
}

#[test]
fn disconnected_locker_leaves_the_session_locked_and_desktop_isolated() {
    let (mut display, mut state) = test_state();
    state.session_lock_state = Some(
        smithay::wayland::session_lock::SessionLockManagerState::new::<Wisp, _>(
            &state.display_handle,
            |_| true,
        ),
    );
    run_roundtrip(&mut display, &mut state, |client, control| {
        let manager = client.bind::<ext_session_lock_manager_v1::ExtSessionLockManagerV1>(1);
        let _lock = manager.lock(&client.queue.handle(), ());
        client.roundtrip();
        control.call(|state, _| state.confirm_session_lock());
        client.roundtrip();
        assert_eq!(client.events.lock_confirmed, 1);
    });
    display.dispatch_clients(&mut state).unwrap();
    assert!(state.session_lock.is_some());
    assert!(state.surface_under((10.0, 10.0).into()).is_none());
    assert!(state.dispatch(Action::Workspace { index: 2 }).is_err());
}

#[test]
fn scratchpad_hides_and_reappears_on_current_workspace() {
    roundtrip_test(|client, control| {
        spawn_mapped_output(control, (800, 600));
        let (_surface, toplevel) = client.window();
        control.call(|state, _| {
            assert_eq!(state.focused, Some(1));
            state.dispatch(Action::SendToScratchpad {}).unwrap();
            assert!(state.windows[0].scratchpad_hidden);
            assert!(
                state
                    .space
                    .element_location(&state.windows[0].window)
                    .is_none()
            );
            state.dispatch(Action::Workspace { index: 2 }).unwrap();
            state.dispatch(Action::ToggleScratchpad {}).unwrap();
            assert_eq!(state.windows[0].workspace, 2);
            assert!(!state.windows[0].scratchpad_hidden);
            assert_eq!(state.focused, Some(1));
            state.dispatch(Action::Workspace { index: 3 }).unwrap();
            assert!(state.windows[0].scratchpad_hidden);
            state.dispatch(Action::ToggleScratchpad {}).unwrap();
            assert_eq!(state.windows[0].workspace, 3);
            assert!(!state.windows[0].scratchpad_hidden);
            state.dispatch(Action::ToggleScratchpad {}).unwrap();
            assert!(state.windows[0].scratchpad_hidden);
            assert!(
                state
                    .space
                    .element_location(&state.windows[0].window)
                    .is_none()
            );
        });
        // Drain and acknowledge all transition configures before destroying
        // the xdg_toplevel; the test client acks them during event dispatch.
        client.roundtrip();
        client.roundtrip();
        toplevel.destroy();
        client.roundtrip();
        assert!(control.call(|state, _| state.scratchpad.is_none()));
    });
}

#[test]
fn slide_keeps_outgoing_windows_mapped_until_finished() {
    roundtrip_test(|client, control| {
        spawn_mapped_output(control, (800, 600));
        let (_surface, _toplevel) = client.window();
        control.call(|state, _| {
            state.config.layout.workspace_slide = true;
            assert!(
                state
                    .space
                    .element_location(&state.windows[0].window)
                    .is_some()
            );
            state.dispatch(Action::Workspace { index: 2 }).unwrap();
            assert!(state.workspace_slide.is_some());
            assert!(
                state
                    .space
                    .element_location(&state.windows[0].window)
                    .is_some()
            );
            assert!(state.surface_under((20.0, 20.0).into()).is_none());
            state.finish_workspace_slide(Instant::now() + Duration::from_secs(1));
            assert!(state.workspace_slide.is_none());
            assert!(
                state
                    .space
                    .element_location(&state.windows[0].window)
                    .is_none()
            );
        });
    });
}

#[test]
fn new_window_fades_only_when_enabled_and_stops_after_its_deadline() {
    roundtrip_test(|client, control| {
        spawn_mapped_output(control, (800, 600));
        control.call(|state, _| state.config.layout.window_open_fade = true);
        let (_surface, _toplevel) = client.window();
        control.call(|state, _| {
            assert!(state.windows[0].fade.is_some());
            assert!(
                state.windows[0]
                    .fade
                    .as_ref()
                    .unwrap()
                    .opacity(Instant::now())
                    <= 1.0
            );
            state.windows[0].fade = None;
            state.config.layout.window_open_fade = false;
        });
        let (_surface, _toplevel) = client.window();
        assert!(control.call(|state, _| state.windows[1].fade.is_none()));
    });
}

#[test]
fn ext_workspace_binding_announces_group_output_and_active_workspace() {
    roundtrip_test(|client, _control| {
        spawn_output(_control);
        client.roundtrip();
        let output = client.bind::<wl_output::WlOutput>(4);
        client.roundtrip();
        let _manager = client.bind::<ExtWorkspaceManagerV1>(1);
        client.roundtrip();

        let events = &client.events;
        assert_eq!(events.ext_groups.len(), 1);
        assert_eq!(events.ext_group_caps, [1]); // create_workspace capability
        assert!(
            events
                .ext_output_enters
                .iter()
                .any(|entered| entered == &output),
            "group must announce the client's wl_output for per-output bars"
        );
        assert_eq!(events.ext_handles.len(), 2); // active + one empty spare
        assert_eq!(events.ext_ids, ["wisp-1", "wisp-2"]);
        assert_eq!(events.ext_names, ["1", "2"]);
        assert_eq!(events.ext_coords, [vec![1], vec![2]]);
        assert_eq!(events.ext_caps, [1 | 2 | 8; 2]); // activate | deactivate | assign
        assert_eq!(events.ext_states, [1, 4]); // active, hidden
        assert_eq!(events.ext_workspace_enters, 2);
        assert!(events.ext_manager_done >= 1);
    });
}

#[test]
fn ext_workspace_client_created_workspace_activation_switches() {
    roundtrip_test(|client, control| {
        spawn_output(control);
        client.roundtrip();
        let _wl_output = client.bind::<wl_output::WlOutput>(4);
        client.roundtrip();
        let manager = client.bind::<ExtWorkspaceManagerV1>(1);
        client.roundtrip();
        assert_eq!(client.events.ext_handles.len(), 2);

        // Noctalia can explicitly create a workspace beyond the spare.
        client.events.ext_groups[0].create_workspace("3".to_string());
        client.roundtrip();
        assert_eq!(client.events.ext_handles.len(), 3);
        assert_eq!(client.events.ext_names, ["1", "2", "3"]);
        // The newly created workspace starts inactive (hidden = 4).
        assert_eq!(client.events.ext_states.last(), Some(&4));

        // Activation (bar click) flows as activate + commit; the server must
        // make it the active workspace.
        client.events.ext_handles[2].activate();
        manager.commit();
        client.roundtrip();
        assert_eq!(control.call(|state, _| state.active_workspace), 3);
        assert_eq!(
            control.call(|state, _| state.materialized_workspaces.clone()),
            (1..=4).collect::<Vec<u8>>()
        );
        // The last state journal entries reflect the switch: workspace 1 is
        // now hidden and workspace 2 active.
        let tail = &client.events.ext_states[client.events.ext_states.len() - 2..];
        assert_eq!(tail, [4, 1]);
    });
}

#[test]
fn ext_workspace_keeps_one_spare_and_removes_unused_trailing_workspaces() {
    roundtrip_test(|client, control| {
        let _manager = client.bind::<ExtWorkspaceManagerV1>(1);
        client.roundtrip();
        assert_eq!(client.events.ext_names, ["1", "2"]);

        control.call(|state, _| state.dispatch(Action::Workspace { index: 5 }).unwrap());
        client.roundtrip();
        assert_eq!(client.events.ext_names, ["1", "2", "3", "4", "5", "6"]);
        assert_eq!(client.events.ext_removed, 0);

        control.call(|state, _| state.dispatch(Action::Workspace { index: 1 }).unwrap());
        client.roundtrip();
        assert_eq!(client.events.ext_removed, 4);
        assert_eq!(
            control.call(|state, _| state.materialized_workspaces.clone()),
            vec![1, 2]
        );
    });
}

#[test]
fn ext_workspace_retains_occupied_workspaces_until_the_window_closes() {
    roundtrip_test(|client, control| {
        let _manager = client.bind::<ExtWorkspaceManagerV1>(1);
        client.roundtrip();
        control.call(|state, _| state.dispatch(Action::Workspace { index: 6 }).unwrap());
        let (_surface, toplevel) = client.window();
        client.roundtrip();
        control.call(|state, _| state.dispatch(Action::Workspace { index: 1 }).unwrap());
        client.roundtrip();
        assert_eq!(
            control.call(|state, _| state.materialized_workspaces.last().copied()),
            Some(7)
        );

        toplevel.destroy();
        client.roundtrip();
        assert_eq!(
            control.call(|state, _| state.materialized_workspaces.clone()),
            vec![1, 2]
        );
    });
}

#[test]
fn hidden_and_unmapped_toplevel_commits_do_not_schedule_frames() {
    roundtrip_test(|client, control| {
        let (surface, _toplevel) = client.window();
        assert!(control.call(|state, _| state.windows[0].mapped));

        control.call(|state, _| state.dirty = false);
        surface.commit();
        client.roundtrip();
        assert!(control.call(|state, _| state.dirty));

        control.call(|state, _| state.dispatch(Action::Workspace { index: 2 }).unwrap());
        control.call(|state, _| state.dirty = false);
        surface.commit();
        client.roundtrip();
        assert!(!control.call(|state, _| state.dirty));

        // Unmapping a hidden window updates its state and workspace list, but
        // neither action needs a composited frame on the active workspace.
        surface.attach(None, 0, 0);
        surface.commit();
        client.roundtrip();
        assert!(!control.call(|state, _| state.dirty));
        assert!(!control.call(|state, _| state.windows[0].mapped));

        control.call(|state, _| state.dispatch(Action::Workspace { index: 1 }).unwrap());
        control.call(|state, _| state.dirty = false);
        surface.commit();
        client.roundtrip();
        assert!(!control.call(|state, _| state.dirty));
        assert!(control.call(|state, _| state.stats.skipped_invisible_commits >= 3));
    });
}

#[test]
fn overview_keyboard_selection_and_enter_release_stay_in_compositor() {
    roundtrip_test(|client, control| {
        let _first = client.window();
        let _second = client.window();
        control.call(|state, _| state.dispatch(Action::Workspace { index: 2 }).unwrap());
        let _hidden = client.window();
        control.call(|state, _| {
            state.dispatch(Action::Workspace { index: 1 }).unwrap();
            state.set_overview(true);
            assert_eq!(state.overview_items().len(), 2);
            let first = state.overview_items()[0].0;
            let second = state.overview_items()[1].0;
            state.focus(Some(first));

            // XKB keycodes: evdev codes + 8 (Tab 15, Shift 42, Return 28).
            state.keyboard_key(23u32.into(), KeyState::Pressed, 1, false);
            assert_eq!(state.focused, Some(second));
            assert!(state.overview);
            assert!(state.seat.get_keyboard().unwrap().current_focus().is_none());
            state.keyboard_key(23u32.into(), KeyState::Released, 2, false);
            state.keyboard_key(50u32.into(), KeyState::Pressed, 3, false);
            state.keyboard_key(23u32.into(), KeyState::Pressed, 4, false);
            assert_eq!(state.focused, Some(first));
            state.keyboard_key(23u32.into(), KeyState::Released, 5, false);
            state.keyboard_key(50u32.into(), KeyState::Released, 6, false);

            // Arrow navigation uses thumbnail geometry and consumes its release.
            state.keyboard_key(114u32.into(), KeyState::Pressed, 6, false);
            assert_eq!(state.focused, Some(second));
            assert!(state.seat.get_keyboard().unwrap().current_focus().is_none());
            state.keyboard_key(114u32.into(), KeyState::Released, 6, false);
            state.keyboard_key(113u32.into(), KeyState::Pressed, 6, false);
            assert_eq!(state.focused, Some(first));
            state.keyboard_key(113u32.into(), KeyState::Released, 6, false);

            state.keyboard_key(36u32.into(), KeyState::Pressed, 7, false);
            assert!(!state.overview);
            assert!(state.overview_exiting);
            assert!(state.suppressed_keys.contains(&36));
            assert_eq!(state.focused, Some(first));
            // Logical focus changes must not re-enable client input mid-exit.
            state.focus(Some(second));
            assert!(state.seat.get_keyboard().unwrap().current_focus().is_none());
            state.focus(Some(first));
            // Model the end of the spring transition before Return is released.
            for entry in &mut state.windows {
                entry.spring = None;
            }
            state.finish_overview_transition();
            state.keyboard_key(36u32.into(), KeyState::Released, 8, false);
            assert!(!state.suppressed_keys.contains(&36));
            assert!(!state.overview_exiting);
        });
    });
}

#[test]
fn workspace_return_restores_focus_and_ignores_closed_or_moved_windows() {
    roundtrip_test(|client, control| {
        let (_first_surface, first_toplevel) = client.window();
        let _second = client.window();
        let (first, second) = control.call(|state, _| {
            let ids = (state.windows[0].id, state.windows[1].id);
            state.focus(Some(ids.0));
            state.dispatch(Action::Workspace { index: 2 }).unwrap();
            ids
        });
        let _third = client.window();
        let third = control.call(|state, _| state.focused.unwrap());
        control.call(move |state, _| {
            state.dispatch(Action::WorkspaceBackAndForth {}).unwrap();
            assert_eq!(state.focused, Some(first));
            state.dispatch(Action::WorkspaceBackAndForth {}).unwrap();
            assert_eq!(state.focused, Some(third));
        });

        // Dispatch queued configures while the toplevel still exists. The
        // synthetic client's handler acknowledges every configure; destroying
        // the role first would send those acknowledgements after its reset.
        client.roundtrip();
        first_toplevel.destroy();
        client.roundtrip();
        control.call(move |state, _| {
            state.dispatch(Action::WorkspaceBackAndForth {}).unwrap();
            assert_eq!(state.focused, Some(second));
            state
                .dispatch(Action::MoveToWorkspace { index: 2 })
                .unwrap();
            assert_eq!(state.focused, None);
            state.dispatch(Action::WorkspaceBackAndForth {}).unwrap();
            assert_eq!(state.focused, Some(third));
            state.dispatch(Action::WorkspaceBackAndForth {}).unwrap();
            assert_eq!(state.focused, None);
        });
        client.roundtrip();
    });
}

#[test]
fn directional_navigation_and_swapping_exclude_hidden_fullscreen_and_floating_tiles() {
    use crate::layout::{Direction, LayoutKind};
    roundtrip_test(|client, control| {
        let _first = client.window();
        let _second = client.window();
        control.call(|state, _| state.dispatch(Action::Workspace { index: 2 }).unwrap());
        let _hidden = client.window();
        control.call(|state, _| {
            state.dispatch(Action::Workspace { index: 1 }).unwrap();
            state
                .dispatch(Action::Layout {
                    kind: LayoutKind::Master,
                })
                .unwrap();
            let first = state.windows[0].id;
            let second = state.windows[1].id;
            state.focus(Some(first));
            state
                .dispatch(Action::FocusDirection {
                    direction: Direction::Right,
                })
                .unwrap();
            assert_eq!(state.focused, Some(second));
            state
                .dispatch(Action::FocusDirection {
                    direction: Direction::Right,
                })
                .unwrap();
            assert_eq!(
                state.focused,
                Some(second),
                "directional focus must not wrap"
            );

            let first_rect = state.windows[0].rect;
            let second_rect = state.windows[1].rect;
            state
                .dispatch(Action::SwapDirection {
                    direction: Direction::Left,
                })
                .unwrap();
            assert_eq!(state.windows[0].id, second);
            assert_eq!(state.windows[0].rect, first_rect);
            assert_eq!(state.windows[1].id, first);
            assert_eq!(state.windows[1].rect, second_rect);
            assert_eq!(state.focused, Some(second));

            state.dispatch(Action::ToggleFullscreen {}).unwrap();
            state.dispatch(Action::FocusNext {}).unwrap();
            state
                .dispatch(Action::FocusDirection {
                    direction: Direction::Right,
                })
                .unwrap();
            state
                .dispatch(Action::SwapDirection {
                    direction: Direction::Right,
                })
                .unwrap();
            assert_eq!(state.focused, Some(second));
            assert_eq!(state.windows[0].id, second);
            state.dispatch(Action::ToggleFullscreen {}).unwrap();
            state.dispatch(Action::ToggleFloating {}).unwrap();
            state
                .dispatch(Action::SwapDirection {
                    direction: Direction::Right,
                })
                .unwrap();
            assert_eq!(
                state.windows[0].id, second,
                "floating windows do not swap tiled slots"
            );
        });
    });
}

#[test]
fn disabling_overview_animation_clears_motion_and_restores_focus_on_exit() {
    roundtrip_test(|client, control| {
        let _window = client.window();
        control.call(|state, _| {
            state.host_focus(true);
            state.set_overview(true);
            assert!(state.windows.iter().any(|entry| entry.spring.is_some()));
            state.config.layout.overview_animation = false;
            state.set_overview(false);
            assert!(!state.overview_exiting);
            assert!(state.windows.iter().all(|entry| entry.spring.is_none()));
            assert!(state.seat.get_keyboard().unwrap().current_focus().is_some());

            state.set_overview(true);
            assert!(state.overview);
            assert!(state.windows.iter().all(|entry| entry.spring.is_none()));
            assert!(state.seat.get_keyboard().unwrap().current_focus().is_none());
            state.set_overview(false);
            assert!(!state.overview_exiting);
            assert!(state.seat.get_keyboard().unwrap().current_focus().is_some());
        });
    });
}

#[test]
fn initial_bufferless_toplevel_commit_only_sends_configure() {
    roundtrip_test(|client, control| {
        let surface = client.surface();
        let xdg = client.bind::<xdg_wm_base::XdgWmBase>(6).get_xdg_surface(
            &surface,
            &client.queue.handle(),
            (),
        );
        let _toplevel = xdg.get_toplevel(&client.queue.handle(), ());
        client.roundtrip();
        control.call(|state, _| state.dirty = false);
        surface.commit();
        client.roundtrip();
        assert!(!control.call(|state, _| state.dirty));
    });
}

#[test]
fn synchronized_subsurface_waits_for_parent_commit_before_redraw() {
    roundtrip_test(|client, control| {
        let (parent, _toplevel) = client.window();
        let child = client.surface();
        let subcompositor = client.bind::<wl_subcompositor::WlSubcompositor>(1);
        let _subsurface = subcompositor.get_subsurface(&child, &parent, &client.queue.handle(), ());
        client.roundtrip();

        control.call(|state, _| state.dirty = false);
        let buffer = client.buffer(32, 32);
        child.attach(Some(&buffer), 0, 0);
        child.commit();
        client.roundtrip();
        assert!(!control.call(|state, _| state.dirty));

        parent.commit();
        client.roundtrip();
        assert!(control.call(|state, _| state.dirty));
    });
}

#[test]
fn rules_moving_first_buffer_to_hidden_workspace_unmap_the_window() {
    roundtrip_test(|client, control| {
        control.call(|state, _| {
            state.config.rules.push(crate::config::WindowRule {
                app_id: Some("wisp-hidden-test".to_owned()),
                title: None,
                floating: None,
                workspace: Some(2),
                size: None,
            });
        });
        let surface = client.surface();
        let xdg = client.bind::<xdg_wm_base::XdgWmBase>(6).get_xdg_surface(
            &surface,
            &client.queue.handle(),
            (),
        );
        let toplevel = xdg.get_toplevel(&client.queue.handle(), ());
        toplevel.set_app_id("wisp-hidden-test".to_owned());
        surface.commit();
        client.roundtrip();
        let buffer = client.buffer(64, 32);
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        client.roundtrip();
        assert!(control.call(|state, _| {
            state.windows[0].workspace == 2
                && state
                    .space
                    .element_location(&state.windows[0].window)
                    .is_none()
        }));
    });
}

#[test]
fn ext_workspace_late_output_bind_announces_to_existing_group() {
    roundtrip_test(|client, control| {
        // Wisp advertises the workspace global before any output, so bars bind
        // the manager first and only learn their outputs afterwards.
        let _manager = client.bind::<ExtWorkspaceManagerV1>(1);
        client.roundtrip();
        assert!(client.events.ext_output_enters.is_empty());

        // The output global shows up later: the group must be told about it
        // and a `done` must wake bars that were already listening.
        spawn_output(control);
        client.roundtrip();
        let output = client.bind::<wl_output::WlOutput>(4);
        client.roundtrip();

        assert!(
            client
                .events
                .ext_output_enters
                .iter()
                .any(|entered| entered == &output),
            "a late-bound output must be announced to the already-bound group"
        );
        // One `done` for the initial sync, one after the late output_enter.
        assert!(client.events.ext_manager_done >= 2);
    });
}

// ----------------------------------------------------------------------------
// wlr layer shell
// ----------------------------------------------------------------------------

/// Returns the output-local geometry of the named mapped layer surface.
fn mapped_layer_geometry(
    control: &Control,
    namespace: &'static str,
) -> Option<smithay::utils::Rectangle<i32, smithay::utils::Logical>> {
    control.call(move |state, _| {
        let output = state.output.clone()?;
        let layer = state
            .layer_surfaces
            .iter()
            .find(|layer| layer.namespace() == namespace)
            .cloned()?;
        layer_map_for_output(&output).layer_geometry(&layer)
    })
}

/// The critical stretch for the nested shell: noctalia's bar is an anchored
/// layer surface. The server must answer the surface's first commit with the
/// initial configure carrying the size the layer map computed from the
/// anchors (right-anchored width 40, spanning top to bottom), and expose the
/// placement in output coordinates once the client maps a buffer.
#[test]
fn layer_surface_bar_gets_initial_configure_and_anchored_geometry() {
    roundtrip_test(|client, control| {
        spawn_mapped_output(control, (1280, 768));
        client.roundtrip();
        let surface = client.surface();
        let layer_shell = client.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1>(4);
        let output = client.bind::<wl_output::WlOutput>(4);
        let bar = layer_shell.get_layer_surface(
            &surface,
            Some(&output),
            zwlr_layer_shell_v1::Layer::Top,
            "bar".to_string(),
            &client.queue.handle(),
            (),
        );
        bar.set_anchor(
            zwlr_layer_surface_v1::Anchor::Top
                | zwlr_layer_surface_v1::Anchor::Bottom
                | zwlr_layer_surface_v1::Anchor::Right,
        );
        bar.set_size(40, 0);
        bar.set_exclusive_zone(0);
        // First commit: no buffer, the state-only commit that the spec
        // requires the compositor to answer with the initial configure.
        surface.commit();
        client.roundtrip();
        let configure = *client
            .events
            .layer_configures
            .first()
            .expect("initial configure missing");
        assert_eq!((configure.1, configure.2), (40, 768));

        // Map: acknowledge and attach a buffer; the map must hold the
        // surface with output-local anchored geometry.
        bar.ack_configure(configure.0);
        let buffer = client.buffer(40, 768);
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        client.roundtrip();
        let geometry = mapped_layer_geometry(control, "bar").expect("bar not mapped");
        assert_eq!(geometry.loc, (1240, 0).into());
        assert_eq!(geometry.size, (40, 768).into());
        assert!(control.call(|state, _| { state.surface_under((1260.0, 20.0).into()).is_some() }));
        assert!(!control.call(|state, _| { state.surface_under((100.0, 20.0).into()).is_some() }));
        bar.destroy();
        client.roundtrip();
        assert_eq!(control.call(|state, _| state.mapped_layers()), 0);
        assert_eq!(control.call(|state, _| state.layer_surfaces.len()), 0);
    });
}

/// Wallpaper: a background layer anchored to all corners with an exclusive
/// zone of -1 spans the whole output.
#[test]
fn layer_surface_wallpaper_spans_the_output() {
    roundtrip_test(|client, control| {
        spawn_mapped_output(control, (1280, 768));
        client.roundtrip();
        let surface = client.surface();
        let layer_shell = client.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1>(4);
        let output = client.bind::<wl_output::WlOutput>(4);
        let wallpaper = layer_shell.get_layer_surface(
            &surface,
            Some(&output),
            zwlr_layer_shell_v1::Layer::Background,
            "wallpaper".to_string(),
            &client.queue.handle(),
            (),
        );
        wallpaper.set_anchor(zwlr_layer_surface_v1::Anchor::all());
        wallpaper.set_exclusive_zone(-1);
        surface.commit();
        client.roundtrip();
        let configure = *client
            .events
            .layer_configures
            .first()
            .expect("initial configure missing");
        assert_eq!((configure.1, configure.2), (1280, 768));
        wallpaper.ack_configure(configure.0);
        let buffer = client.buffer(1280, 768);
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        client.roundtrip();
        let geometry = mapped_layer_geometry(control, "wallpaper").expect("wallpaper not mapped");
        assert_eq!(geometry.loc, (0, 0).into());
        assert_eq!(geometry.size, (1280, 768).into());
        wallpaper.destroy();
    });
}

/// A layer surface that commits an empty surface unmaps and stops reserving
/// space; its next mapped commit restores it. Unchanged state never produces
/// additional configure events.
#[test]
fn layer_surface_unmaps_and_remaps_with_buffer_presence() {
    roundtrip_test(|client, control| {
        spawn_mapped_output(control, (1280, 768));
        client.roundtrip();
        let surface = client.surface();
        let layer_shell = client.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1>(4);
        let output = client.bind::<wl_output::WlOutput>(4);
        let bar = layer_shell.get_layer_surface(
            &surface,
            Some(&output),
            zwlr_layer_shell_v1::Layer::Top,
            "bar".to_string(),
            &client.queue.handle(),
            (),
        );
        bar.set_anchor(
            zwlr_layer_surface_v1::Anchor::Top
                | zwlr_layer_surface_v1::Anchor::Bottom
                | zwlr_layer_surface_v1::Anchor::Right,
        );
        bar.set_size(40, 0);
        bar.set_exclusive_zone(0);
        surface.commit();
        client.roundtrip();
        let configure = *client
            .events
            .layer_configures
            .first()
            .expect("initial configure missing");
        bar.ack_configure(configure.0);
        let buffer = client.buffer(40, 768);
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        client.roundtrip();
        assert!(mapped_layer_geometry(control, "bar").is_some());

        // Shell hides the bar: empty commit detaches the buffer.
        surface.attach(None, 0, 0);
        surface.commit();
        client.roundtrip();
        assert!(mapped_layer_geometry(control, "bar").is_none());

        // Shell shows the bar again: re-attach restores it without another
        // configure.
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        client.roundtrip();
        let geometry = mapped_layer_geometry(control, "bar").expect("bar not remapped");
        assert_eq!(geometry.loc, (1240, 0).into());
        assert_eq!(geometry.size, (40, 768).into());
        assert_eq!(
            client.events.layer_configures.len(),
            1,
            "no spurious configures for unchanged state"
        );
        bar.destroy();
    });
}
