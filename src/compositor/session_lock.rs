//! Experimental ext-session-lock integration. Never rely on the handler's
//! `unlock` callback alone: Smithay 0.7 calls it after `invalid_unlock` on an
//! unconfirmed lock. Check the exact resource BEFORE forwarding that request.

use smithay::{
    backend::renderer::utils::with_renderer_surface_state,
    desktop::utils::under_from_surface_tree,
    output::Output,
    reexports::{
        wayland_protocols::ext::session_lock::v1::server::{
            ext_session_lock_manager_v1::ExtSessionLockManagerV1,
            ext_session_lock_surface_v1::ExtSessionLockSurfaceV1,
            ext_session_lock_v1::{self, ExtSessionLockV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, Resource, protocol::wl_output::WlOutput,
        },
    },
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::session_lock::{
        ExtLockSurfaceUserData, LockSurface, SessionLockHandler, SessionLockManagerGlobalData,
        SessionLockManagerState, SessionLockState, SessionLocker,
    },
};

use super::Wisp;

pub(super) struct LockedSession {
    resource: ExtSessionLockV1,
    confirmation: Option<SessionLocker>,
    surface: Option<LockSurface>,
}

impl Wisp {
    pub(super) fn lock_surface(&self) -> Option<&LockSurface> {
        self.session_lock
            .as_ref()
            .and_then(|lock| lock.surface.as_ref())
            .filter(|surface| surface.alive())
    }

    pub(super) fn lock_surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(
        smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        Point<f64, Logical>,
    )> {
        let surface = self.lock_surface()?;
        under_from_surface_tree(
            surface.wl_surface(),
            pos,
            (0, 0),
            smithay::desktop::WindowSurfaceType::ALL,
        )
        .map(|(surface, location)| (surface, location.to_f64()))
    }

    pub(super) fn lock_focus(&mut self) {
        let surface = self.lock_surface().and_then(|surface| {
            with_renderer_surface_state(surface.wl_surface(), |state| state.buffer().is_some())
                .unwrap_or(false)
                .then(|| surface.wl_surface().clone())
        });
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(
            self,
            surface.filter(|_| self.host_focused),
            SERIAL_COUNTER.next_serial(),
        );
        self.refresh_pointer_focus();
    }

    pub(super) fn confirm_session_lock(&mut self) {
        if let Some(confirmation) = self
            .session_lock
            .as_mut()
            .and_then(|lock| lock.confirmation.take())
        {
            confirmation.lock();
        }
    }

    pub(super) fn session_lock_pending(&self) -> bool {
        self.session_lock
            .as_ref()
            .is_some_and(|lock| lock.confirmation.is_some())
    }

    pub(super) fn pending_session_lock_id(&self) -> Option<ExtSessionLockV1> {
        self.session_lock
            .as_ref()
            .and_then(|lock| lock.confirmation.as_ref().map(|_| lock.resource.clone()))
    }

    pub(super) fn reconfigure_lock_surface(&mut self) {
        let size = self
            .output
            .as_ref()
            .and_then(Output::current_mode)
            .map(|mode| mode.size);
        if let (Some(size), Some(surface)) = (size, self.lock_surface()) {
            surface.with_pending_state(|state| {
                state.size = Some((size.w as u32, size.h as u32).into())
            });
            surface.send_configure();
        }
        self.dirty = true;
    }
}

impl SessionLockHandler for Wisp {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        self.session_lock_state
            .as_mut()
            .expect("session-lock global is disabled")
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        if self.session_lock.is_some() {
            // A second client must not replace the real lock owner.
            return; // dropping confirmation sends `finished`
        }
        let resource = confirmation.ext_session_lock().clone();
        self.cancel_workspace_slide();
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.start_time.elapsed().as_millis() as u32;
        self.release_held_input(serial, time);
        let pointer = self.seat.get_pointer().unwrap();
        pointer.unset_grab(self, serial, time);
        pointer.frame(self);
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.unset_grab(self);
        keyboard.set_focus(self, None, serial);
        self.session_lock = Some(LockedSession {
            resource,
            confirmation: Some(confirmation),
            surface: None,
        });
        self.refresh_pointer_focus();
        self.dirty = true;
    }

    fn unlock(&mut self) {
        // Dispatch checks the owner and confirmed state before reaching here.
        self.session_lock = None;
        self.restore_x11_selections();
        self.release_input();
        self.dirty = true;
        self.host_focus(self.host_focused);
        self.refresh_pointer_focus();
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        let owner = self
            .session_lock
            .as_ref()
            .and_then(|lock| lock.resource.client());
        let expected = self.output.as_ref();
        if owner.is_none()
            || surface.wl_surface().client() != owner
            || Output::from_resource(&output).as_ref() != expected
        {
            return;
        }
        if let Some(lock) = self.session_lock.as_mut() {
            lock.surface = Some(surface);
        }
        self.reconfigure_lock_surface();
    }
}

