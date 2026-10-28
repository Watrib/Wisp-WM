// Input-grab plumbing is adapted from Smithay's MIT-licensed Smallvil example.
use smithay::{
    input::pointer::{
        AxisFrame, ButtonEvent, Focus, GestureHoldBeginEvent, GestureHoldEndEvent,
        GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
        GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData,
        MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel::{ResizeEdge, State},
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Serial},
    wayland::{compositor::with_states, shell::xdg::SurfaceCachedState},
};

use super::Wisp;
use crate::layout::Rect;

struct InteractiveGrab {
    start: GrabStartData<Wisp>,
    id: u64,
    initial: Rect,
    edges: Option<ResizeEdge>,
}

impl Wisp {
    pub(crate) fn start_interactive(
        &mut self,
        id: u64,
        start: GrabStartData<Self>,
        serial: Serial,
        edges: Option<ResizeEdge>,
    ) {
        let Some(entry) = self
            .windows
            .iter_mut()
            .find(|w| w.id == id && !w.fullscreen)
        else {
            return;
        };
        let initial = entry.rect;
        entry.floating = true;
        entry.floating_rect = initial;
        self.arrange();
        let pointer = self.seat.get_pointer().unwrap();
        pointer.set_grab(
            self,
            InteractiveGrab {
                start,
                id,
                initial,
                edges,
            },
            serial,
            Focus::Clear,
        );
    }
}

// Forward gesture events without re-locking the outer PointerHandle.
macro_rules! forward_gesture {
    ($name:ident, $event:ty) => {
        fn $name(
            &mut self,
            data: &mut Wisp,
            handle: &mut PointerInnerHandle<'_, Wisp>,
            event: &$event,
        ) {
            handle.$name(data, event);
        }
    };
}

impl PointerGrab<Wisp> for InteractiveGrab {
    fn motion(
        &mut self,
        data: &mut Wisp,
        handle: &mut PointerInnerHandle<'_, Wisp>,
        _: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        let Some(entry) = data.windows.iter_mut().find(|w| w.id == self.id) else {
            handle.unset_grab(self, data, event.serial, event.time, true);
            return;
        };
        let delta: Point<i32, Logical> = (event.location - self.start.location).to_i32_round();
        let mut rect = self.initial;
        if let Some(edges) = self.edges {
            let left = matches!(
                edges,
                ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft
            );
            let right = matches!(
                edges,
                ResizeEdge::Right | ResizeEdge::TopRight | ResizeEdge::BottomRight
            );
            let top = matches!(
                edges,
                ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight
            );
            let bottom = matches!(
                edges,
                ResizeEdge::Bottom | ResizeEdge::BottomLeft | ResizeEdge::BottomRight
            );
            let (min, max) = entry.window.toplevel().map_or_else(
                || ((1, 1).into(), (0, 0).into()),
                |surface| {
                    with_states(surface.wl_surface(), |states| {
                        let mut cache = states.cached_state.get::<SurfaceCachedState>();
                        (cache.current().min_size, cache.current().max_size)
                    })
                },
            );
            let max_width = if max.w > 0 {
                max.w.max(min.w).max(1)
            } else {
                i32::MAX
            };
            let max_height = if max.h > 0 {
                max.h.max(min.h).max(1)
            } else {
                i32::MAX
            };
            if left || right {
                rect.width = (self.initial.width + if left { -delta.x } else { delta.x })
                    .clamp(min.w.max(1), max_width);
            }
            if top || bottom {
                rect.height = (self.initial.height + if top { -delta.y } else { delta.y })
                    .clamp(min.h.max(1), max_height);
            }
            if left {
                rect.x = self.initial.x + self.initial.width - rect.width;
            }
            if top {
                rect.y = self.initial.y + self.initial.height - rect.height;
            }
            if let Some(surface) = entry.window.toplevel() {
                surface.with_pending_state(|state| {
                    state.size = Some((rect.width, rect.height).into());
                    state.states.set(State::Resizing);
                });
                surface.send_pending_configure();
            } else if let Some(x11) = entry.window.x11_surface() {
                let geometry = smithay::utils::Rectangle::new(
                    (rect.x, rect.y).into(),
                    (rect.width, rect.height).into(),
                );
                if let Err(error) = x11.configure(geometry) {
                    tracing::warn!(%error, "resizing X11 window");
                }
            }
        } else {
            rect.x += delta.x;
            rect.y += delta.y;
        }
        entry.floating_rect = rect;
        entry.rect = rect;
        data.space
            .map_element(entry.window.clone(), (rect.x, rect.y), false);
        data.dirty = true;
    }

    fn relative_motion(
        &mut self,
        data: &mut Wisp,
        handle: &mut PointerInnerHandle<'_, Wisp>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut Wisp,
        handle: &mut PointerInnerHandle<'_, Wisp>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if !handle.current_pressed().contains(&self.start.button) {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut Wisp,
        handle: &mut PointerInnerHandle<'_, Wisp>,
        frame: AxisFrame,
    ) {
        handle.axis(data, frame);
    }
    fn frame(&mut self, data: &mut Wisp, handle: &mut PointerInnerHandle<'_, Wisp>) {
        handle.frame(data);
    }
    forward_gesture!(gesture_swipe_begin, GestureSwipeBeginEvent);
    forward_gesture!(gesture_swipe_update, GestureSwipeUpdateEvent);
    forward_gesture!(gesture_swipe_end, GestureSwipeEndEvent);
    forward_gesture!(gesture_pinch_begin, GesturePinchBeginEvent);
    forward_gesture!(gesture_pinch_update, GesturePinchUpdateEvent);
    forward_gesture!(gesture_pinch_end, GesturePinchEndEvent);
    forward_gesture!(gesture_hold_begin, GestureHoldBeginEvent);
    forward_gesture!(gesture_hold_end, GestureHoldEndEvent);

    fn start_data(&self) -> &GrabStartData<Wisp> {
        &self.start
    }

    fn unset(&mut self, data: &mut Wisp) {
        if let Some(entry) = data.windows.iter().find(|w| w.id == self.id) {
            if let Some(surface) = entry.window.toplevel() {
                surface.with_pending_state(|state| state.states.unset(State::Resizing));
                surface.send_pending_configure();
            }
        }
        data.dirty = true;
    }
}
