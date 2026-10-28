//! Optional dual-filter Kawase backdrop blur for normal toplevels.
//!
//! The element is drawn *after* the scene behind a window and *before* the
//! window's own surfaces. It copies that already-composited background from
//! the active framebuffer, downsamples it, then upsamples back to a texture.
//! Smithay draws the final texture with its own damage scissor. No CPU readback.

use std::{
    cmp::max,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use smithay::{
    backend::renderer::{
        Frame, Renderer, Texture,
        element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
        gles::{GlesError, GlesFrame, GlesRenderer, GlesTexture, ffi, link_program},
        utils::{CommitCounter, DamageSet, OpaqueRegions},
    },
    utils::{Buffer, Logical, Physical, Rectangle, Scale, Size, Transform},
};

const VERT: &str = include_str!("effects_blur.vert");
const DOWN: &str = include_str!("effects_blur_down.frag");
const UP: &str = include_str!("effects_blur_up.frag");
static BLUR_FAILURE_LOGGED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
struct Program {
    id: ffi::types::GLuint,
    vert: ffi::types::GLint,
    tex: ffi::types::GLint,
    half_pixel: ffi::types::GLint,
    offset: ffi::types::GLint,
}

#[derive(Debug)]
pub(super) struct BlurPrograms {
    down: Program,
    up: Program,
    context: smithay::backend::renderer::ContextId<GlesTexture>,
}

impl BlurPrograms {
    pub(super) fn compile(renderer: &mut GlesRenderer) -> Result<Arc<Self>, GlesError> {
        // `link_program` logs full GLSL diagnostics at smithay ERROR level.
        let (down, up) = renderer.with_context(|gl| unsafe {
            let down = compile(gl, DOWN)?;
            let up = match compile(gl, UP) {
                Ok(program) => program,
                Err(error) => {
                    gl.DeleteProgram(down.id);
                    return Err(error);
                }
            };
            Ok::<_, GlesError>((down, up))
        })??;
        Ok(Arc::new(Self {
            down,
            up,
            context: renderer.context_id(),
        }))
    }
}

unsafe fn compile(gl: &ffi::Gles2, fragment: &str) -> Result<Program, GlesError> {
    let id = unsafe { link_program(gl, VERT, fragment)? };
    Ok(Program {
        id,
        vert: unsafe { gl.GetAttribLocation(id, c"vert".as_ptr()) },
        tex: unsafe { gl.GetUniformLocation(id, c"tex".as_ptr()) },
        half_pixel: unsafe { gl.GetUniformLocation(id, c"half_pixel".as_ptr()) },
        offset: unsafe { gl.GetUniformLocation(id, c"offset".as_ptr()) },
    })
}

/// One source at full resolution, followed by successively halved targets.
#[derive(Debug)]
pub(super) struct BlurTextures {
    size: Size<i32, Buffer>,
    passes: u8,
    pub(super) textures: Arc<Vec<GlesTexture>>,
}

impl BlurTextures {
    pub(super) fn prepare(
        renderer: &mut GlesRenderer,
        old: Option<Self>,
        size: Size<i32, Buffer>,
        passes: u8,
    ) -> Result<Self, GlesError> {
        if let Some(old) = old {
            if old.size == size && old.passes == passes {
                return Ok(old);
            }
        }
        let mut textures = Vec::with_capacity(usize::from(passes) + 1);
        let mut current = size;
        for _ in 0..=passes {
            let id = renderer.with_context(|gl| unsafe {
                let mut id = 0;
                gl.GenTextures(1, &mut id);
                gl.BindTexture(ffi::TEXTURE_2D, id);
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_WRAP_S,
                    ffi::CLAMP_TO_EDGE as i32,
                );
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_WRAP_T,
                    ffi::CLAMP_TO_EDGE as i32,
                );
                gl.TexImage2D(
                    ffi::TEXTURE_2D,
                    0,
                    ffi::RGBA as i32,
                    current.w,
                    current.h,
                    0,
                    ffi::RGBA,
                    ffi::UNSIGNED_BYTE,
                    std::ptr::null(),
                );
                gl.BindTexture(ffi::TEXTURE_2D, 0);
                id
            })?;
            if id == 0 {
                return Err(GlesError::FramebufferBindingError);
            }
            textures.push(unsafe {
                GlesTexture::from_raw(renderer, Some(ffi::RGBA), false, id, current)
            });
            current = Size::from((max(1, current.w / 2), max(1, current.h / 2)));
        }
        Ok(Self {
            size,
            passes,
            textures: Arc::new(textures),
        })
    }
}

#[derive(Debug, Clone)]
pub(super) struct BlurBackdrop {
    pub(super) id: Id,
    pub(super) commit: CommitCounter,
    pub(super) area: Rectangle<i32, Logical>,
    pub(super) output_size: Size<i32, Physical>,
    pub(super) passes: u8,
    pub(super) offset: f32,
    pub(super) program: Arc<BlurPrograms>,
    pub(super) textures: Arc<Vec<GlesTexture>>,
}