// Delegate the global, manager and lock-surface protocol to Smithay. Handle
// the lock resource ourselves: the stock dispatcher calls unlock() even after
// posting InvalidUnlock for a lock that never reached the locked state.
smithay::reexports::wayland_server::delegate_global_dispatch!(Wisp: [ExtSessionLockManagerV1: SessionLockManagerGlobalData] => SessionLockManagerState);
smithay::reexports::wayland_server::delegate_dispatch!(Wisp: [ExtSessionLockManagerV1: ()] => SessionLockManagerState);
smithay::reexports::wayland_server::delegate_dispatch!(Wisp: [ExtSessionLockSurfaceV1: ExtLockSurfaceUserData] => SessionLockManagerState);

impl Dispatch<ExtSessionLockV1, SessionLockState> for Wisp {
    fn request(
        state: &mut Self,
        client: &Client,
        lock: &ExtSessionLockV1,
        request: ext_session_lock_v1::Request,
        data: &SessionLockState,
        dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let owned = state
            .session_lock
            .as_ref()
            .is_some_and(|active| active.resource == *lock);
        match request {
            ext_session_lock_v1::Request::UnlockAndDestroy if !owned || state.session_lock_pending() => {
                lock.post_error(ext_session_lock_v1::Error::InvalidUnlock, "not the confirmed active lock");
            }
            ext_session_lock_v1::Request::GetLockSurface { .. } if !owned => {
                lock.post_error(ext_session_lock_v1::Error::InvalidUnlock, "not the active lock");
            }
            ext_session_lock_v1::Request::GetLockSurface { ref surface, ref output, .. }
                if state.session_lock.as_ref().is_some_and(|active| active.surface.is_some())
                    || surface.client() != lock.client()
                    || Output::from_resource(output).as_ref() != state.output.as_ref() => {
                lock.post_error(ext_session_lock_v1::Error::Role, "invalid or duplicate lock surface/output");
            }
            ext_session_lock_v1::Request::Destroy if owned && state.session_lock_pending()
                && state.lock_surface().is_some() => {
                // Smithay retains its private locked_outputs list after
                // pending Destroy; fail closed rather than corrupt future locks.
                lock.post_error(ext_session_lock_v1::Error::InvalidDestroy, "pending lock surface must not be abandoned");
            }
            ext_session_lock_v1::Request::Destroy if owned && state.session_lock_pending() => {
                state.session_lock = None;
                state.restore_x11_selections();
                state.dirty = true;
                state.host_focus(state.host_focused);
                <SessionLockManagerState as Dispatch<ExtSessionLockV1, SessionLockState, Wisp>>::request(
                    state, client, lock, request, data, dh, data_init,
                );
            }
            _ => <SessionLockManagerState as Dispatch<ExtSessionLockV1, SessionLockState, Wisp>>::request(
                state, client, lock, request, data, dh, data_init,
            ),
        }
    }
}
