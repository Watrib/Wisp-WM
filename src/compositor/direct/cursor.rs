//! Software cursor rendering for the direct backend.
//!
//! This backend composes the cursor into the scene alongside client surfaces.
//! Only the first frame of animated Xcursor themes is used for now.

use std::{collections::HashMap, env, fs};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            },
            gles::{GlesError, GlesRenderer},
        },
    },
    input::pointer::CursorIcon,
    utils::{Logical, Physical, Point, Transform},
};
use tracing::warn;
use xcursor::{
    CursorTheme,
    parser::{Image, parse_xcursor},
};

pub(super) struct SoftwareCursor {
    theme: CursorTheme,
    size: u32,
    cache: HashMap<String, Vec<Image>>,
    current: Option<CursorIcon>,
    buffer: Option<MemoryRenderBuffer>,
    hotspot: Point<i32, Physical>,
}

impl SoftwareCursor {
    pub fn load() -> Self {
        let theme_name = env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
        let size = env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|size| size.parse::<u32>().ok())
            .filter(|size| *size > 0)
            .unwrap_or(24);
        Self {
            theme: CursorTheme::load(&theme_name),
            size,
            cache: HashMap::new(),
            current: None,
            buffer: None,
            hotspot: Point::from((0, 0)),
        }
    }

    /// Update the visible named cursor. Client cursor surfaces bypass this and
    /// are rendered by the shared scene.
    pub fn set_icon(&mut self, icon: Option<CursorIcon>) {
        if self.current == icon {
            return;
        }
        self.current = icon;
        let Some(icon) = icon else {
            self.buffer = None;
            self.hotspot = Point::from((0, 0));
            return;
        };
        let size = self.size;
        let image = self
            .images(icon)
            .and_then(|images| pick_image(images, size))
            .cloned()
            .unwrap_or_else(|| fallback_image(size));
        self.buffer = Some(MemoryRenderBuffer::from_slice(
            &image.pixels_rgba,
            Fourcc::Argb8888,
            (image.width as i32, image.height as i32),
            1,
            Transform::Normal,
            None,
        ));
        self.hotspot = Point::from((image.xhot as i32, image.yhot as i32));
    }

    pub fn element(
        &self,
        renderer: &mut GlesRenderer,
        pointer: Point<f64, Logical>,
    ) -> Result<Option<MemoryRenderBufferRenderElement<GlesRenderer>>, GlesError> {
        let Some(buffer) = self.buffer.as_ref() else {
            return Ok(None);
        };
        let location = pointer.to_physical(1.0) - self.hotspot.to_f64();
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            buffer,
            None,
            None,
            None,
            Kind::Cursor,
        )
        .map(Some)
    }

    fn images(&mut self, icon: CursorIcon) -> Option<&[Image]> {
        let mut found = None;
        for name in std::iter::once(icon.name()).chain(icon.alt_names().iter().copied()) {
            if !self.cache.contains_key(name) {
                let images = self
                    .theme
                    .load_icon(name)
                    .and_then(|path| fs::read(path).ok())
                    .and_then(|data| parse_xcursor(&data))
                    .unwrap_or_default();
                // Cache misses too, avoiding repeated lookups for absent aliases.
                self.cache.insert(name.to_owned(), images);
            }
            if self
                .cache
                .get(name)
                .is_some_and(|images| !images.is_empty())
            {
                found = Some(name);
                break;
            }
        }
        if found.is_none() {
            warn!(?icon, "cursor theme has no image for shape; using fallback");
        }
        found
            .and_then(|name| self.cache.get(name))
            .map(Vec::as_slice)
    }
}

/// Choose the frame of the nearest nominal size. Animated themes use their
/// first frame; the compositor does not schedule cursor animation yet.
fn pick_image(images: &[Image], size: u32) -> Option<&Image> {
    let nearest = images
        .iter()
        .min_by_key(|image| (i64::from(image.size) - i64::from(size)).abs())?;
    images
        .iter()
        .find(|image| image.width == nearest.width && image.height == nearest.height)
}

/// A minimal premultiplied arrow used when no theme can be loaded. Keeping a
/// built-in copy guarantees that the pointer is visible on a bare VT.
fn fallback_image(size: u32) -> Image {
    const MASK: &[&str] = &[
        "X                       ",
        "XX                      ",
        "X.X                     ",
        "X..X                    ",
        "X...X                   ",
        "X....X                  ",
        "X.....X                 ",
        "X......X                ",
        "X.......X               ",
        "X........X              ",
        "X.........X             ",
        "X..........X            ",
        "X...........X           ",
        "X............X          ",
        "X.....XXXXXXX           ",
        "X..X....X               ",
        "X.X X....X              ",
        "XX  X....X              ",
        "X    X....X             ",
        "     X....X             ",
        "      X....X            ",
        "      X....X            ",
        "       X....X           ",
        "       XXXXXX           ",
    ];
    let width = 24usize;
    let height = MASK.len();
    let mut pixels = vec![0u8; width * height * 4];
    for (y, row) in MASK.iter().enumerate() {
        for (x, pixel) in row.bytes().enumerate() {
            let color = match pixel {
                b'X' => [0, 0, 0, 255],
                b'.' => [255, 255, 255, 255],
                _ => continue,
            };
            let index = (y * width + x) * 4;
            pixels[index..index + 4].copy_from_slice(&color);
        }
    }
    Image {
        size,
        width: width as u32,
        height: height as u32,
        xhot: 0,
        yhot: 0,
        delay: 1,
        pixels_rgba: pixels,
        pixels_argb: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(size: u32, width: u32) -> Image {
        Image {
            size,
            width,
            height: width,
            xhot: 0,
            yhot: 0,
            delay: 1,
            pixels_rgba: vec![0; (width * width * 4) as usize],
            pixels_argb: Vec::new(),
        }
    }

    #[test]
    fn picks_the_image_closest_to_the_requested_size() {
        let images = [image(16, 16), image(24, 24), image(48, 48)];
        assert_eq!(pick_image(&images, 22).unwrap().size, 24);
        assert_eq!(pick_image(&images, 40).unwrap().size, 48);
    }

    #[test]
    fn picks_the_first_frame_of_the_nearest_size() {
        let mut images = [image(24, 24), image(24, 24), image(48, 48)];
        images[0].delay = 17;
        images[1].delay = 31;
        assert_eq!(pick_image(&images, 24).unwrap().delay, 17);
    }

    #[test]
    fn fallback_cursor_has_visible_pixels_and_a_tip_hotspot() {
        let fallback = fallback_image(24);
        assert_eq!(fallback.xhot, 0);
        assert_eq!(fallback.yhot, 0);
        assert!(fallback.pixels_rgba.chunks_exact(4).any(|p| p[3] > 0));
    }
}
