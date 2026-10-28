//! Pure, bounded geometry for a current-workspace window overview.

use crate::layout::Rect;

pub(super) fn thumbnails(viewport: Rect, windows: &[Rect]) -> Vec<Rect> {
    if windows.is_empty() {
        return Vec::new();
    }
    let count = windows.len();
    let columns = (count as f64).sqrt().ceil() as i32;
    let rows = (count as i32 + columns - 1) / columns;
    let area = viewport.inset(24);
    let cell_w = (area.width / columns).max(1);
    let cell_h = (area.height / rows).max(1);
    windows
        .iter()
        .enumerate()
        .map(|(index, window)| {
            let column = index as i32 % columns;
            let row = index as i32 / columns;
            let cell_x = area.x + column * cell_w;
            let cell_y = area.y + row * cell_h;
            let available_w = (cell_w - 24).max(1);
            let available_h = (cell_h - 24).max(1);
            let scale = (f64::from(available_w) / f64::from(window.width))
                .min(f64::from(available_h) / f64::from(window.height))
                .min(1.0);
            let w = (f64::from(window.width) * scale).round().max(1.0) as i32;
            let h = (f64::from(window.height) * scale).round().max(1.0) as i32;
            Rect::new(cell_x + (cell_w - w) / 2, cell_y + (cell_h - h) / 2, w, h)
        })
        .collect()
}

/// Identify a thumbnail without using the compositor's untransformed client
/// hitboxes. Returning `None` for the gap leaves overview open on empty clicks.
pub(super) fn hit(items: &[(u64, Rect)], x: f64, y: f64) -> Option<u64> {
    items.iter().find_map(|(id, rect)| {
        (x >= f64::from(rect.x)
            && y >= f64::from(rect.y)
            && x < f64::from(rect.x + rect.width)
            && y < f64::from(rect.y + rect.height))
        .then_some(*id)
    })
}

/// Cycle in thumbnail order, including wraparound and absent/stale selection.
pub(super) fn cycle(items: &[(u64, Rect)], selected: Option<u64>, backwards: bool) -> Option<u64> {
    super::navigation::cycle(items.iter().map(|(id, _)| *id), selected, backwards)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overview_is_empty_without_windows() {
        assert!(thumbnails(Rect::new(0, 0, 1280, 800), &[]).is_empty());
    }

    #[test]
    fn keyboard_selection_wraps_and_recovers_from_missing_windows() {
        let items = [
            (12, Rect::new(0, 0, 100, 100)),
            (44, Rect::new(120, 0, 100, 100)),
        ];
        assert_eq!(cycle(&items, Some(12), false), Some(44));
        assert_eq!(cycle(&items, Some(44), false), Some(12));
        assert_eq!(cycle(&items, Some(12), true), Some(44));
        assert_eq!(cycle(&items, Some(44), true), Some(12));
        for selected in [None, Some(99)] {
            assert_eq!(cycle(&items, selected, false), Some(12));
            assert_eq!(cycle(&items, selected, true), Some(44));
        }
        for backwards in [false, true] {
            assert_eq!(cycle(&[], Some(12), backwards), None);
            assert_eq!(cycle(&items[..1], Some(12), backwards), Some(12));
        }
    }

    #[test]
    fn thumbnails_fit_aspect_ratio_and_do_not_overlap() {
        let viewport = Rect::new(0, 0, 1280, 800);
        let windows = [
            Rect::new(0, 0, 1000, 500),
            Rect::new(0, 0, 500, 900),
            Rect::new(0, 0, 1280, 720),
            Rect::new(0, 0, 400, 400),
        ];
        let thumbs = thumbnails(viewport, &windows);
        assert_eq!(thumbs.len(), windows.len());
        for (i, rect) in thumbs.iter().enumerate() {
            assert!(rect.width > 0 && rect.height > 0);
            assert!(rect.x >= viewport.x && rect.y >= viewport.y);
            assert!(rect.x + rect.width <= viewport.x + viewport.width);
            assert!(rect.y + rect.height <= viewport.y + viewport.height);
            assert!(rect.width <= windows[i].width && rect.height <= windows[i].height);
            for other in &thumbs[i + 1..] {
                assert!(
                    rect.x + rect.width <= other.x
                        || other.x + other.width <= rect.x
                        || rect.y + rect.height <= other.y
                        || other.y + other.height <= rect.y
                );
            }
        }
    }

    #[test]
    fn overview_hit_testing_uses_thumbnail_rectangles_not_original_window_rects() {
        let items = [
            (12, Rect::new(100, 100, 200, 150)),
            (44, Rect::new(350, 100, 200, 150)),
        ];
        assert_eq!(hit(&items, 150.0, 120.0), Some(12));
        assert_eq!(hit(&items, 500.0, 249.0), Some(44));
        assert_eq!(hit(&items, 300.0, 120.0), None);
        assert_eq!(hit(&items, f64::NAN, 120.0), None);
    }
}
