use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, Keycode, PointerAxisEvent, PointerButtonEvent,
        PointerMotionEvent,
    },
    input::{
        keyboard::{FilterResult, keysyms},
        pointer::{AxisFrame, ButtonEvent, GrabStartData, MotionEvent},
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
};
use tracing::warn;

use smithay::wayland::shell::wlr_layer::Layer;

use super::Wisp;
use crate::{layout::Direction, protocol::Action};

enum KeyAction {
    Dispatch(Action),
    SwitchVt(i32),
    OverviewCycle { backwards: bool },
}

impl Wisp {
    pub(crate) fn process_input<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event } => {
                self.keyboard_key(event.key_code(), event.state(), event.time_msec(), false);
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let Some(output) = self.output.as_ref() else {
                    return;
                };
                let Some(geometry) = self.space.output_geometry(output) else {
                    return;
                };
                let position = event.position_transformed(geometry.size) + geometry.loc.to_f64();
                self.pointer_motion(position, event.time_msec());
            }
            InputEvent::PointerMotion { event } => {
                let pointer = self.seat.get_pointer().unwrap();
                let position =
                    pointer.current_location() + Point::from((event.delta_x(), event.delta_y()));
                self.pointer_motion(position, event.time_msec());
            }
            InputEvent::PointerButton { event } => {
                let pointer = self.seat.get_pointer().unwrap();
                let button = event.button_code();
                if self.session_lock.is_some() {
                    if event.state() == ButtonState::Pressed {
                        self.pressed_buttons.insert(button);
                    } else {
                        self.pressed_buttons.remove(&button);
                    }
                    pointer.button(
                        self,
                        &ButtonEvent {
                            button,
                            state: event.state(),
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time_msec(),
                        },
                    );
                    pointer.frame(self);
                    return;
                }
                if self.workspace_slide.is_some() {
                    if event.state() == ButtonState::Pressed {
                        self.slide_buttons.insert(button);
                    } else {
                        self.slide_buttons.remove(&button);
                    }
                    return;
                }
                if event.state() == ButtonState::Released && self.slide_buttons.remove(&button) {
                    return;
                }
                let serial = SERIAL_COUNTER.next_serial();
                if self.overview || self.overview_exiting || self.overview_click == Some(button) {
                    if event.state() == ButtonState::Pressed && button == 0x110 {
                        self.overview_click = Some(button);
                    } else if event.state() == ButtonState::Released
                        && self.overview_click == Some(button)
                    {
                        self.overview_click = None;
                        if self.overview {
                            let selected = self.overview_window_at(pointer.current_location());
                            if selected.is_some() {
                                self.set_overview(false);
                                self.focus(selected);
                            }
                        }
                    }
                    return;
                }
                if event.state() == ButtonState::Pressed {
                    self.pressed_buttons.insert(button);
                } else {
                    self.pressed_buttons.remove(&button);
                }
                if event.state() == ButtonState::Pressed && !pointer.is_grabbed() {
                    let position = pointer.current_location();
                    // Panel/overlay surfaces above windows own clicks on
                    // their region: pass them through without moving window
                    // focus or starting an interactive move underneath.
                    let owned = self
                        .layer_surface_under(position, &[Layer::Overlay, Layer::Top])
                        .is_some();
                    let id = if owned {
                        None
                    } else {
                        self.window_under(position)
                    };
                    if !owned {
                        self.focus(id);
                    }
                    let mods = self.seat.get_keyboard().unwrap().modifier_state();
                    if !owned
                        && self.config.general.modifier.held(&mods)
                        && matches!(button, 0x110 | 0x111)
                    {
                        if let Some(id) = id {
                            let start = GrabStartData {
                                focus: self.surface_under(position),
                                button,
                                location: position,
                            };
                            let edges = if button == 0x111 {
                                Some(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge::BottomRight)
                            } else {
                                None
                            };
                            self.start_interactive(id, start, serial, edges);
                        }
                    }
                }
                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: event.state(),
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
                if event.state() == ButtonState::Pressed {
                    self.remember_activation_input(serial, pointer.current_focus());
                }
            }
            InputEvent::PointerAxis { event } => {
                if self.workspace_slide.is_some() {
                    return;
                }
                let mut frame = AxisFrame::new(event.time_msec()).source(event.source());
                for axis in [Axis::Horizontal, Axis::Vertical] {
                    let amount = event
                        .amount(axis)
                        .unwrap_or_else(|| event.amount_v120(axis).unwrap_or(0.0) * 15.0 / 120.0);
                    if amount != 0.0 {
                        frame = frame.value(axis, amount);
                    }
                    if let Some(discrete) = event.amount_v120(axis) {
                        frame = frame.v120(axis, discrete as i32);
                    }
                    if event.source() == AxisSource::Finger && event.amount(axis) == Some(0.0) {
                        frame = frame.stop(axis);
                    }
                }
                let pointer = self.seat.get_pointer().unwrap();
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            _ => {}
        }
    }

    /// XKB and shortcut handling shared by both backends. Only the direct
    /// caller enables VT actions and executes them through its libseat session.
    pub(super) fn keyboard_key(
        &mut self,
        key: Keycode,
        state: KeyState,
        time: u32,
        allow_vt_switch: bool,
    ) -> Option<i32> {
        let keyboard = self.seat.get_keyboard().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        let action = keyboard.input(self, key, state, serial, time, |data, mods, handle| {
            if data.session_lock.is_some() {
                let symbol = handle.modified_sym().raw();
                if state == KeyState::Pressed
                    && allow_vt_switch
                    && (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12)
                        .contains(&symbol)
                {
                    data.suppressed_keys.insert(key.raw());
                    return FilterResult::Intercept(Some(KeyAction::SwitchVt(
                        (symbol - keysyms::KEY_XF86Switch_VT_1 + 1) as i32,
                    )));
                }
                if state == KeyState::Released && data.suppressed_keys.remove(&key.raw()) {
                    return FilterResult::Intercept(None);
                }
                return FilterResult::Forward;
            }
            if state == KeyState::Released && data.suppressed_keys.remove(&key.raw()) {
                return FilterResult::Intercept(None);
            }
            if state == KeyState::Pressed {
                let symbol = handle.modified_sym().raw();
                if data.overview && symbol == keysyms::KEY_Escape {
                    data.suppressed_keys.insert(key.raw());
                    return FilterResult::Intercept(Some(KeyAction::Dispatch(
                        Action::ToggleOverview {},
                    )));
                }
                if data.overview && !mods.alt && !mods.ctrl && !mods.logo {
                    let action = match symbol {
                        keysyms::KEY_Tab | keysyms::KEY_ISO_Left_Tab => {
                            Some(KeyAction::OverviewCycle {
                                backwards: mods.shift,
                            })
                        }
                        keysyms::KEY_Return | keysyms::KEY_KP_Enter if !mods.shift => {
                            Some(KeyAction::Dispatch(Action::ToggleOverview {}))
                        }
                        keysyms::KEY_Left if !mods.shift => {
                            Some(KeyAction::Dispatch(Action::FocusDirection {
                                direction: Direction::Left,
                            }))
                        }
                        keysyms::KEY_Right if !mods.shift => {
                            Some(KeyAction::Dispatch(Action::FocusDirection {
                                direction: Direction::Right,
                            }))
                        }
                        keysyms::KEY_Up if !mods.shift => {
                            Some(KeyAction::Dispatch(Action::FocusDirection {
                                direction: Direction::Up,
                            }))
                        }
                        keysyms::KEY_Down if !mods.shift => {
                            Some(KeyAction::Dispatch(Action::FocusDirection {
                                direction: Direction::Down,
                            }))
                        }
                        _ => None,
                    };
                    if let Some(action) = action {
                        data.suppressed_keys.insert(key.raw());
                        return FilterResult::Intercept(Some(action));
                    }
                }
                if allow_vt_switch
                    && (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12)
                        .contains(&symbol)
                {
                    data.suppressed_keys.insert(key.raw());
                    return FilterResult::Intercept(Some(KeyAction::SwitchVt(
                        (symbol - keysyms::KEY_XF86Switch_VT_1 + 1) as i32,
                    )));
                }
                // Avoid allocating keysyms in the keyboard hot path.
                if let Some(symbol) = handle.raw_latin_sym_or_raw_current_sym() {
                    if let Some(binding) = data.bindings.iter().find(|b| b.matches(mods, &[symbol]))
                    {
                        data.suppressed_keys.insert(key.raw());
                        return FilterResult::Intercept(Some(KeyAction::Dispatch(
                            binding.action.clone(),
                        )));
                    }
                }
            }
            if data.workspace_slide.is_some() {
                if state == KeyState::Pressed {
                    data.suppressed_keys.insert(key.raw());
                    return FilterResult::Intercept(None);
                }
                // No client has focus while sliding. Forward *only* releases
                // that were forwarded before the slide so Smithay clears its
                // forwarded-pressed set. Shortcut releases were intercepted
                // above via suppressed_keys and never enter that set.
                return FilterResult::Forward;
            }
            FilterResult::Forward
        });
        if state == KeyState::Pressed && action.is_none() {
            self.remember_activation_input(serial, keyboard.current_focus());
        }
        match action {
            Some(Some(KeyAction::OverviewCycle { backwards })) => {
                let selected =
                    super::overview::cycle(self.overview_items(), self.focused, backwards);
                self.focus(selected);
            }
            Some(Some(KeyAction::SwitchVt(vt))) => return Some(vt),
            Some(Some(KeyAction::Dispatch(action))) => {
                if let Err(error) = self.dispatch(action) {
                    warn!(error = %format!("{error:#}"), "shortcut failed");
                }
            }
            _ => {}
        }
        None
    }

    /// Move the pointer in logical coordinates, clamped to the single output.
    /// Absolute and relative backends share this to keep focus and cursor
    /// invalidation behavior identical.
    pub(crate) fn pointer_motion(&mut self, position: Point<f64, Logical>, time: u32) {
        let Some(output) = self.output.as_ref() else {
            return;
        };
        let Some(geometry) = self.space.output_geometry(output) else {
            return;
        };
        let position = clamp_position(position, geometry);
        let pointer = self.seat.get_pointer().unwrap();
        if self.session_lock.is_some() {
            // Only the lock surface may receive pointer motion; never focus a
            // desktop window even when focus-follows-mouse is enabled.
        } else if self.overview {
            if let Some(id) = self.overview_window_at(position) {
                self.focus(Some(id));
            }
        } else if !self.overview_exiting
            && self.workspace_slide.is_none()
            && self.config.input.focus_follows_mouse
            && !pointer.is_grabbed()
        {
            let owned = self
                .layer_surface_under(position, &[Layer::Overlay, Layer::Top])
                .is_some();
            if !owned {
                if let Some(id) = self.window_under(position) {
                    self.focus(Some(id));
                }
            }
        }
        let under = self.surface_under(position);
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: position,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
        if self.software_cursor
            || matches!(
                self.cursor_status,
                smithay::input::pointer::CursorImageStatus::Surface(_)
            )
        {
            self.dirty = true;
        }
    }
}

