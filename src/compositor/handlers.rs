// Wayland protocol plumbing is adapted from Smithay's MIT-licensed Smallvil example.
use smithay::{
    backend::renderer::utils::{on_commit_buffer_handler, with_renderer_surface_state},
    delegate_compositor, delegate_data_device, delegate_output, delegate_primary_selection,
    delegate_seat, delegate_shm, delegate_xdg_decoration, delegate_xdg_shell,
    desktop::{
        PopupKeyboardGrab, PopupKind, PopupPointerGrab, find_popup_root_surface,
        get_popup_toplevel_coords, layer_map_for_output,
    },
    input::{
        Seat, SeatHandler, SeatState,
        pointer::{CursorImageStatus, Focus, GrabStartData},
    },
    output::Output,
    reexports::{
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
            shell::server::xdg_toplevel::ResizeEdge,
        },
        wayland_server::{
            Client, Resource,
            protocol::{
                wl_buffer::WlBuffer, wl_output::WlOutput, wl_seat::WlSeat, wl_surface::WlSurface,
            },
        },
    },
    utils::{SERIAL_COUNTER, Serial},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent,
            is_sync_subsurface, with_states,
        },
        output::OutputHandler,
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                set_data_device_focus,
            },
            primary_selection::{
                PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
            },
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData, decoration::XdgDecorationHandler,
        },
        shm::{ShmHandler, ShmState},
    },
};

use super::{ClientState, Wisp};

