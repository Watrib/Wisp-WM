//! Critically damped, visual-only geometry animation. Client configures and
//! pointer hit-testing always use the final logical rectangle.

use std::time::Instant;

use crate::layout::Rect;

#[derive(Clone, Copy, Debug)]
struct Axis {
    position: f64,
    velocity: f64,
    target: f64,
}

impl Axis {
    fn new(position: i32, target: i32) -> Self {
        Self {
            position: f64::from(position),
            velocity: 0.0,
            target: f64::from(target),
        }
    }

    fn step(&mut self, dt: f64, omega: f64) {
        let delta = self.position - self.target;
        let impulse = self.velocity + omega * delta;
        let decay = (-omega * dt).exp();
        self.position = self.target + (delta + impulse * dt) * decay;
        self.velocity = (self.velocity - omega * impulse * dt) * decay;
        if (self.position - self.target).abs() < 0.5 && self.velocity.abs() < 2.0 {
            self.position = self.target;
            self.velocity = 0.0;
        }
    }

    fn settled(self) -> bool {
        self.position == self.target && self.velocity == 0.0
    }
}

/// Four linked axes retain their velocity when the target changes mid-flight.
#[derive(Clone, Debug)]
pub(super) struct WindowSpring {
    axes: [Axis; 4],
    last: Instant,
}

impl WindowSpring {
    pub(super) fn new(from: Rect, to: Rect, now: Instant) -> Self {
        Self {
            axes: [
                Axis::new(from.x, to.x),
                Axis::new(from.y, to.y),
                Axis::new(from.width, to.width),
                Axis::new(from.height, to.height),
            ],
            last: now,
        }
    }

    pub(super) fn retarget(&mut self, to: Rect, now: Instant, frequency_hz: f64) {
        self.advance(now, frequency_hz);
        for (axis, target) in self.axes.iter_mut().zip([to.x, to.y, to.width, to.height]) {
            axis.target = f64::from(target);
        }
    }

    pub(super) fn advance(&mut self, now: Instant, frequency_hz: f64) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        let omega = std::f64::consts::TAU * frequency_hz;
        for axis in &mut self.axes {
            axis.step(dt, omega);
        }
    }

    pub(super) fn settled(&self) -> bool {
        self.axes.iter().all(|axis| axis.settled())
    }

    pub(super) fn rect(&self) -> Rect {
        let [x, y, width, height] = self.axes.map(|axis| axis.position.round() as i32);
        Rect::new(x, y, width, height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn spring_starts_at_source_and_converges_to_exact_target() {
        let now = Instant::now();
        let from = Rect::new(10, 20, 300, 500);
        let to = Rect::new(110, 40, 600, 300);
        let mut spring = WindowSpring::new(from, to, now);
        assert_eq!(spring.rect(), from);
        assert!(!spring.settled());
        spring.advance(now + Duration::from_millis(80), 4.0);
        assert!(spring.rect().x > from.x && spring.rect().x < to.x);
        spring.advance(now + Duration::from_secs(2), 4.0);
        assert!(spring.settled());
        assert_eq!(spring.rect(), to);
    }

    #[test]
    fn retarget_preserves_continuous_position_and_velocity() {
        let now = Instant::now();
        let mut spring =
            WindowSpring::new(Rect::new(0, 0, 800, 600), Rect::new(50, 0, 400, 600), now);
        let next = now + Duration::from_millis(50);
        spring.advance(next, 4.0);
        let before = spring.rect();
        let velocity = spring.axes[0].velocity;
        spring.retarget(Rect::new(100, 0, 300, 600), next, 4.0);
        assert_eq!(spring.rect(), before);
        assert_eq!(spring.axes[0].velocity, velocity);
        spring.advance(next + Duration::from_secs(2), 4.0);
        assert_eq!(spring.rect(), Rect::new(100, 0, 300, 600));
    }
}