/// Clamp a logical pointer position to the inclusive area of one output.
fn clamp_position(
    position: Point<f64, Logical>,
    area: Rectangle<i32, Logical>,
) -> Point<f64, Logical> {
    let min_x = area.loc.x as f64;
    let min_y = area.loc.y as f64;
    let max_x = (area.loc.x + area.size.w - 1).max(area.loc.x) as f64;
    let max_y = (area.loc.y + area.size.h - 1).max(area.loc.y) as f64;
    Point::from((
        position.x.clamp(min_x, max_x),
        position.y.clamp(min_y, max_y),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::{
        output::{Mode, Output, PhysicalProperties, Subpixel},
        utils::Transform,
    };

    #[test]
    fn alt_stays_held_across_slide_shortcuts_and_releases_cleanly() {
        let (_display, mut state) = crate::compositor::tests::test_state();
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
            Some(Mode {
                size: (800, 600).into(),
                refresh: 60_000,
            }),
            Some(Transform::Normal),
            None,
            Some((0, 0).into()),
        );
        state.space.map_output(&output, (0, 0));
        state.output = Some(output);
        state.config.layout.workspace_slide = true;
        // Avoid launching a real foot process in the test. Use the default
        // Alt+Return binding, but make its action another workspace switch.
        state
            .bindings
            .iter_mut()
            .find(|binding| matches!(binding.action, Action::Terminal {}))
            .unwrap()
            .action = Action::Workspace { index: 3 };
        let keyboard = state.seat.get_keyboard().unwrap();

        // XKB evdev codes: left Alt=64, digit 2=11, Return=36.
        state.keyboard_key(64u32.into(), KeyState::Pressed, 1, false);
        state.keyboard_key(11u32.into(), KeyState::Pressed, 2, false);
        assert_eq!(state.active_workspace, 2);
        assert!(state.workspace_slide.is_some());
        assert!(keyboard.modifier_state().alt);
        state.keyboard_key(11u32.into(), KeyState::Released, 3, false);
        assert!(keyboard.modifier_state().alt);
        state.keyboard_key(36u32.into(), KeyState::Pressed, 4, false);
        assert_eq!(state.active_workspace, 3);
        state.keyboard_key(36u32.into(), KeyState::Released, 5, false);
        state.keyboard_key(64u32.into(), KeyState::Released, 6, false);
        assert!(!keyboard.modifier_state().alt);
        state.finish_workspace_slide(std::time::Instant::now() + std::time::Duration::from_secs(1));
        assert!(state.workspace_slide.is_none());
        assert!(state.suppressed_keys.is_empty());

        // Releasing Alt *during* a slide must also clear forwarded key state.
        state.keyboard_key(64u32.into(), KeyState::Pressed, 7, false);
        state.keyboard_key(11u32.into(), KeyState::Pressed, 8, false);
        state.keyboard_key(64u32.into(), KeyState::Released, 9, false);
        assert!(!keyboard.modifier_state().alt);
        state.keyboard_key(11u32.into(), KeyState::Released, 10, false);
        state.finish_workspace_slide(std::time::Instant::now() + std::time::Duration::from_secs(1));
        assert!(state.suppressed_keys.is_empty());
    }

    #[test]
    fn vt_shortcut_is_direct_only_and_releases_cleanly() {
        let (_display, mut state) = crate::compositor::tests::test_state();
        let keyboard = state.seat.get_keyboard().unwrap();
        // XKB keycodes: left Ctrl, left Alt, F3.
        for direct in [false, true] {
            state.keyboard_key(37u32.into(), KeyState::Pressed, 1, direct);
            state.keyboard_key(64u32.into(), KeyState::Pressed, 2, direct);
            assert_eq!(
                state.keyboard_key(69u32.into(), KeyState::Pressed, 3, direct),
                direct.then_some(3)
            );
            assert_eq!(state.suppressed_keys.contains(&69), direct);
            state.host_focus(false);
            assert!(keyboard.pressed_keys().is_empty());
            assert!(!keyboard.modifier_state().ctrl);
            assert!(!keyboard.modifier_state().alt);
            assert!(state.suppressed_keys.is_empty());
            state.host_focus(true);
        }
        // A normal function key must not cause a session switch.
        assert_eq!(
            state.keyboard_key(69u32.into(), KeyState::Pressed, 4, true),
            None
        );
        assert!(state.suppressed_keys.is_empty());
    }

    #[test]
    fn pointer_is_clamped_to_the_output() {
        let area = Rectangle::new((10, 20).into(), (100, 50).into());
        assert_eq!(
            clamp_position(Point::from((-5.0, -5.0)), area),
            Point::from((10.0, 20.0))
        );
        assert_eq!(
            clamp_position(Point::from((500.0, 500.0)), area),
            Point::from((109.0, 69.0))
        );
        assert_eq!(
            clamp_position(Point::from((50.5, 30.5)), area),
            Point::from((50.5, 30.5))
        );
    }

    #[test]
    fn degenerate_output_still_clamps_to_its_origin() {
        let area = Rectangle::new((0, 0).into(), (0, 0).into());
        assert_eq!(
            clamp_position(Point::from((10.0, 10.0)), area),
            Point::from((0.0, 0.0))
        );
    }
}