impl CompositorHandler for Wisp {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(data) = client.get_data::<ClientState>() {
            &data.compositor_state
        } else {
            &client
                .get_data::<smithay::xwayland::XWaylandClientData>()
                .expect("Wayland client has no compositor state")
                .compositor_state
        }
    }

    fn commit(&mut self, surface: &WlSurface) {
        self.stats.client_commits += 1;
        on_commit_buffer_handler::<Self>(surface);
        let synchronized = is_sync_subsurface(surface);
        // Unknown surfaces (including popups and cursor surfaces) retain the
        // existing conservative redraw behavior. Only suppress commits that
        // we can positively identify as hidden/unmapped xdg toplevel trees.
        // A synchronized subsurface cannot change the scene until its parent
        // commits, so its own commit does not need a frame either.
        let mut redraw = !synchronized;
        if !synchronized {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(index) = self.windows.iter().position(|w| {
                w.window
                    .toplevel()
                    .is_some_and(|toplevel| toplevel.wl_surface() == &root)
            }) {
                self.windows[index].window.on_commit();
                let on_workspace = !self.windows[index].scratchpad_hidden
                    && (self.windows[index].workspace == self.active_workspace
                        || self
                            .workspace_slide
                            .as_ref()
                            .is_some_and(|slide| self.windows[index].workspace == slide.from));
                redraw = on_workspace && self.windows[index].mapped;
                if &root == surface {
                    let toplevel = self.windows[index].window.toplevel().unwrap().clone();
                    if !toplevel.is_initial_configure_sent() {
                        toplevel.send_configure();
                    }
                    let mapped = with_renderer_surface_state(surface, |s| s.buffer().is_some())
                        .unwrap_or(false);
                    let was_mapped = self.windows[index].mapped;
                    self.windows[index].mapped = mapped;
                    if !mapped {
                        self.windows[index].fade = None;
                    }
                    redraw = on_workspace && (mapped || was_mapped);
                    if mapped && !was_mapped {
                        if self.config.layout.window_open_fade {
                            self.windows[index].fade = Some(super::window_fade::WindowFade::new(
                                std::time::Instant::now(),
                                self.config.layout.window_open_fade_ms,
                            ));
                        }
                        self.windows[index].participating = true;
                        if !self.windows[index].rules_applied {
                            let parent = with_states(surface, |s| {
                                s.data_map
                                    .get::<XdgToplevelSurfaceData>()
                                    .unwrap()
                                    .lock()
                                    .unwrap()
                                    .parent
                                    .is_some()
                            });
                            if parent {
                                self.windows[index].floating = true;
                            }
                            self.apply_rules(index);
                        }
                        // Rules may move a newly mapped window to another
                        // workspace. Check visibility *after* applying them.
                        let now_on_workspace = self.windows[index].workspace
                            == self.active_workspace
                            && !self.windows[index].scratchpad_hidden;
                        redraw = now_on_workspace || on_workspace != now_on_workspace;
                        if redraw {
                            // A rule moving this window away also needs an
                            // arrange to remove its initial space mapping.
                            self.arrange();
                        }
                        if now_on_workspace {
                            let id = self.windows[index].id;
                            self.focus(Some(id));
                        }
                    } else if !mapped && was_mapped {
                        self.windows[index].participating = false;
                        let id = self.windows[index].id;
                        if on_workspace {
                            self.arrange();
                        }
                        if self.focused == Some(id) {
                            self.focus_fallback();
                        }
                    }
                    if mapped != was_mapped {
                        self.ext_workspace_notify();
                    }
                }
            } else if let Some(index) = self.windows.iter().position(|w| {
                w.window
                    .x11_surface()
                    .and_then(|x11| x11.wl_surface())
                    .as_ref()
                    == Some(&root)
            }) {
                self.windows[index].window.on_commit();
                let mapped = self.windows[index]
                    .window
                    .x11_surface()
                    .is_some_and(|x11| x11.is_mapped())
                    && with_renderer_surface_state(&root, |s| s.buffer().is_some())
                        .unwrap_or(false);
                let was_mapped = self.windows[index].mapped;
                self.windows[index].mapped = mapped;
                self.windows[index].participating = mapped;
                redraw = mapped || was_mapped;
                if mapped && !was_mapped {
                    self.apply_rules(index);
                    self.arrange();
                    let id = self.windows[index].id;
                    if self.windows[index].workspace == self.active_workspace {
                        self.focus(Some(id));
                    }
                } else if !mapped && was_mapped {
                    self.arrange();
                    if self.focused == Some(self.windows[index].id) {
                        self.focus_fallback();
                    }
                }
                if mapped != was_mapped {
                    self.ext_workspace_notify();
                }
            } else if let Some(window) = self.x11_unmanaged.iter().find(|window| {
                window
                    .x11_surface()
                    .and_then(|x11| x11.wl_surface())
                    .as_ref()
                    == Some(&root)
            }) {
                window.on_commit();
                redraw = true;
            }
        }
        self.popups.commit(surface);
        if self
            .lock_surface()
            .is_some_and(|lock| lock.wl_surface() == surface)
        {
            self.lock_focus();
            redraw = true;
        }
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface) {
            if !popup.is_initial_configure_sent() {
                let _ = popup.send_configure();
            }
        }
        // Layer surfaces: react to the client's commits with the geometry the
        // layer map computed from anchors, margins and exclusive zones —
        // including the mandatory initial configure in response to the
        // surface's first commit, without which wlr-layer-shell clients may
        // never attach a buffer. map_layer/unmap_layer/arrange keep stacking
        // and zones consistent; cleanup() prunes surfaces their client freed.
        let layer = self
            .layer_surfaces
            .iter()
            .find(|layer| layer.wl_surface() == surface)
            .cloned();
        if let Some(layer) = layer {
            if let Some(output) = self.output.clone() {
                let mapped =
                    with_renderer_surface_state(surface, |states| states.buffer().is_some())
                        .unwrap_or(false);
                {
                    let mut map = layer_map_for_output(&output);
                    let in_map = map.layers().any(|entry| entry == &layer);
                    // Arrange while the surface is still in the map so the
                    // freshly committed anchors, margins and sizes flow into
                    // the pending configure shipped below.
                    if in_map {
                        map.arrange();
                    }
                    match (mapped, in_map) {
                        (true, false) => {
                            let _ = map.map_layer(&layer);
                        }
                        (false, true) => {
                            map.unmap_layer(&layer);
                        }
                        _ => {}
                    }
                }
                // map_layer and arrange() send follow-up configures on their
                // own after the initial configure exists; the initial one is
                // the compositor's responsibility, and send_pending_configure
                // covers it because the initial configure still counts as a
                // pending change. Later no-op commits send nothing.
                let _ = layer.layer_surface().send_pending_configure();
            }
        }
        if !redraw {
            self.stats.skipped_invisible_commits += 1;
        }
        self.dirty |= redraw;
    }
}

