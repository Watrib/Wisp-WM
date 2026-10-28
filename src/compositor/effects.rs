//! Window-decoration render effects: rounded corners, gradient borders, and
//! soft shadows (phase 1 of the effects roadmap).
//!
//! Everything here is GPU-shaped: the rounded-corner path overrides the
//! default texture program of a window's surface elements, and border/shadow
//! are composed in custom pixel-shader elements. Compilation failures degrade
//! to no effect with a warning instead of failing the session; scenes then
//! fall back to the plain decoration path.
//!
//! Limitations documented for honesty:
//! - surfaces drawn as solid colors (single-pixel-buffer windows) bypass the
//!   texture-program override and are not corner-clipped;
//! - y-inverted buffer textures receive a mirrored rounding mask (Smithay 0.7
//!   does not expose the buffer flag publicly for surface elements).

use smithay::backend::renderer::element::{
    Element, Id, Kind, RenderElement, UnderlyingStorage, surface::WaylandSurfaceRenderElement,
};
use smithay::backend::renderer::gles::{
    GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexProgram, Uniform, UniformName,
    UniformType, UniformValue, element::PixelShaderElement,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use std::sync::Arc;
use tracing::warn;

use super::blur::{BlurPrograms, BlurTextures};

const CLIP_FRAG: &str = include_str!("effects_clip.frag");
const BORDER_FRAG: &str = include_str!("effects_border.frag");
const SHADOW_FRAG: &str = include_str!("effects_shadow.frag");
/// Shared helper functions appended after every effect fragment shader.
const SHADER_HELPERS: &str = include_str!("effects_shaders.glsl");

/// Compiled effect programs for one renderer context.
#[derive(Debug, Default)]
pub(crate) struct EffectsShaders {
    /// Override program drawing a texture clipped to a rounded rectangle.
    pub(super) rounded: Option<GlesTexProgram>,
    /// Pixel program drawing the rounded border ring.
    pub(super) border: Option<GlesPixelProgram>,
    /// Pixel program drawing a gaussian soft shadow.
    pub(super) shadow: Option<GlesPixelProgram>,
    pub(super) blur: Option<Arc<BlurPrograms>>,
    blur_attempted: bool,
}

impl EffectsShaders {
    /// Texture-override program source (rounding helpers before main).
    fn texture_program_text() -> String {
        format!("#version 100\n//_DEFINES_\n{SHADER_HELPERS}\n{CLIP_FRAG}")
    }

    fn border_program_text() -> String {
        format!("{SHADER_HELPERS}\n{BORDER_FRAG}")
    }

    fn shadow_program_text() -> String {
        format!("{SHADER_HELPERS}\n{SHADOW_FRAG}")
    }

    /// Compile all programs. Individual failures warn and disable that effect
    /// only; the rest keep working.
    pub(super) fn compile(renderer: &mut GlesRenderer) -> Self {
        let rounded = renderer
            .compile_custom_texture_shader(
                Self::texture_program_text(),
                &[
                    UniformName::new("geo_size", UniformType::_2f),
                    UniformName::new("corner_radius", UniformType::_4f),
                    UniformName::new("input_to_geo", UniformType::Matrix3x3),
                ],
            )
            .map_err(|error| warn!("compiling rounded-corner shader: {error}"))
            .ok();

        let border = renderer
            .compile_custom_pixel_shader(
                Self::border_program_text(),
                &[
                    UniformName::new("color_from", UniformType::_4f),
                    UniformName::new("color_to", UniformType::_4f),
                    UniformName::new("grad_vec", UniformType::_2f),
                    UniformName::new("geo_size", UniformType::_2f),
                    UniformName::new("outer_radius", UniformType::_4f),
                    UniformName::new("border_width", UniformType::_1f),
                ],
            )
            .map_err(|error| warn!("compiling border shader: {error}"))
            .ok();

        let shadow = renderer
            .compile_custom_pixel_shader(
                Self::shadow_program_text(),
                &[
                    UniformName::new("shadow_color", UniformType::_4f),
                    UniformName::new("sigma", UniformType::_1f),
                    UniformName::new("window_offset", UniformType::_2f),
                    UniformName::new("window_size", UniformType::_2f),
                    UniformName::new("radius", UniformType::_4f),
                ],
            )
            .map_err(|error| warn!("compiling shadow shader: {error}"))
            .ok();

        Self {
            rounded,
            border,
            shadow,
            blur: None,
            blur_attempted: false,
        }
    }

    pub(super) fn ensure_blur(&mut self, renderer: &mut GlesRenderer) {
        if !self.blur_attempted {
            self.blur_attempted = true;
            self.blur = BlurPrograms::compile(renderer)
                .map_err(|error| warn!(%error, "compiling Kawase blur shader"))
                .ok();
        }
    }
}

/// Column-major 3x3 matrix (GL upload layout: [c0r0, c0r1, c0r2, c1r0, ...]).
#[derive(Debug, Clone, Copy)]
struct Mat3([f32; 9]);

impl Mat3 {
    fn translation(x: f32, y: f32) -> Self {
        Self([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, x, y, 1.0])
    }

    fn scale(x: f32, y: f32) -> Self {
        Self([x, 0.0, 0.0, 0.0, y, 0.0, 0.0, 0.0, 1.0])
    }

    /// Matrix product `self * other` (apply `other` first, GL convention).
    fn mul(&self, other: &Self) -> Self {
        let mut out = [0.0f32; 9];
        for column in 0..3 {
            for row in 0..3 {
                out[column * 3 + row] = (0..3)
                    .map(|k| self.0[k * 3 + row] * other.0[column * 3 + k])
                    .sum();
            }
        }
        Self(out)
    }

    fn uniform(&self) -> Uniform<'static> {
        Uniform::new(
            "input_to_geo",
            UniformValue::Matrix3x3 {
                matrices: vec![self.0],
                transpose: false,
            },
        )
    }
}

