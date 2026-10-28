use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum LayoutKind {
    #[default]
    Dwindle,
    Master,
    Floating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width: width.max(1),
            height: height.max(1),
        }
    }

    pub fn inset(self, amount: i32) -> Self {
        let dx = amount.max(0).min((self.width - 1) / 2);
        let dy = amount.max(0).min((self.height - 1) / 2);
        Self::new(
            self.x + dx,
            self.y + dy,
            self.width - 2 * dx,
            self.height - 2 * dy,
        )
    }
}

/// Compute the tiled viewport; smart gaps affect only the one-tile case.
/// Floating-only layouts never consume a tile, so their gap policy stays
/// independent of this function.
pub fn tiled_area(viewport: Rect, count: usize, outer_gap: i32, smart_gaps: bool) -> Rect {
    if smart_gaps && count == 1 {
        viewport
    } else {
        viewport.inset(outer_gap)
    }
}

/// Pure geometry: no Wayland objects, allocations other than the result, or renderer calls.
pub fn arrange(kind: LayoutKind, area: Rect, count: usize, gap: i32, ratio: f64) -> Vec<Rect> {
    if count == 0 || kind == LayoutKind::Floating {
        return Vec::new();
    }
    if count == 1 {
        return vec![area];
    }
    let mut result = Vec::with_capacity(count);
    let ratio = if ratio.is_finite() {
        ratio.clamp(0.1, 0.9)
    } else {
        0.5
    };
    match kind {
        LayoutKind::Dwindle => {
            let mut remaining = area;
            for _ in 0..count - 1 {
                let (first, rest) =
                    split(remaining, remaining.width >= remaining.height, gap, ratio);
                result.push(first);
                remaining = rest;
            }
            result.push(remaining);
        }
        LayoutKind::Master => {
            let (master, stack) = split(area, true, gap, ratio);
            result.push(master);
            let n = (count - 1) as i32;
            let gap = gap.max(0).min(((stack.height - n) / (n - 1).max(1)).max(0));
            let usable = (stack.height - gap * (n - 1)).max(1);
            for i in 0..n {
                let start = usable * i / n;
                let end = usable * (i + 1) / n;
                let y = (start + gap * i).min(stack.height - 1);
                result.push(Rect::new(
                    stack.x,
                    stack.y + y,
                    stack.width,
                    (end - start).max(1).min(stack.height - y),
                ));
            }
        }
        LayoutKind::Floating => unreachable!(),
    }
    result
}

fn split(area: Rect, horizontal: bool, gap: i32, ratio: f64) -> (Rect, Rect) {
    let dimension = if horizontal { area.width } else { area.height };
    if dimension < 2 {
        // There is no non-overlapping solution when a tile is smaller than two pixels.
        return (area, area);
    }
    let gap = gap.max(0).min(dimension - 2);
    let usable = dimension - gap;
    let first = ((usable as f64 * ratio).round() as i32).clamp(1, usable - 1);
    if horizontal {
        (
            Rect::new(area.x, area.y, first, area.height),
            Rect::new(area.x + first + gap, area.y, usable - first, area.height),
        )
    } else {
        (
            Rect::new(area.x, area.y, area.width, first),
            Rect::new(area.x, area.y + first + gap, area.width, usable - first),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_gaps_only_remove_outer_gap_for_one_tiled_window() {
        let viewport = Rect::new(5, 7, 1000, 800);
        let gapped = viewport.inset(12);
        assert_eq!(tiled_area(viewport, 0, 12, true), gapped);
        assert_eq!(tiled_area(viewport, 1, 12, true), viewport);
        assert_eq!(tiled_area(viewport, 2, 12, true), gapped);
        assert_eq!(tiled_area(viewport, 1, 12, false), gapped);
        assert_eq!(tiled_area(viewport, 1, 0, true), viewport);
        assert_eq!(tiled_area(viewport, 1, 128, true), viewport);
    }

    #[test]
    fn dwindle_splits_the_remaining_tile() {
        assert_eq!(
            arrange(LayoutKind::Dwindle, Rect::new(0, 0, 1000, 800), 3, 0, 0.5),
            vec![
                Rect::new(0, 0, 500, 800),
                Rect::new(500, 0, 500, 400),
                Rect::new(500, 400, 500, 400),
            ]
        );
    }

    #[test]
    fn master_accounts_for_gaps_and_rounding() {
        let tiles = arrange(LayoutKind::Master, Rect::new(10, 20, 1001, 801), 4, 9, 0.6);
        assert_eq!(tiles.len(), 4);
        assert_eq!(tiles[1].y, 20);
        assert_eq!(tiles[3].y + tiles[3].height, 821);
        assert_eq!(tiles[0].x + tiles[0].width + 9, tiles[1].x);
    }

    #[test]
    fn normal_tiles_are_bounded_and_do_not_overlap() {
        for kind in [LayoutKind::Dwindle, LayoutKind::Master] {
            for count in 1..=12 {
                let tiles = arrange(kind, Rect::new(0, 0, 1920, 1080), count, 8, 0.5);
                assert_eq!(tiles.len(), count);
                for (i, a) in tiles.iter().enumerate() {
                    assert!(a.width > 0 && a.height > 0 && a.x >= 0 && a.y >= 0);
                    assert!(a.x + a.width <= 1920 && a.y + a.height <= 1080);
                    for b in &tiles[i + 1..] {
                        assert!(
                            a.x + a.width <= b.x
                                || b.x + b.width <= a.x
                                || a.y + a.height <= b.y
                                || b.y + b.height <= a.y
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn tiny_outputs_and_extreme_gaps_never_produce_invalid_sizes() {
        for kind in [LayoutKind::Dwindle, LayoutKind::Master] {
            for width in 1..20 {
                for height in 1..20 {
                    let area = Rect::new(0, 0, width, height).inset(100);
                    for tile in arrange(kind, area, 5, 1000, f64::NAN) {
                        assert!(tile.width > 0 && tile.height > 0);
                        assert!(tile.x + tile.width <= width && tile.y + tile.height <= height);
                    }
                }
            }
        }
    }

    #[test]
    fn empty_and_floating_layouts_have_no_tiles() {
        let area = Rect::new(0, 0, 1280, 800);
        assert!(arrange(LayoutKind::Dwindle, area, 0, 8, 0.5).is_empty());
        assert!(arrange(LayoutKind::Floating, area, 3, 8, 0.5).is_empty());
    }
}