impl BufferHandler for Wisp {
    fn buffer_destroyed(&mut self, buffer: &WlBuffer) {
        self.toplevel_icon_state.buffer_destroyed(buffer);
    }
}
impl ShmHandler for Wisp {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
delegate_compositor!(Wisp);
delegate_shm!(Wisp);

impl SeatHandler for Wisp {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
    fn cursor_image(&mut self, _: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        self.dirty = true;
    }
    fn led_state_changed(&mut self, _: &Seat<Self>, led_state: smithay::input::keyboard::LedState) {
        self.pending_led_state = Some(led_state);
    }
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        use smithay::input::keyboard::KeyboardTarget;
        let next = focused
            .filter(|_| self.session_lock.is_none())
            .and_then(|surface| {
                self.windows
                    .iter()
                    .filter_map(|entry| entry.window.x11_surface())
                    .find(|x11| x11.wl_surface().as_ref() == Some(surface))
                    .cloned()
            });
        if self.x11_focus.as_ref().map(|x11| x11.window_id())
            != next.as_ref().map(|x11| x11.window_id())
        {
            let serial = SERIAL_COUNTER.next_serial();
            if let Some(previous) = self.x11_focus.take() {
                previous.leave(seat, self, serial);
            }
            if let Some(next) = next {
                // The Wayland wl_surface is the seat focus for key delivery;
                // X11 also needs its own input-focus/WM_TAKE_FOCUS transition.
                next.enter(seat, self, Vec::new(), serial);
                self.x11_focus = Some(next);
            }
        }
        // A lock client has no need for the desktop clipboard. Keep both X11
        // and Wayland selection offers away from it until confirmed unlock.
        let client = focused
            .filter(|_| self.session_lock.is_none())
            .and_then(|s| self.display_handle.get_client(s.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client.clone());
        set_primary_focus(&self.display_handle, seat, client);
    }
}
delegate_seat!(Wisp);

impl SelectionHandler for Wisp {
    type SelectionUserData = super::xwayland::X11Selection;

    fn new_selection(
        &mut self,
        ty: smithay::wayland::selection::SelectionTarget,
        source: Option<smithay::wayland::selection::SelectionSource>,
        _: Seat<Self>,
    ) {
        self.x11_selection_changed(ty, source);
    }