/// 3x3 matrix with the 2x2 transform block as its columns; cell values mirror
/// Smithay's `Transform::matrix()` (cgmath column values).
fn mat3_from_transform(transform: Transform) -> Mat3 {
    let (column0, column1): ([f32; 2], [f32; 2]) = match transform {
        Transform::Normal => ([1.0, 0.0], [0.0, 1.0]),
        Transform::_90 => ([0.0, -1.0], [1.0, 0.0]),
        Transform::_180 => ([-1.0, 0.0], [0.0, -1.0]),
        Transform::_270 => ([0.0, 1.0], [-1.0, 0.0]),
        Transform::Flipped => ([-1.0, 0.0], [0.0, 1.0]),
        Transform::Flipped90 => ([0.0, 1.0], [1.0, 0.0]),
        Transform::Flipped180 => ([1.0, 0.0], [0.0, -1.0]),
        Transform::Flipped270 => ([0.0, -1.0], [-1.0, 0.0]),
    };
    Mat3([
        column0[0], column0[1], 0.0, column1[0], column1[1], 0.0, 0.0, 0.0, 1.0,
    ])
}

/// A wayland surface render element drawn through the rounded-corner texture
/// program. Element metadata delegates to the inner element so the damage
/// tracker keeps recognizing it across frames.
#[derive(Debug)]
pub(super) struct ClippedWindowSurface {
    inner: WaylandSurfaceRenderElement<GlesRenderer>,
    program: GlesTexProgram,
    geometry: Rectangle<f64, Logical>,
    radius: [f32; 4],
    scale: f32,
}

impl ClippedWindowSurface {
    pub(super) fn new(
        inner: WaylandSurfaceRenderElement<GlesRenderer>,
        program: GlesTexProgram,
        geometry: Rectangle<f64, Logical>,
        radius: [f32; 4],
        scale: f32,
    ) -> Self {
        Self {
            inner,
            program,
            geometry,
            radius,
            scale,
        }
    }

    fn compute_uniforms(&self) -> Vec<Uniform<'static>> {
        let scale = Scale::from(f64::from(self.scale));
        let elem_geo = self.inner.geometry(scale);
        let elem_geo_size = [elem_geo.size.w as f32, elem_geo.size.h as f32];
        let elem_geo_loc = [elem_geo.loc.x as f32, elem_geo.loc.y as f32];

        let geo = self.geometry.to_physical_precise_round::<f64, i32>(scale);
        let geo_loc = [geo.loc.x as f32, geo.loc.y as f32];
        let geo_size = [geo.size.w as f32, geo.size.h as f32];

        let buf_size = self.inner.buffer_size();
        let buf_size = [buf_size.w as f32, buf_size.h as f32];

        let view = self.inner.view();
        let src_loc = [view.src.loc.x as f32, view.src.loc.y as f32];
        let src_size = [view.src.size.w as f32, view.src.size.h as f32];

