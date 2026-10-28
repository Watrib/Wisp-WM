//! Experimental, opt-in XWayland lifecycle and initial managed-window bridge.
//! Clipboard and primary-selection bridging are experimental; the bridge
//! closes its transfer requests and suppresses offers while a lock is active.
//! Both directions and lock isolation still need real-client validation.

use std::{os::unix::io::OwnedFd, process::Stdio};

use anyhow::{Context, Result};
use smithay::{
    desktop::Window,
    reexports::{calloop::LoopHandle, wayland_server::protocol::wl_surface::WlSurface},
    utils::{Logical, Rectangle},
    wayland::{
        selection::{
            SelectionTarget,
            data_device::{
                clear_data_device_selection, current_data_device_selection_userdata,
                request_data_device_client_selection, set_data_device_selection,
            },
            primary_selection::{
                clear_primary_selection, current_primary_selection_userdata,
                request_primary_client_selection, set_primary_selection,
            },
        },
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
        xwm::{Reorder, ResizeEdge, X11Window, XwmId},
    },
};
use tracing::{info, warn};

use super::{LoopData, ManagedWindow, Rect, Wisp, effects};
use smithay::backend::renderer::{
    element::{Id, solid::SolidColorBuffer},
    utils::{CommitCounter, with_renderer_surface_state},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X11Selection {
    Owner(XwmId),
}

pub(super) enum PendingSelection {
    Wayland(Option<Vec<String>>),
    X11(XwmId, Vec<String>),
    ClearedX11,
}

fn selection_index(ty: SelectionTarget) -> usize {
    match ty {
        SelectionTarget::Clipboard => 0,
        SelectionTarget::Primary => 1,
    }
}

pub(super) fn start(handle: &LoopHandle<'static, LoopData>, data: &mut LoopData) -> Result<()> {
    data.state.x11_loop_handle = Some(handle.clone());
    let (xwayland, client) = XWayland::spawn(
        &data.state.display_handle,
        None,
        std::iter::empty::<(String, String)>(),
        true,
        Stdio::null(),
        Stdio::null(),
        |_| (),
    )
    .context("starting opt-in XWayland")?;
    let x_display = format!(":{}", xwayland.display_number());
    let wm_handle = handle.clone();
    handle
        .insert_source(xwayland, move |event, _, data| match event {
            XWaylandEvent::Ready { x11_socket, .. } => {
                match X11Wm::start_wm(wm_handle.clone(), x11_socket, client.clone()) {
                    Ok(wm) => {
                        info!(%x_display, "experimental XWayland window manager ready");
                        data.state.x11_wm = Some(wm);
                        data.state.x_display = Some(x_display.clone());
                        if let Some(command) = data.deferred_command.take() {
                            if !command.is_empty() {
                                if let Err(error) = data.state.spawn(&command) {
                                    data.state.fail(
                                        error.context("starting initial command after XWM ready"),
                                    );
                                    return;
                                }
                            }
                            data.state.start_autostart();
                        }
                    }
                    Err(error) => data
                        .state
                        .fail(anyhow::anyhow!("starting X11 window manager: {error}")),
                }
            }
            XWaylandEvent::Error => {
                data.state.stop_xwayland();
                warn!("XWayland exited; new X11 clients are disabled");
            }
        })
        .map_err(|error| error.error)
        .context("registering XWayland event source")?;
    Ok(())
}

impl Wisp {
    pub(super) fn x11_selection_changed(
        &mut self,
        ty: SelectionTarget,
        source: Option<smithay::wayland::selection::SelectionSource>,
    ) {
        let mime_types = source.map(|source| source.mime_types());
        if self.session_lock.is_some() {
            self.x11_pending_selections[selection_index(ty)] =
                Some(PendingSelection::Wayland(mime_types));
            return;
        }
        let Some(wm) = self.x11_wm.as_mut() else {
            return;
        };
        // Do not offer the desktop clipboard to X11 while a locker owns the
        // seat. Never call back into XWM from an X11-originated selection: the
        // compositor-provided source is installed directly, not via this hook.
        if let Err(error) = wm.new_selection(ty, mime_types) {
            warn!(%error, "updating X11 selection from Wayland");
        }
    }

    pub(super) fn restore_x11_selections(&mut self) {
        // Defense in depth: both callers clear `session_lock` first, but an
        // accidental restore while locked would offer desktop selections.
        if self.session_lock.is_some() {
            return;
        }
        for ty in [SelectionTarget::Clipboard, SelectionTarget::Primary] {
            let Some(pending) = self.x11_pending_selections[selection_index(ty)].take() else {
                continue;
            };
            match pending {
                PendingSelection::Wayland(mime_types) => {
                    if let Some(wm) = self.x11_wm.as_mut() {
                        if let Err(error) = wm.new_selection(ty, mime_types) {
                            warn!(%error, "restoring Wayland selection in X11 after unlock");
                        }
                    }
                }
                PendingSelection::X11(xwm, mime_types) => {
                    if self.x11_wm.as_ref().is_some_and(|wm| wm.id() == xwm) {
                        match ty {
                            SelectionTarget::Clipboard => set_data_device_selection(
                                &self.display_handle,
                                &self.seat,
                                mime_types,
                                X11Selection::Owner(xwm),
                            ),
                            SelectionTarget::Primary => set_primary_selection(
                                &self.display_handle,
                                &self.seat,
                                mime_types,
                                X11Selection::Owner(xwm),
                            ),
                        }
                    }
                }
                PendingSelection::ClearedX11 => {
                    self.clear_x11_selection(ty, None);
                }
            }
        }
    }

    fn clear_x11_selection(&mut self, ty: SelectionTarget, owner: Option<XwmId>) {
        match ty {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(&self.seat)
                    .is_some_and(|data| owner.is_none_or(|id| *data == X11Selection::Owner(id)))
                {
                    clear_data_device_selection(&self.display_handle, &self.seat);
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat)
                    .is_some_and(|data| owner.is_none_or(|id| *data == X11Selection::Owner(id)))
                {
                    clear_primary_selection(&self.display_handle, &self.seat);
                }
            }
        }
    }

    pub(super) fn x11_send_selection(
        &mut self,
        ty: SelectionTarget,
        mime: String,
        fd: OwnedFd,
        owner: X11Selection,
    ) {
        let X11Selection::Owner(id) = owner;
        if self.session_lock.is_some() {
            return;
        }
        let Some(wm) = self.x11_wm.as_mut().filter(|wm| wm.id() == id) else {
            return;
        };
        let Some(handle) = self.x11_loop_handle.as_ref() else {
            return;
        };
        if let Err(error) = wm.send_selection(ty, mime, fd, handle.clone()) {
            warn!(%error, "transferring X11 clipboard to Wayland");
        }
    }

    fn stop_xwayland(&mut self) {
        self.x_display = None;
        self.x11_focus = None;
        self.x11_loop_handle = None;
        self.x11_pending_selections = [None, None];
        let seat = self.seat.clone();
        let dh = self.display_handle.clone();
        if current_data_device_selection_userdata(&seat).is_some() {
            clear_data_device_selection(&dh, &seat);
        }
        if current_primary_selection_userdata(&seat).is_some() {
            clear_primary_selection(&dh, &seat);
        }
        for window in std::mem::take(&mut self.x11_unmanaged) {
            self.space.unmap_elem(&window);
        }
        let windows: Vec<_> = self
            .windows
            .iter()
            .filter_map(|entry| entry.window.x11_surface().cloned())
            .collect();
        for window in windows {
            self.unregister_x11_window(&window);
        }
        self.x11_wm = None;
    }

    fn register_x11_window(&mut self, surface: X11Surface) {
        if surface.is_override_redirect() {
            self.register_x11_popup(surface);
            return;
        }
        if !surface.is_mapped()
            || surface.wl_surface().is_none()
            || self.windows.iter().any(|entry| {
                entry
                    .window
                    .x11_surface()
                    .is_some_and(|x11| x11.window_id() == surface.window_id())
            })
        {
            return;
        }
        // XWayland's X11 map and wl_surface commit can arrive in either order.
        // A surface that was already committed before serial association may
        // never commit again, so register its current buffer immediately.
        let mapped = x11_buffer_committed(&surface);
        let area = self.viewport();
        let width = surface.geometry().size.w.max(1).min(area.width);
        let height = surface.geometry().size.h.max(1).min(area.height);
        let rect = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let window = Window::new_x11_window(surface);
        if mapped {
            window.on_commit();
        }
        self.windows.push(ManagedWindow {
            id: self.next_id,
            window,
            workspace: self.active_workspace,
            scratchpad_hidden: false,
            floating: false,
            fullscreen: false,
            urgent: false,
            mapped,
            participating: mapped,
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
        if mapped {
            let index = self.windows.len() - 1;
            self.apply_rules(index);
            self.arrange();
            if self.windows[index].workspace == self.active_workspace {
                let id = self.windows[index].id;
                self.focus(Some(id));
            }
            self.ext_workspace_notify();
        }
        self.dirty = true;
    }

    fn unregister_x11_window(&mut self, surface: &X11Surface) {
        if let Some(index) = self.x11_unmanaged.iter().position(|window| {
            window
                .x11_surface()
                .is_some_and(|x11| x11.window_id() == surface.window_id())
        }) {
            let window = self.x11_unmanaged.remove(index);
            self.space.unmap_elem(&window);
            self.dirty = true;
            self.refresh_pointer_focus();
            return;
        }
        let Some(index) = self.windows.iter().position(|entry| {
            entry
                .window
                .x11_surface()
                .is_some_and(|x11| x11.window_id() == surface.window_id())
        }) else {
            return;
        };
        let entry = self.windows.remove(index);
        self.space.unmap_elem(&entry.window);
        if self.scratchpad == Some(entry.id) {
            self.scratchpad = None;
        }
        for remembered in &mut self.workspace_focus {
            if *remembered == Some(entry.id) {
                *remembered = None;
            }
        }
        self.arrange();
        if self.focused == Some(entry.id) {
            self.focus_fallback();
        }
        self.ext_workspace_notify();
        self.dirty = true;
    }

    fn register_x11_popup(&mut self, surface: X11Surface) {
        if !surface.is_mapped()
            || surface.wl_surface().is_none()
            || self.x11_unmanaged.iter().any(|window| {
                window
                    .x11_surface()
                    .is_some_and(|x11| x11.window_id() == surface.window_id())
            })
        {
            return;
        }
        let geometry = surface.geometry();
        let buffered = x11_buffer_committed(&surface);
        let window = Window::new_x11_window(surface);
        if buffered {
            window.on_commit();
        }
        window.override_z_index(40);
        self.space.map_element(window.clone(), geometry.loc, true);
        self.x11_unmanaged.push(window);
        self.dirty = true;
    }
}

fn x11_buffer_committed(surface: &X11Surface) -> bool {
    surface.wl_surface().is_some_and(|wl_surface| {
        with_renderer_surface_state(&wl_surface, |state| state.buffer().is_some()).unwrap_or(false)
    })
}

impl XWaylandShellHandler for Wisp {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        self.xwayland_shell_state
            .as_mut()
            .expect("XWayland shell is disabled")
    }

    fn surface_associated(&mut self, _: XwmId, _: WlSurface, surface: X11Surface) {
        self.register_x11_window(surface);
    }
}

impl XwmHandler for Wisp {
    fn xwm_state(&mut self, xwm: XwmId) -> &mut X11Wm {
        let wm = self.x11_wm.as_mut().expect("X11 WM event before ready");
        assert_eq!(wm.id(), xwm, "unexpected X11 WM ID");
        wm
    }
    fn new_window(&mut self, _: XwmId, _: X11Surface) {}
    fn new_override_redirect_window(&mut self, _: XwmId, _: X11Surface) {}
    fn map_window_request(&mut self, _: XwmId, surface: X11Surface) {
        if let Err(error) = surface.set_mapped(true) {
            warn!(%error, "mapping X11 window");
        }
        self.register_x11_window(surface);
    }
    fn mapped_override_redirect_window(&mut self, _: XwmId, surface: X11Surface) {
        self.register_x11_popup(surface);
    }
    fn unmapped_window(&mut self, _: XwmId, surface: X11Surface) {
        self.unregister_x11_window(&surface);
    }
    fn destroyed_window(&mut self, _: XwmId, surface: X11Surface) {
        self.unregister_x11_window(&surface);
    }
    fn configure_request(
        &mut self,
        _: XwmId,
        surface: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _: Option<Reorder>,
    ) {
        let mut geometry = surface.geometry();
        if let Some(x) = x {
            geometry.loc.x = x;
        }
        if let Some(y) = y {
            geometry.loc.y = y;
        }
        if let Some(w) = w {
            geometry.size.w = w.min(i32::MAX as u32) as i32;
        }
        if let Some(h) = h {
            geometry.size.h = h.min(i32::MAX as u32) as i32;
        }
        geometry.size.w = geometry.size.w.max(1);
        geometry.size.h = geometry.size.h.max(1);
        if let Err(error) = surface.configure(geometry) {
            warn!(%error, "X11 configure request");
        }
    }
    fn configure_notify(
        &mut self,
        _: XwmId,
        surface: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _: Option<X11Window>,
    ) {
        if let Some(window) = self.x11_unmanaged.iter().find(|window| {
            window
                .x11_surface()
                .is_some_and(|x11| x11.window_id() == surface.window_id())
        }) {
            self.space.map_element(window.clone(), geometry.loc, true);
            self.dirty = true;
        }
    }
    fn resize_request(&mut self, _: XwmId, _: X11Surface, _: u32, _: ResizeEdge) {}
    fn move_request(&mut self, _: XwmId, _: X11Surface, _: u32) {}
    fn allow_selection_access(&mut self, _: XwmId, _: SelectionTarget) -> bool {
        self.session_lock.is_none()
    }
    fn send_selection(&mut self, _: XwmId, ty: SelectionTarget, mime: String, fd: OwnedFd) {
        if self.session_lock.is_some() {
            return;
        }
        let result = match ty {
            SelectionTarget::Clipboard => {
                request_data_device_client_selection(&self.seat, mime, fd)
                    .map_err(|error| error.to_string())
            }
            SelectionTarget::Primary => request_primary_client_selection(&self.seat, mime, fd)
                .map_err(|error| error.to_string()),
        };
        if let Err(error) = result {
            warn!(%error, "X11 requested unavailable Wayland selection");
        }
    }
    fn new_selection(&mut self, xwm: XwmId, ty: SelectionTarget, mime_types: Vec<String>) {
        if self.session_lock.is_some() {
            self.x11_pending_selections[selection_index(ty)] =
                Some(PendingSelection::X11(xwm, mime_types));
            return;
        }
        match ty {
            SelectionTarget::Clipboard => set_data_device_selection(
                &self.display_handle,
                &self.seat,
                mime_types,
                X11Selection::Owner(xwm),
            ),
            SelectionTarget::Primary => set_primary_selection(
                &self.display_handle,
                &self.seat,
                mime_types,
                X11Selection::Owner(xwm),
            ),
        }
    }
    fn cleared_selection(&mut self, xwm: XwmId, ty: SelectionTarget) {
        if self.session_lock.is_some() {
            self.x11_pending_selections[selection_index(ty)] = Some(PendingSelection::ClearedX11);
        }
        self.clear_x11_selection(ty, Some(xwm));
    }
    fn disconnected(&mut self, _: XwmId) {
        self.stop_xwayland();
        warn!("X11 window manager disconnected; X11 clients disabled");
    }
}

impl XWaylandShellHandler for LoopData {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        self.state.xwayland_shell_state()
    }
    fn surface_associated(&mut self, xwm: XwmId, surface: WlSurface, window: X11Surface) {
        self.state.surface_associated(xwm, surface, window);
    }
}

