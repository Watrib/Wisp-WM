//! Optional client protocols. Keep policy here; Smithay owns wire validation.
use std::time::{Duration, Instant};

use smithay::{
    delegate_cursor_shape, delegate_fractional_scale, delegate_input_method_manager,
    delegate_text_input_manager, delegate_viewporter, delegate_xdg_activation,
    desktop::{PopupKind, PopupManager, Window, layer_map_for_output},
    input::Seat,
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    utils::{Logical, Rectangle, Serial},
    wayland::shell::wlr_layer::{WlrLayerShellHandler, WlrLayerShellState},
    wayland::{
        compositor::with_states,
        fractional_scale::{FractionalScaleHandler, with_fractional_scale},
        input_method::{InputMethodHandler, PopupSurface},
        tablet_manager::TabletSeatHandler,
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
    },
};
use tracing::warn;

use super::Wisp;

pub(super) mod icons;
use icons::ToplevelIconCachedState;

const ACTIVATION_LIFETIME: Duration = Duration::from_secs(10);
const MAX_ACTIVATION_TOKENS: usize = 128;
const MAX_INPUT_SERIALS: usize = 32;

struct ActivationGrant {
    can_focus: bool,
}

impl Wisp {
    pub(super) fn remember_activation_input(&mut self, serial: Serial, surface: Option<WlSurface>) {
        let Some(client) = surface.and_then(|surface| surface.client()) else {
            return;
        };
        self.recent_input
            .retain(|(_, _, time)| time.elapsed() < ACTIVATION_LIFETIME);
        if self.recent_input.len() == MAX_INPUT_SERIALS {
            self.recent_input.pop_front();
        }
        self.recent_input
            .push_back((serial, client.id(), Instant::now()));
    }

    fn token_can_focus(&self, data: &XdgActivationTokenData) -> bool {
        let Some((serial, seat)) = &data.serial else {
            return false;
        };
        if self.session_lock.is_some()
            || !self.host_focused
            || Seat::<Self>::from_resource(seat).as_ref() != Some(&self.seat)
        {
            return false;
        }
        let keyboard = self.seat.get_keyboard().unwrap();
        let Some(client) = keyboard
            .current_focus()
            .and_then(|surface| surface.client())
        else {
            return false;
        };
        if data.client_id.as_ref() != Some(&client.id()) {
            return false;
        }
        // Do not accept arbitrary larger serials: they must be a real focus
        // event or recent key/button event delivered to the requesting client.
        keyboard.last_enter() == Some(*serial)
            || self.recent_input.iter().any(|(seen, owner, time)| {
                seen == serial && owner == &client.id() && time.elapsed() < ACTIVATION_LIFETIME
            })
    }

    pub(super) fn icon_metadata(window: &Window) -> (Option<String>, usize) {
        let Some(toplevel) = window.toplevel() else {
            return (None, 0);
        };
        with_states(toplevel.wl_surface(), |states| {
            let mut icon = states.cached_state.get::<ToplevelIconCachedState>();
            let icon = icon.current();
            (icon.icon_name().map(str::to_owned), icon.buffer_count())
        })
    }
}

impl WlrLayerShellHandler for Wisp {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self._layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: smithay::wayland::shell::wlr_layer::LayerSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
        _layer: smithay::wayland::shell::wlr_layer::Layer,
        namespace: String,
    ) {
        // Smithay's delegation owns the wire protocol, not the scene. Build
        // the canonical desktop handle once (the layer map keys handles by
        // identity) and reserve the surface on the single output's layer map
        // right away: it stays invisible until it commits a buffer, while
        // stacking, exclusive zones and the initial configure all become
        // live. `_output` is the raw resource handle; with one output there
        // is nothing to resolve.
        let layer = smithay::desktop::LayerSurface::new(surface.clone(), namespace);
        if let Some(output) = &self.output {
            let _ = layer_map_for_output(output).map_layer(&layer);
        }
        self.layer_surfaces.push(layer);
    }

    fn layer_destroyed(&mut self, surface: smithay::wayland::shell::wlr_layer::LayerSurface) {
        if let Some(index) = self
            .layer_surfaces
            .iter()
            .position(|layer| layer.wl_surface() == surface.wl_surface())
        {
            let layer = self.layer_surfaces.swap_remove(index);
            if let Some(output) = &self.output {
                layer_map_for_output(output).unmap_layer(&layer);
            }
            self.dirty = true;
            self.refresh_pointer_focus();
        }
    }
}