        // Rotation transforms and their flipped counterparts compose with the
        // texture sampling order differently; swapping keeps rotation+flip
        // consistent with the fragment mapping.
        let transform = match self.inner.transform() {
            Transform::_90 => Transform::_270,
            Transform::_270 => Transform::_90,
            other => other,
        };
        let wrap = Mat3::translation(0.5, 0.5)
            .mul(&mat3_from_transform(transform))
            .mul(&Mat3::translation(-0.5, -0.5));
        let input_to_geo = wrap
            .mul(&Mat3::scale(
                elem_geo_size[0] / geo_size[0],
                elem_geo_size[1] / geo_size[1],
            ))
            .mul(&Mat3::translation(
                (elem_geo_loc[0] - geo_loc[0]) / elem_geo_size[0],
                (elem_geo_loc[1] - geo_loc[1]) / elem_geo_size[1],
            ))
            .mul(&Mat3::scale(
                buf_size[0] / src_size[0],
                buf_size[1] / src_size[1],
            ))
            .mul(&Mat3::translation(
                -src_loc[0] / buf_size[0],
                -src_loc[1] / buf_size[1],
            ));

        vec![
            Uniform::new("geo_size", UniformValue::_2f(geo_size[0], geo_size[1])),
            Uniform::new(
                "corner_radius",
                UniformValue::_4f(
                    self.radius[0],
                    self.radius[1],
                    self.radius[2],
                    self.radius[3],
                ),
            ),
            input_to_geo.uniform(),
        ]
    }

    fn corner_squares(
        geo: Rectangle<f64, Logical>,
        radius: [f32; 4],
    ) -> Vec<Rectangle<f64, Logical>> {
        let px = |r: f32| f64::from(r);
        let squares = [
            (geo.loc, px(radius[0])),
            (
                Point::from((geo.loc.x + geo.size.w - px(radius[1]), geo.loc.y)),
                px(radius[1]),
            ),
            (
                Point::from((
                    geo.loc.x + geo.size.w - px(radius[2]),
                    geo.loc.y + geo.size.h - px(radius[2]),
                )),
                px(radius[2]),
            ),
            (
                Point::from((geo.loc.x, geo.loc.y + geo.size.h - px(radius[3]))),
                px(radius[3]),
            ),
        ];
        squares
            .into_iter()
            .filter(|(_, r)| *r > 0.0)
            .map(|(loc, r)| Rectangle::new(loc, Size::from((r, r))))
            .collect()
    }
}