    fn send_selection(
        &mut self,
        ty: smithay::wayland::selection::SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
        _: Seat<Self>,
        user_data: &Self::SelectionUserData,
    ) {
        self.x11_send_selection(ty, mime_type, fd, *user_data);
    }
}
impl DataDeviceHandler for Wisp {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}
impl ClientDndGrabHandler for Wisp {}
impl ServerDndGrabHandler for Wisp {}
delegate_data_device!(Wisp);
impl PrimarySelectionHandler for Wisp {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}
delegate_primary_selection!(Wisp);
impl OutputHandler for Wisp {
    fn output_bound(&mut self, _output: Output, wl_output: WlOutput) {
        // New client-side output instances update workspace group membership
        // for the ext-workspace protocol.
        self.ext_workspace_output_bound(wl_output);
    }
}
delegate_output!(Wisp);

impl XdgShellHandler for Wisp {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.add_window(surface);
    }
    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.remove_window(surface.wl_surface());
    }
    fn new_popup(&mut self, surface: PopupSurface, _: PositionerState) {
        if self.session_lock.is_some() {
            return;
        }
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }
    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }
    fn move_request(&mut self, surface: ToplevelSurface, seat: WlSeat, serial: Serial) {
        if let Some(start) = self.check_grab(&seat, surface.wl_surface(), serial) {
            if let Some(id) = self
                .windows
                .iter()
                .find(|w| w.window.toplevel() == Some(&surface))
                .map(|w| w.id)
            {
                self.start_interactive(id, start, serial, None);
            }
        }
    }
    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: WlSeat,
        serial: Serial,
        edges: ResizeEdge,
    ) {
        if edges == ResizeEdge::None {
            return;
        }
        if let Some(start) = self.check_grab(&seat, surface.wl_surface(), serial) {
            if let Some(id) = self
                .windows
                .iter()
                .find(|w| w.window.toplevel() == Some(&surface))
                .map(|w| w.id)
            {
                self.start_interactive(id, start, serial, Some(edges));
            }
        }
    }
    fn grab(&mut self, surface: PopupSurface, seat: WlSeat, serial: Serial) {
        if self.session_lock.is_some() {
            return;
        }
        let Some(seat) = Seat::<Self>::from_resource(&seat) else {
            return;
        };
        if seat != self.seat {
            return;
        }
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        let Ok(grab) = self.popups.grab_popup(root, kind, &seat, serial) else {
            return;
        };
        let keyboard = seat.get_keyboard().unwrap();
        let pointer = seat.get_pointer().unwrap();
        let previous = grab.previous_serial().unwrap_or(serial);
        if (keyboard.is_grabbed() && !keyboard.has_grab(previous))
            || (pointer.is_grabbed() && !pointer.has_grab(previous))
        {
            return;
        }
        keyboard.set_focus(self, grab.current_grab(), SERIAL_COUNTER.next_serial());
        keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
    }
    fn fullscreen_request(&mut self, surface: ToplevelSurface, _: Option<WlOutput>) {
        self.client_fullscreen(&surface, true);
    }
    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.client_fullscreen(&surface, false);
    }
    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.client_fullscreen(&surface, true);
    }
    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.client_fullscreen(&surface, false);
    }
}
delegate_xdg_shell!(Wisp);

impl XdgDecorationHandler for Wisp {
    fn new_decoration(&mut self, surface: ToplevelSurface) {
        self.server_decoration(surface);
    }
    fn request_mode(&mut self, surface: ToplevelSurface, _: Mode) {
        self.server_decoration(surface);
    }
    fn unset_mode(&mut self, surface: ToplevelSurface) {
        self.server_decoration(surface);
    }
}
delegate_xdg_decoration!(Wisp);

impl Wisp {
    fn server_decoration(&self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| state.decoration_mode = Some(Mode::ServerSide));
        if surface.is_initial_configure_sent() {
            surface.send_pending_configure();
        }
    }

    fn client_fullscreen(&mut self, surface: &ToplevelSurface, fullscreen: bool) {
        let Some(index) = self
            .windows
            .iter()
            .position(|w| w.window.toplevel() == Some(surface))
        else {
            return;
        };
        let workspace = self.windows[index].workspace;
        if fullscreen {
            for entry in &mut self.windows {
                if entry.workspace == workspace {
                    entry.fullscreen = false;
                }
            }
        }
        self.windows[index].fullscreen = fullscreen;
        let id = self.windows[index].id;
        self.arrange();
        if workspace == self.active_workspace {
            self.focus(Some(id));
        }
    }

    fn check_grab(
        &self,
        seat: &WlSeat,
        surface: &WlSurface,
        serial: Serial,
    ) -> Option<GrabStartData<Self>> {
        let seat = Seat::<Self>::from_resource(seat)?;
        if seat != self.seat {
            return None;
        }
        let pointer = seat.get_pointer()?;
        if !pointer.has_grab(serial) {
            return None;
        }
        let start = pointer.grab_start_data()?;
        if !start.focus.as_ref()?.0.id().same_client_as(&surface.id()) {
            return None;
        }
        Some(start)
    }

    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self.space.elements().find(|w| {
            w.toplevel()
                .is_some_and(|toplevel| toplevel.wl_surface() == &root)
        }) else {
            return;
        };
        let Some(output) = self.output.as_ref() else {
            return;
        };
        let Some(mut target) = self.space.output_geometry(output) else {
            return;
        };
        let Some(window_geometry) = self.space.element_geometry(window) else {
            return;
        };
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geometry.loc;
        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target)
        });
    }
}