smithay::delegate_layer_shell!(Wisp);

impl XdgActivationHandler for Wisp {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.activation_state
    }

    fn token_created(&mut self, _: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        self.activation_state
            .retain_tokens(|_, data| data.timestamp.elapsed() < ACTIVATION_LIFETIME);
        if self.shutdown.is_some()
            || self.activation_state.tokens().count() >= MAX_ACTIVATION_TOKENS
        {
            return false;
        }
        let can_focus = self.token_can_focus(&data);
        data.user_data
            .insert_if_missing(|| ActivationGrant { can_focus });
        // A token without user input may request attention for its own window,
        // but must never steal focus or change the active workspace.
        true
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        self.activation_state.remove_token(&token);
        if self.shutdown.is_some()
            || self.session_lock.is_some()
            || data.timestamp.elapsed() >= ACTIVATION_LIFETIME
        {
            return;
        }
        let Some(index) = self.windows.iter().position(|w| {
            w.window
                .toplevel()
                .is_some_and(|toplevel| toplevel.wl_surface() == &surface)
        }) else {
            return;
        };
        let can_focus = data
            .user_data
            .get::<ActivationGrant>()
            .is_some_and(|grant| grant.can_focus);
        if can_focus && self.host_focused {
            let workspace = self.windows[index].workspace;
            let id = self.windows[index].id;
            self.select_workspace(workspace);
            self.arrange();
            self.focus(Some(id));
            // The focused-follows-activation switch is visible to clients.
            self.ext_workspace_notify();
        } else if surface.client().map(|client| client.id()) == data.client_id
            && self.focused != Some(self.windows[index].id)
        {
            self.windows[index].urgent = true;
            self.dirty = true;
            self.ext_workspace_notify();
        }
    }
}
delegate_xdg_activation!(Wisp);

impl TabletSeatHandler for Wisp {}
delegate_cursor_shape!(Wisp);

impl FractionalScaleHandler for Wisp {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        // There is one output, including for hidden workspaces and
        // not-yet-mapped surfaces. It currently stays at scale 1.
        let scale = self
            .output
            .as_ref()
            .map(|output| output.current_scale().fractional_scale())
            .unwrap_or(1.0);
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| fractional.set_preferred_scale(scale));
        });
    }
}
delegate_fractional_scale!(Wisp);
// Smithay's on_commit_buffer_handler validates and applies viewport source and
// destination rectangles before the existing surface renderer imports a buffer.
delegate_viewporter!(Wisp);

// WlSurface's KeyboardTarget implementation synchronizes text-input focus with
// keyboard focus, including popups, host focus loss, and workspace changes.
delegate_text_input_manager!(Wisp);

impl InputMethodHandler for Wisp {
    fn new_popup(&mut self, surface: PopupSurface) {
        if let Err(error) = self.popups.track_popup(PopupKind::InputMethod(surface)) {
            warn!(%error, "could not track input-method popup");
        }
        self.dirty = true;
    }

    fn dismiss_popup(&mut self, surface: PopupSurface) {
        if let Some(parent) = surface.get_parent() {
            let parent = parent.surface.clone();
            let _ = PopupManager::dismiss_popup(&parent, &PopupKind::InputMethod(surface));
        }
        self.dirty = true;
    }

    fn popup_repositioned(&mut self, _: PopupSurface) {
        self.dirty = true;
    }

    fn parent_geometry(&self, parent: &WlSurface) -> Rectangle<i32, Logical> {
        self.windows
            .iter()
            .find(|entry| {
                entry
                    .window
                    .toplevel()
                    .is_some_and(|toplevel| toplevel.wl_surface() == parent)
            })
            .map(|entry| entry.window.geometry())
            .unwrap_or_default()
    }
}
delegate_input_method_manager!(Wisp);

#[cfg(test)]
mod tests;