impl Element for ClippedWindowSurface {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        // Conservative: intersect with the clip window but do not subtract
        // corners; corner-side over-damage is cheaper than missing damage.
        let inner_geo = self.inner.geometry(scale);
        let mut geo = self.geometry.to_physical_precise_round(scale);
        geo.loc -= inner_geo.loc;
        self.inner
            .damage_since(scale, commit)
            .into_iter()
            .filter_map(|rect| rect.intersection(geo))
            .collect()
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        let regions = self.inner.opaque_regions(scale);
        if self.radius == [0.0; 4] {
            return regions;
        }
        let inner_geo_loc = self.inner.geometry(scale).loc;
        let mut geo = self.geometry.to_physical_precise_round(scale);
        geo.loc -= inner_geo_loc;
        let corners: Vec<Rectangle<i32, Physical>> =
            Self::corner_squares(self.geometry, self.radius)
                .into_iter()
                .map(|square| {
                    let square = square.to_physical_precise_up::<f64, i32>(scale);
                    let mut square = Rectangle::new(square.loc, square.size);
                    square.loc -= inner_geo_loc;
                    square
                })
                .collect();
        let regions = regions
            .into_iter()
            .filter_map(|region| region.intersection(geo));
        OpaqueRegions::from_slice(&Rectangle::subtract_rects_many(regions, corners))
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for ClippedWindowSurface {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        frame.override_default_tex_program(self.program.clone(), self.compute_uniforms());
        RenderElement::<GlesRenderer>::draw(&self.inner, frame, src, dst, damage, opaque_regions)?;
        frame.clear_tex_program_override();
        Ok(())
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

/// Persistent window decoration elements (border ring + shadow). Storing the
/// elements instead of rebuilding them every frame keeps element ids and
/// commit counters stable, so idle frames stay damage-free.
#[derive(Debug, Default)]
pub(crate) struct WindowDecor {
    border: Option<PixelShaderElement>,
    border_uniforms: Option<Vec<Uniform<'static>>>,
    shadow: Option<PixelShaderElement>,
    shadow_uniforms: Option<Vec<Uniform<'static>>>,
    pub(super) blur_textures: Option<BlurTextures>,
    pub(super) blur_id: Option<Id>,
    pub(super) blur_commit: CommitCounter,
    pub(super) blur_options: Option<(u8, f32)>,
}

impl WindowDecor {
    /// Update the persistent shadow element. Returns the element to draw.
    pub(super) fn sync_shadow(
        &mut self,
        program: &GlesPixelProgram,
        area: Rectangle<i32, Logical>,
        uniforms: &[Uniform<'static>],
    ) -> Option<&PixelShaderElement> {
        sync_element(
            &mut self.shadow,
            &mut self.shadow_uniforms,
            program,
            area,
            uniforms,
        )
    }

    /// Update the persistent border element. Returns the element to draw.
    pub(super) fn sync_border(
        &mut self,
        program: &GlesPixelProgram,
        area: Rectangle<i32, Logical>,
        uniforms: &[Uniform<'static>],
    ) -> Option<&PixelShaderElement> {
        sync_element(
            &mut self.border,
            &mut self.border_uniforms,
            program,
            area,
            uniforms,
        )
    }
}

/// Shared create-resize-update logic for persistent pixel-shader decorations.
/// Uniform values are only re-uploaded when they change so unchanged frames
/// produce no damage.
fn sync_element<'a>(
    element: &'a mut Option<PixelShaderElement>,
    last_uniforms: &mut Option<Vec<Uniform<'static>>>,
    program: &GlesPixelProgram,
    area: Rectangle<i32, Logical>,
    uniforms: &[Uniform<'static>],
) -> Option<&'a PixelShaderElement> {
    let changed_uniforms = last_uniforms.as_deref() != Some(uniforms);
    match element {
        Some(existing) => {
            existing.resize(area, None);
            if changed_uniforms {
                existing.update_uniforms(uniforms.to_vec());
                *last_uniforms = Some(uniforms.to_vec());
            }
        }
        None => {
            *element = Some(PixelShaderElement::new(
                program.clone(),
                area,
                None,
                1.0,
                uniforms.to_vec(),
                Kind::Unspecified,
            ));
            *last_uniforms = Some(uniforms.to_vec());
        }
    }
    element.as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_fragment_matches_texture_program_contract() {
        // `compile_custom_texture_shader` prepends the version line itself.
        assert!(!CLIP_FRAG.contains("#version"));
        // The renderer must see the defines marker once per program.
        assert_eq!(
            EffectsShaders::texture_program_text()
                .matches("//_DEFINES_")
                .count(),
            1
        );
        // The helper must be defined before main references it.
        let program = EffectsShaders::texture_program_text();
        let fragment_start = program
            .find(CLIP_FRAG)
            .expect("helpers precede the fragment");
        assert!(program[..fragment_start].contains("float wisp_rounding_alpha"));
        assert!(CLIP_FRAG.contains("wisp_rounding_alpha"));
    }

    #[test]
    fn every_compiled_uniform_is_declared_in_its_program() {
        let programs = [
            (
                EffectsShaders::texture_program_text(),
                vec![
                    "geo_size",
                    "corner_radius",
                    "input_to_geo",
                    "tex",
                    "alpha",
                    "v_coords",
                ],
            ),
            (
                EffectsShaders::border_program_text(),
                vec![
                    "color_from",
                    "color_to",
                    "grad_vec",
                    "geo_size",
                    "outer_radius",
                    "border_width",
                    "alpha",
                    "size",
                    "v_coords",
                ],
            ),
            (
                EffectsShaders::shadow_program_text(),
                vec![
                    "shadow_color",
                    "sigma",
                    "window_offset",
                    "window_size",
                    "radius",
                    "alpha",
                    "size",
                    "v_coords",
                ],
            ),
        ];
        for (program, names) in programs {
            for name in names {
                assert!(program.contains(name), "missing `{name}` in program");
            }
        }
    }

    #[test]
    fn pixel_shaders_have_no_version_line() {
        // Pixel programs get `#version 100` from the compiler.
        assert!(!BORDER_FRAG.contains("#version"));
        assert!(!SHADOW_FRAG.contains("#version"));
        for program in [
            EffectsShaders::border_program_text(),
            EffectsShaders::shadow_program_text(),
        ] {
            assert!(!program.contains("#version"));
        }
    }
}