impl Element for BlurBackdrop {
    fn id(&self) -> &Id {
        &self.id
    }
    fn current_commit(&self) -> CommitCounter {
        self.commit
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.area.to_physical_precise_round(scale)
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(self.textures[0].size()).to_f64()
    }
    fn transform(&self) -> Transform {
        Transform::Normal
    }
    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        if commit == Some(self.commit) {
            DamageSet::default()
        } else {
            DamageSet::from_slice(&[Rectangle::from_size(self.geometry(scale).size)])
        }
    }
    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }
    fn alpha(&self) -> f32 {
        1.0
    }
    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<GlesRenderer> for BlurBackdrop {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        if frame.context_id() != self.program.context {
            return Ok(());
        }
        // Draw-time capture matters: `scene::elements` builds the list *before*
        // the background is composed. The output damage tracker draws back to
        // front; by the time we reach this element the backdrop is on the GPU.
        if let Err(error) = frame
            .with_context(|gl| unsafe { self.blur(gl) })
            .and_then(|result| result)
        {
            if !BLUR_FAILURE_LOGGED.swap(true, Ordering::Relaxed) {
                tracing::warn!(%error, "Kawase blur failed; drawing unblurred background instead");
            }
            return Ok(());
        }
        if let Err(error) = frame.render_texture_from_to(
            &self.textures[0],
            src,
            dst,
            damage,
            &[],
            Transform::Normal,
            1.0,
            None,
            &[],
        ) {
            if !BLUR_FAILURE_LOGGED.swap(true, Ordering::Relaxed) {
                tracing::warn!(%error, "Kawase backdrop drawing failed; skipping blur");
            }
        }
        Ok(())
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

impl BlurBackdrop {
    unsafe fn blur(&self, gl: &ffi::Gles2) -> Result<(), GlesError> {
        let mut old_fbo = 0;
        let mut old_program = 0;
        let mut old_active_tex = 0;
        let mut old_texture = 0;
        let mut old_buffer = 0;
        let mut old_viewport = [0; 4];
        let scissor = unsafe { gl.IsEnabled(ffi::SCISSOR_TEST) == ffi::TRUE };
        let blend = unsafe { gl.IsEnabled(ffi::BLEND) == ffi::TRUE };
        unsafe {
            gl.GetIntegerv(ffi::FRAMEBUFFER_BINDING, &mut old_fbo);
            gl.GetIntegerv(ffi::CURRENT_PROGRAM, &mut old_program);
            gl.GetIntegerv(ffi::ACTIVE_TEXTURE, &mut old_active_tex);
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.GetIntegerv(ffi::TEXTURE_BINDING_2D, &mut old_texture);
            gl.GetIntegerv(ffi::ARRAY_BUFFER_BINDING, &mut old_buffer);
            gl.GetIntegerv(ffi::VIEWPORT, old_viewport.as_mut_ptr());
            gl.Disable(ffi::SCISSOR_TEST);
            gl.Disable(ffi::BLEND);
        }
        let mut fbo = 0;
        let result = (|| unsafe {
            let size = self.area.size;
            gl.BindTexture(ffi::TEXTURE_2D, self.textures[0].tex_id());
            gl.CopyTexSubImage2D(
                ffi::TEXTURE_2D,
                0,
                0,
                0,
                self.area.loc.x,
                self.output_size.h - self.area.loc.y - size.h,
                size.w,
                size.h,
            );
            gl.GenFramebuffers(1, &mut fbo);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
            gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
            let verts: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];
            for step in 1..=usize::from(self.passes) {
                draw_pass(
                    gl,
                    &self.program.down,
                    &self.textures[step - 1],
                    &self.textures[step],
                    self.offset,
                    &verts,
                )?;
            }
            for step in (1..=usize::from(self.passes)).rev() {
                draw_pass(
                    gl,
                    &self.program.up,
                    &self.textures[step],
                    &self.textures[step - 1],
                    self.offset,
                    &verts,
                )?;
            }
            Ok(())
        })();
        unsafe {
            gl.DeleteFramebuffers(1, &fbo);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, old_fbo as u32);
            gl.Viewport(
                old_viewport[0],
                old_viewport[1],
                old_viewport[2],
                old_viewport[3],
            );
            gl.UseProgram(old_program as u32);
            gl.BindBuffer(ffi::ARRAY_BUFFER, old_buffer as u32);
            gl.BindTexture(ffi::TEXTURE_2D, old_texture as u32);
            gl.ActiveTexture(old_active_tex as u32);
            if scissor {
                gl.Enable(ffi::SCISSOR_TEST);
            }
            if blend {
                gl.Enable(ffi::BLEND);
            }
        }
        result
    }
}

unsafe fn draw_pass(
    gl: &ffi::Gles2,
    program: &Program,
    src: &GlesTexture,
    dst: &GlesTexture,
    offset: f32,
    verts: &[f32; 8],
) -> Result<(), GlesError> {
    let size = dst.size();
    unsafe {
        gl.FramebufferTexture2D(
            ffi::FRAMEBUFFER,
            ffi::COLOR_ATTACHMENT0,
            ffi::TEXTURE_2D,
            dst.tex_id(),
            0,
        );
        if gl.CheckFramebufferStatus(ffi::FRAMEBUFFER) != ffi::FRAMEBUFFER_COMPLETE {
            return Err(GlesError::FramebufferBindingError);
        }
        gl.Viewport(0, 0, size.w, size.h);
        gl.UseProgram(program.id);
        gl.Uniform1i(program.tex, 0);
        gl.Uniform2f(program.half_pixel, 0.5 / size.w as f32, 0.5 / size.h as f32);
        gl.Uniform1f(program.offset, offset);
        gl.BindTexture(ffi::TEXTURE_2D, src.tex_id());
        gl.EnableVertexAttribArray(program.vert as u32);
        gl.VertexAttribPointer(
            program.vert as u32,
            2,
            ffi::FLOAT,
            ffi::FALSE,
            0,
            verts.as_ptr().cast(),
        );
        gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        gl.DisableVertexAttribArray(program.vert as u32);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blur_shaders_use_es_precision_and_kawase_samples() {
        for program in [VERT, DOWN, UP] {
            assert!(program.starts_with("#version 100\n"));
        }
        for fragment in [DOWN, UP] {
            assert!(fragment.contains("precision highp float;"));
            assert!(fragment.contains("half_pixel"));
            assert!(fragment.contains("offset"));
            assert!(fragment.contains("texture2D("));
        }
    }
}
