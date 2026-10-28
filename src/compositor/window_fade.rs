//! Bounded entry fade for newly mapped toplevels. Geometry, focus and client
//! configure timing remain independent of this visual-only transition.

use std::time::{Duration, Instant};

pub(super) struct WindowFade {
    started: Instant,
    duration: Duration,
}

impl WindowFade {
    pub(super) fn new(now: Instant, ms: u32) -> Self {
        Self {
            started: now,
            duration: Duration::from_millis(u64::from(ms)),
        }
    }

    pub(super) fn opacity(&self, now: Instant) -> f32 {
        let t = (now.saturating_duration_since(self.started).as_secs_f32()
            / self.duration.as_secs_f32())
        .clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    }

    pub(super) fn done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= self.duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fade_is_bounded_and_finishes() {
        let now = Instant::now();
        let fade = WindowFade::new(now, 160);
        assert_eq!(fade.opacity(now), 0.0);
        assert!((fade.opacity(now + Duration::from_millis(80)) - 0.5).abs() < 0.001);
        assert_eq!(fade.opacity(now + Duration::from_millis(160)), 1.0);
        assert_eq!(fade.opacity(now + Duration::from_millis(400)), 1.0);
        assert!(fade.done(now + Duration::from_millis(160)));
    }
}
