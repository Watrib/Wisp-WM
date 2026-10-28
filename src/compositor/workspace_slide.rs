//! Bounded, opt-in workspace transition. The shell and cursor stay stationary;
//! only windows from the outgoing and incoming workspaces move horizontally.

use std::time::{Duration, Instant};

use crate::layout::Rect;

pub(super) struct WorkspaceSlide {
    pub(super) from: u8,
    pub(super) to: u8,
    started: Instant,
    duration: Duration,
    forward: bool,
}

impl WorkspaceSlide {
    pub(super) fn new(from: u8, to: u8, duration_ms: u32, now: Instant) -> Self {
        Self {
            from,
            to,
            started: now,
            duration: Duration::from_millis(u64::from(duration_ms)),
            forward: to > from,
        }
    }

    pub(super) fn done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= self.duration
    }

    fn progress(&self, now: Instant) -> f64 {
        (now.saturating_duration_since(self.started).as_secs_f64() / self.duration.as_secs_f64())
            .clamp(0.0, 1.0)
    }

    /// Pair short motion with a crossfade rather than moving every window a
    /// full monitor width. Keep the opacity endpoints exact for damage and
    /// animation completion.
    pub(super) fn opacity(&self, workspace: u8, now: Instant) -> Option<f32> {
        let t = self.progress(now);
        let smooth = |t: f64| t * t * (3.0 - 2.0 * t);
        if workspace == self.from {
            Some((1.0 - smooth((t / 0.75).min(1.0))) as f32)
        } else if workspace == self.to {
            Some(smooth((t / 0.85).min(1.0)) as f32)
        } else {
            None
        }
    }

    pub(super) fn offset(&self, workspace: u8, width: i32, now: Instant) -> Option<i32> {
        let t = self.progress(now);
        // A brief decisive move with a soft landing. Outgoing content travels
        // less than incoming content so the destination feels foregrounded.
        let progress = 1.0 - (1.0 - t).powi(3);
        let signed_width = f64::from(width.max(0)) * if self.forward { 1.0 } else { -1.0 };
        let shift = if workspace == self.from {
            -signed_width * 0.12 * progress
        } else if workspace == self.to {
            signed_width * 0.24 * (1.0 - progress)
        } else {
            return None;
        };
        Some(shift.round() as i32)
    }

    pub(super) fn slide_rect(
        &self,
        workspace: u8,
        rect: Rect,
        width: i32,
        now: Instant,
    ) -> Option<Rect> {
        self.offset(workspace, width, now).map(|offset| {
            Rect::new(
                rect.x.saturating_add(offset),
                rect.y,
                rect.width,
                rect.height,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slides_both_workspaces_and_stops_exactly_at_the_target() {
        let now = Instant::now();
        let rect = Rect::new(100, 20, 200, 100);
        let slide = WorkspaceSlide::new(1, 2, 240, now);
        assert_eq!(slide.slide_rect(1, rect, 1000, now).unwrap().x, 100);
        assert_eq!(slide.slide_rect(2, rect, 1000, now).unwrap().x, 340);
        assert_eq!(
            slide
                .slide_rect(1, rect, 1000, now + Duration::from_millis(120))
                .unwrap()
                .x,
            -5
        );
        assert_eq!(
            slide
                .slide_rect(2, rect, 1000, now + Duration::from_millis(120))
                .unwrap()
                .x,
            130
        );
        assert_eq!(
            slide
                .slide_rect(2, rect, 1000, now + Duration::from_millis(240))
                .unwrap(),
            rect
        );
        assert!(slide.done(now + Duration::from_millis(240)));
        assert_eq!(slide.offset(3, 1000, now), None);
        assert_eq!(slide.opacity(1, now), Some(1.0));
        assert_eq!(slide.opacity(2, now), Some(0.0));
        assert_eq!(
            slide.opacity(1, now + Duration::from_millis(240)),
            Some(0.0)
        );
        assert_eq!(
            slide.opacity(2, now + Duration::from_millis(240)),
            Some(1.0)
        );
        assert_eq!(slide.opacity(3, now), None);
    }

    #[test]
    fn reverse_direction_and_extreme_coordinates_do_not_overflow() {
        let now = Instant::now();
        let slide = WorkspaceSlide::new(4, 1, 120, now);
        assert_eq!(
            slide.offset(4, 1000, now + Duration::from_millis(60)),
            Some(105)
        );
        assert_eq!(
            slide.offset(1, 1000, now + Duration::from_millis(60)),
            Some(-30)
        );
        assert_eq!(
            slide
                .slide_rect(
                    4,
                    Rect::new(i32::MAX, 0, 1, 1),
                    i32::MAX,
                    now + Duration::from_millis(120)
                )
                .unwrap()
                .x,
            i32::MAX
        );
        assert!(!slide.done(now));
    }
}
