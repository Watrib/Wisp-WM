//! Keyboard navigation over existing window/thumbnail geometry. Candidate
//! selection streams through iterators; no temporary window-ID lists.

use crate::layout::{Direction, LayoutKind, Rect};

use super::Wisp;

/// Wrap through eligible IDs. Missing focus starts at the first/last window.
pub(super) fn cycle(
    ids: impl IntoIterator<Item = u64>,
    selected: Option<u64>,
    backwards: bool,
) -> Option<u64> {
    let mut first = None;
    let mut previous = None;
    let mut found = false;
    for id in ids {
        first.get_or_insert(id);
        if found && !backwards {
            return Some(id);
        }
        if Some(id) == selected {
            if backwards && previous.is_some() {
                return previous;
            }
            found = true;
        }
        previous = Some(id);
    }
    if backwards { previous } else { first }
}

/// Prefer neighbors overlapping the perpendicular axis, then the closest edge,
/// then perpendicular/forward center distance. Window ID breaks exact ties.
/// Doubled centers keep the comparison integral; i64 handles all i32 rectangles.
fn neighbor(
    origin: Rect,
    candidates: impl IntoIterator<Item = (u64, Rect)>,
    direction: Direction,
) -> Option<u64> {
    let axes = |rect: Rect| {
        if matches!(direction, Direction::Left | Direction::Right) {
            (
                i64::from(rect.x),
                i64::from(rect.width),
                i64::from(rect.y),
                i64::from(rect.height),
            )
        } else {
            (
                i64::from(rect.y),
                i64::from(rect.height),
                i64::from(rect.x),
                i64::from(rect.width),
            )
        }
    };
    let (a, length, b, breadth) = axes(origin);
    let positive = matches!(direction, Direction::Right | Direction::Down);
    candidates
        .into_iter()
        .filter_map(|(id, rect)| {
            let (ca, clength, cb, cbreadth) = axes(rect);
            let forward = (2 * ca + clength) - (2 * a + length);
            let forward = if positive { forward } else { -forward };
            if forward <= 0 {
                return None;
            }
            let aligned = b < cb + cbreadth && cb < b + breadth;
            let edge_gap = if positive {
                ca - (a + length)
            } else {
                a - (ca + clength)
            }
            .max(0);
            let sideways = ((2 * cb + cbreadth) - (2 * b + breadth)).abs();
            Some(((!aligned, edge_gap, sideways, forward, id), id))
        })
        .min_by_key(|(score, _)| *score)
        .map(|(_, id)| id)
}

impl Wisp {
    pub(super) fn focus_direction(&mut self, direction: Direction) {
        if self.overview_exiting {
            return;
        }
        let Some(selected) = self.focused else {
            self.focus_fallback();
            return;
        };
        let target = if self.overview {
            let Some((_, origin)) = self.overview_items().iter().find(|(id, _)| *id == selected)
            else {
                self.focus_fallback();
                return;
            };
            neighbor(
                *origin,
                self.overview_items()
                    .iter()
                    .copied()
                    .filter(|(id, _)| *id != selected),
                direction,
            )
        } else {
            let Some(origin) = self
                .windows
                .iter()
                .find(|w| w.id == selected && self.focusable(w))
            else {
                self.focus_fallback();
                return;
            };
            neighbor(
                origin.rect,
                self.windows
                    .iter()
                    .filter(|w| w.id != selected && self.focusable(w))
                    .map(|w| (w.id, w.rect)),
                direction,
            )
        };
        if let Some(id) = target {
            self.focus(Some(id));
        }
    }

    pub(super) fn swap_direction(&mut self, direction: Direction) {
        if self.overview || self.overview_exiting || self.layout() == LayoutKind::Floating {
            return;
        }
        let eligible = |w: &super::ManagedWindow| self.focusable(w) && !w.floating && !w.fullscreen;
        let Some(index) = self
            .windows
            .iter()
            .position(|w| Some(w.id) == self.focused && eligible(w))
        else {
            return;
        };
        let origin = &self.windows[index];
        let target = neighbor(
            origin.rect,
            self.windows
                .iter()
                .filter(|w| w.id != origin.id && eligible(w))
                .map(|w| (w.id, w.rect)),
            direction,
        );
        if let Some(other) = target.and_then(|id| self.windows.iter().position(|w| w.id == id)) {
            self.windows.swap(index, other);
            self.arrange();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycling_wraps_and_handles_missing_focus_without_skipping_first() {
        assert_eq!(cycle([4, 7, 9], Some(4), false), Some(7));
        assert_eq!(cycle([4, 7, 9], Some(9), false), Some(4));
        assert_eq!(cycle([4, 7, 9], Some(4), true), Some(9));
        assert_eq!(cycle([4, 7, 9], Some(9), true), Some(7));
        for selected in [None, Some(55)] {
            assert_eq!(cycle([4, 7, 9], selected, false), Some(4));
            assert_eq!(cycle([4, 7, 9], selected, true), Some(9));
        }
        for backwards in [false, true] {
            assert_eq!(cycle([], Some(4), backwards), None);
            assert_eq!(cycle([4], Some(4), backwards), Some(4));
        }
    }

    #[test]
    fn neighbors_prefer_alignment_and_do_not_wrap_at_edges() {
        let origin = Rect::new(100, 100, 100, 100);
        let candidates = [
            (1, Rect::new(0, 100, 80, 100)),
            (2, Rect::new(220, 300, 100, 100)),
            (3, Rect::new(240, 100, 100, 100)),
            (4, Rect::new(100, -20, 100, 100)),
            (5, Rect::new(100, 220, 100, 100)),
        ];
        assert_eq!(neighbor(origin, candidates, Direction::Left), Some(1));
        assert_eq!(neighbor(origin, candidates, Direction::Right), Some(3));
        assert_eq!(neighbor(origin, candidates, Direction::Up), Some(4));
        assert_eq!(neighbor(origin, candidates, Direction::Down), Some(5));
        assert_eq!(neighbor(origin, [candidates[0]], Direction::Right), None);
    }

    #[test]
    fn neighbors_have_stable_ties_and_accept_negative_and_extreme_coordinates() {
        let origin = Rect::new(-200, -100, 100, 100);
        let target = Rect::new(0, -100, 100, 100);
        assert_eq!(
            neighbor(origin, [(8, target), (3, target)], Direction::Right),
            Some(3)
        );
        assert_eq!(
            neighbor(origin, [(3, target), (8, target)], Direction::Right),
            Some(3)
        );
        let low = Rect::new(i32::MIN, i32::MIN, i32::MAX, i32::MAX);
        let high = Rect::new(i32::MAX, i32::MAX, i32::MAX, i32::MAX);
        assert_eq!(neighbor(low, [(1, high)], Direction::Right), Some(1));
        assert_eq!(neighbor(high, [(1, low)], Direction::Up), Some(1));
    }
}