impl XwmHandler for LoopData {
    fn xwm_state(&mut self, xwm: XwmId) -> &mut X11Wm {
        self.state.xwm_state(xwm)
    }
    fn new_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.new_window(xwm, window);
    }
    fn new_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.new_override_redirect_window(xwm, window);
    }
    fn map_window_request(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.map_window_request(xwm, window);
    }
    fn mapped_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.mapped_override_redirect_window(xwm, window);
    }
    fn unmapped_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.unmapped_window(xwm, window);
    }
    fn destroyed_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.destroyed_window(xwm, window);
    }
    fn configure_request(
        &mut self,
        xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        reorder: Option<Reorder>,
    ) {
        self.state
            .configure_request(xwm, window, x, y, w, h, reorder);
    }
    fn configure_notify(
        &mut self,
        xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        above: Option<X11Window>,
    ) {
        self.state.configure_notify(xwm, window, geometry, above);
    }
    fn resize_request(&mut self, xwm: XwmId, window: X11Surface, button: u32, edge: ResizeEdge) {
        self.state.resize_request(xwm, window, button, edge);
    }
    fn move_request(&mut self, xwm: XwmId, window: X11Surface, button: u32) {
        self.state.move_request(xwm, window, button);
    }
    fn allow_selection_access(&mut self, xwm: XwmId, selection: SelectionTarget) -> bool {
        self.state.allow_selection_access(xwm, selection)
    }
    fn send_selection(
        &mut self,
        xwm: XwmId,
        selection: SelectionTarget,
        mime: String,
        fd: OwnedFd,
    ) {
        self.state.send_selection(xwm, selection, mime, fd);
    }
    fn new_selection(&mut self, xwm: XwmId, ty: SelectionTarget, mime_types: Vec<String>) {
        self.state.new_selection(xwm, ty, mime_types);
    }
    fn cleared_selection(&mut self, xwm: XwmId, ty: SelectionTarget) {
        self.state.cleared_selection(xwm, ty);
    }
    fn disconnected(&mut self, xwm: XwmId) {
        self.state.disconnected(xwm);
    }
}

smithay::delegate_xwayland_shell!(Wisp);
