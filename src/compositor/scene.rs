//! Render elements shared by the nested and direct backends.

use std::time::{Duration, Instant};

use smithay::{
    backend::renderer::{
        Color32F,
        element::{
            AsRenderElements, Kind,
            memory::MemoryRenderBufferRenderElement,
            render_elements,
            solid::SolidColorRenderElement,
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
        },
        gles::{GlesRenderer, Uniform, UniformValue, element::PixelShaderElement},
    },
    desktop::layer_map_for_output,
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    output::Output,
    utils::{Point, Rectangle},
    wayland::compositor::with_states,
    wayland::shell::wlr_layer::Layer,
};

use super::Wisp;
use super::blur::{BlurBackdrop, BlurTextures};
use super::effects::{ClippedWindowSurface, EffectsShaders};
use super::visual::SpringElement;

render_elements! {
    pub(super) WindowElement<=GlesRenderer>;
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    Solid=SolidColorRenderElement,
    Pixel=PixelShaderElement,
    Clipped=ClippedWindowSurface,
}

render_elements! {
    pub(super) Element<=GlesRenderer>;
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    Solid=SolidColorRenderElement,
    Pixel=PixelShaderElement,
    Clipped=ClippedWindowSurface,
    Blur=BlurBackdrop,
    Visual=SpringElement<WindowElement>,
    Memory=MemoryRenderBufferRenderElement<GlesRenderer>,
}

/// Replace only the current window's children. An unanimated element keeps
/// its Smithay ID, so the damage tracker compares its old/new geometry.
fn apply_visual(
    elements: &mut Vec<Element>,
    start: usize,
    content: Rectangle<i32, smithay::utils::Physical>,
    visual: Option<Rectangle<i32, smithay::utils::Physical>>,
) {
    let Some(visual) = visual else { return };
    let children: Vec<_> = elements.drain(start..).collect();
    for element in children {
        let inner = match element {
            Element::Surface(inner) => WindowElement::Surface(inner),
            Element::Solid(inner) => WindowElement::Solid(inner),
            Element::Pixel(inner) => WindowElement::Pixel(inner),
            Element::Clipped(inner) => WindowElement::Clipped(inner),
            Element::Blur(_) => continue, // capture uses untransformed GL coordinates
            Element::Memory(_) | Element::Visual(_) | Element::_GenericCatcher(_) => {
                unreachable!("only window children are transformed")
            }
        };
        elements.push(Element::Visual(SpringElement::new(inner, content, visual)));
    }
}

/// Build the scene for one output. Elements are front-to-back: the cursor and
/// client cursor surfaces come first, followed by windows (topmost first).
/// Border buffers are persistent so their damage tracking survives frames.
/// With shaders compiled, each window additionally gets GPU decorations:
/// shadow first (most back), border ring, then the corner-clipped surfaces.
#[allow(clippy::too_many_lines)]
pub(super) fn elements(state: &mut Wisp, renderer: &mut GlesRenderer, elements: &mut Vec<Element>) {
    // Each backend keeps this vector between frames. Clearing drops stale
    // buffer/texture references, while retaining capacity avoids a fresh
    // allocation for every animation or client commit.
    elements.clear();
    elements.reserve(state.windows.len() * 7 + state.mapped_layers() * 3 + 1);

    // A pending or confirmed lock must never sample the normal desktop scene.
    // The output clear color is black while locked, including before the lock
    // client has committed its first buffer or after the client disappears.
    if state.session_lock.is_some() {
        if let Some(surface) = state.lock_surface() {
            elements.extend(render_elements_from_surface_tree(
                renderer,
                surface.wl_surface(),
                (0, 0),
                1.0,
                1.0,
                Kind::Unspecified,
            ));
        }
        return;
    }

    if let CursorImageStatus::Surface(surface) = &state.cursor_status {
        let hotspot = with_states(surface, |states| {
            states
                .data_map
                .get::<CursorImageSurfaceData>()
                .map(|data| data.lock().unwrap().hotspot)
                .unwrap_or_default()
        });
        let position = state
            .seat
            .get_pointer()
            .unwrap()
            .current_location()
            .to_i32_round()
            - hotspot;
        elements.extend(render_elements_from_surface_tree(
            renderer,
            surface,
            position.to_physical(1),
            1.0,
            1.0,
            Kind::Cursor,
        ));
    }

    // In normal mode panels and overlays own their regions above windows.
    // Overview instead draws thumbnails over a dimmed shell, while the cursor
    // remains frontmost. `render_output` consumes this list front-to-back.
    let overview_scene = state.overview || state.overview_exiting;
    if !overview_scene {
        layer_elements(state, renderer, &[Layer::Overlay, Layer::Top], elements);
    }

    // Effect programs are bound to the renderer's context; compile them once
    // on the first render pass. Failures disable individual effects.
    let shaders = state
        .effects
        .get_or_insert_with(|| EffectsShaders::compile(renderer));
    if state.config.decoration.blur.enabled {
        shaders.ensure_blur(renderer);
    }
    let rounded = shaders.rounded.clone();
    let border_program = shaders.border.clone();
    let shadow_program = shaders.shadow.clone();
    let blur_program = shaders.blur.clone();
    let radius = f32::from(state.config.decoration.corner_radius);
    let overview_targets = &state.overview_rects;
    // Outputs are scale 1 today; the cursor and window code paths assume it.
    let scale: f32 = 1.0;
    let slide_width = state.viewport().width;

    for window in state.space.elements().rev() {
        let location = state.space.element_location(window).unwrap();
        let origin = location - window.geometry().loc;
        let geometry = window.geometry();

        let entry = state
            .windows
            .iter_mut()
            .find(|existing| &existing.window == window);
        let Some(entry) = entry else {
            elements.extend(window.render_elements(
                renderer,
                origin.to_physical(1),
                1.0.into(),
                1.0,
            ));
            continue;
        };
        let element_start = elements.len();
        let now = Instant::now();
        let fade_opacity = if entry.fade.as_ref().is_some_and(|fade| fade.done(now)) {
            entry.fade = None;
            1.0
        } else {
            entry.fade.as_ref().map_or(1.0, |fade| fade.opacity(now))
        };
        let opacity = fade_opacity
            * state
                .workspace_slide
                .as_ref()
                .and_then(|slide| slide.opacity(entry.workspace, now))
                .unwrap_or(1.0);
        if let Some(spring) = &mut entry.spring {
            spring.advance(now, state.config.layout.spring_frequency_hz);
            if spring.settled() {
                entry.spring = None;
            }
        }
        // A newly mapped window eases from a slightly inset rect while it
        // fades. Tiling and hit-testing stay at the final logical position.
        let opening = entry.fade.as_ref().map(|_| {
            let inset_x = (geometry.size.w as f32 * (1.0 - opacity) * 0.04).round() as i32;
            let inset_y = (geometry.size.h as f32 * (1.0 - opacity) * 0.04).round() as i32;
            super::Rect::new(
                (location.x + geometry.loc.x).saturating_add(inset_x),
                (location.y + geometry.loc.y).saturating_add(inset_y),
                (geometry.size.w - 2 * inset_x).max(1),
                (geometry.size.h - 2 * inset_y).max(1),
            )
        });
        let visual = if let Some(slide) = &state.workspace_slide {
            let rect = super::Rect::new(
                location.x + geometry.loc.x,
                location.y + geometry.loc.y,
                geometry.size.w,
                geometry.size.h,
            );
            slide.slide_rect(entry.workspace, rect, slide_width, now)
        } else if state.overview {
            entry
                .spring
                .as_ref()
                .map(|spring| spring.rect())
                .or_else(|| {
                    overview_targets
                        .iter()
                        .find(|(id, _)| *id == entry.id)
                        .map(|(_, rect)| *rect)
                })
        } else {
            entry
                .spring
                .as_ref()
                .map(|spring| spring.rect())
                .or(opening)
        };
        let focused = Some(entry.id) == state.focused || entry.urgent;
        // Window content rect in logical space, anchor-aware. The legacy
        // solid-buffer border path below keeps its pre-effects anchoring.
        let content = Rectangle::new(location + geometry.loc, geometry.size);

        // Inactive-window dimming (phase 2a): a zero-allocation solid overlay
        // pushed ahead of the window's own elements so it darkens exactly this
        // window's content area while every window above stays unaffected.
        // Fullscreen windows skip it like the other decorations.
        let dim_strength = if entry.fullscreen || !entry.mapped || focused {
            0.0
        } else {
            state.config.decoration.dim_strength as f32 * opacity
        };
        if dim_strength > 0.0 {
            if entry.dim_strength != dim_strength {
                entry.dim_strength = dim_strength;
                entry.dim_commit.increment();
            }
            elements.push(
                SolidColorRenderElement::new(
                    entry.dim_id.clone(),
                    content.to_physical_precise_round::<f64, i32>(1.0),
                    entry.dim_commit,
                    Color32F::new(0.0, 0.0, 0.0, dim_strength),
                    Kind::Unspecified,
                )
                .into(),
            );
        }

        // Window surfaces. With rounding active every surface is wrapped by
        // the clip shader; metadata delegates to the inner element.
        let surfaces = window.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
            renderer,
            origin.to_physical(1),
            1.0.into(),
            opacity,
        );
        let clip = !entry.fullscreen && entry.mapped && radius > 0.0;
        let rounded_program = if clip { rounded.clone() } else { None };
        for surface in surfaces {
            let element = match &rounded_program {
                Some(program) => Element::Clipped(ClippedWindowSurface::new(
                    surface,
                    program.clone(),
                    Rectangle::new(
                        Point::from((content.loc.x as f64, content.loc.y as f64)),
                        (f64::from(content.size.w), f64::from(content.size.h)).into(),
                    ),
                    [radius; 4],
                    scale,
                )),
                None => Element::Surface(surface),
            };
            elements.push(element);
        }

        // This element is behind window surfaces but ahead of all windows and
        // layers below. At draw time their composited pixels are copied from
        // the active framebuffer; no readback or separate scene traversal.
        if entry.mapped
            && !entry.fullscreen
            && !overview_scene
            && visual.is_none()
            && state.workspace_slide.is_none()
            && entry.fade.is_none()
            && state.config.decoration.blur.enabled
        {
            if let (Some(program), Some(output)) = (blur_program.as_ref(), state.output.as_ref()) {
                if let Some(mode) = output.current_mode() {
                    let output_size = mode.size;
                    let bounds = Rectangle::from_size(output_size.to_logical(1));
                    if let Some(area) = content.intersection(bounds) {
                        if area.size.w > 0 && area.size.h > 0 {
                            let options = &state.config.decoration.blur;
                            let old = entry.decor.blur_textures.take();
                            match BlurTextures::prepare(
                                renderer,
                                old,
                                area.size.to_buffer(1, smithay::utils::Transform::Normal),
                                options.passes,
                            ) {
                                Ok(textures) => {
                                    let id = entry
                                        .decor
                                        .blur_id
                                        .get_or_insert_with(
                                            smithay::backend::renderer::element::Id::new,
                                        )
                                        .clone();
                                    let options_key = (options.passes, options.offset as f32);
                                    if entry.decor.blur_options != Some(options_key) {
                                        entry.decor.blur_options = Some(options_key);
                                        entry.decor.blur_commit.increment();
                                    }
                                    elements.push(Element::Blur(BlurBackdrop {
                                        id,
                                        commit: entry.decor.blur_commit,
                                        area,
                                        output_size,
                                        passes: options.passes,
                                        offset: options.offset as f32,
                                        program: program.clone(),
                                        textures: textures.textures.clone(),
                                    }));
                                    entry.decor.blur_textures = Some(textures);
                                }
                                Err(error) => {
                                    tracing::warn!(%error, "allocating Kawase blur textures; skipping blur")
                                }
                            }
                        }
                    }
                }
            }
        }

        // GPU shadow: behind everything for this window, from the config.
        let shadow = &state.config.decoration.shadow;
        let use_shadow = !entry.fullscreen && entry.mapped && shadow.enabled;
        if let (true, Some(program)) = (use_shadow, shadow_program.as_ref()) {
            let sigma = shadow.sigma;
            let spread = (sigma * 2.5).ceil() as i32;
            let area = Rectangle::new(
                Point::from((content.loc.x - spread, content.loc.y - spread)),
                (content.size.w + spread * 2, content.size.h + spread * 2).into(),
            );
            // Premultiplied semantics: scale every channel by the strength.
            let strength = (if focused {
                shadow.strength_active
            } else {
                shadow.strength_inactive
            }) as f32;
            let color = shadow.color.0.map(|channel| channel * strength * opacity);
            let uniforms = [
                Uniform::new(
                    "shadow_color",
                    UniformValue::_4f(color[0], color[1], color[2], color[3]),
                ),
                Uniform::new("sigma", UniformValue::_1f(sigma as f32)),
                Uniform::new(
                    "window_offset",
                    UniformValue::_2f(spread as f32, spread as f32),
                ),
                Uniform::new(
                    "window_size",
                    UniformValue::_2f(content.size.w as f32, content.size.h as f32),
                ),
                Uniform::new("radius", UniformValue::_4f(radius, radius, radius, radius)),
            ];
            if let Some(element) = entry.decor.sync_shadow(program, area, &uniforms) {
                elements.push(Element::Pixel(element.clone()));
            }
        }

        // GPU gradient border replaces the solid buffers when shaders exist;
        // the legacy path stays available for radius 0 without a gradient.
        let decoration = &state.config.decoration;
        let use_pixel_border =
            !entry.fullscreen && entry.mapped && (radius > 0.0 || decoration.gradient.enabled);
        if let (true, Some(program)) = (use_pixel_border, border_program.as_ref()) {
            let border_width = decoration.border_width;
            let (from, to) = if decoration.gradient.enabled {
                (decoration.gradient.from, decoration.gradient.to)
            } else if focused {
                (decoration.active_border, decoration.active_border)
            } else {
                (decoration.inactive_border, decoration.inactive_border)
            };
            if border_width > 0 {
                let from = from.0.map(|channel| channel * opacity);
                let to = to.0.map(|channel| channel * opacity);
                let area = Rectangle::new(
                    Point::from((content.loc.x - border_width, content.loc.y - border_width)),
                    (
                        content.size.w + border_width * 2,
                        content.size.h + border_width * 2,
                    )
                        .into(),
                );
                let angle = f64::from(decoration.gradient.angle).to_radians();
                let (sin, cos) = angle.sin_cos();
                let uniforms = [
                    Uniform::new(
                        "color_from",
                        UniformValue::_4f(from[0], from[1], from[2], from[3]),
                    ),
                    Uniform::new("color_to", UniformValue::_4f(to[0], to[1], to[2], to[3])),
                    Uniform::new("grad_vec", UniformValue::_2f(sin as f32, cos as f32)),
                    Uniform::new(
                        "geo_size",
                        UniformValue::_2f(
                            (content.size.w + border_width * 2) as f32,
                            (content.size.h + border_width * 2) as f32,
                        ),
                    ),
                    Uniform::new(
                        "outer_radius",
                        UniformValue::_4f(
                            radius + border_width as f32,
                            radius + border_width as f32,
                            radius + border_width as f32,
                            radius + border_width as f32,
                        ),
                    ),
                    Uniform::new("border_width", UniformValue::_1f(border_width as f32)),
                ];
                if let Some(element) = entry.decor.sync_border(program, area, &uniforms) {
                    elements.push(Element::Pixel(element.clone()));
                }
            }
        }

        let border = if entry.fullscreen {
            0
        } else {
            state.config.decoration.border_width
        };
        // Legacy solid border ring: kept for radius 0 without gradient so the
        // previous rendering path stays byte-identical.
        let pixel_border = use_pixel_border && border_program.is_some();
        if border == 0 || !entry.mapped || pixel_border {
            apply_visual(
                elements,
                element_start,
                content.to_physical_precise_round::<f64, i32>(1.0),
                visual.map(|rect| {
                    Rectangle::new((rect.x, rect.y).into(), (rect.width, rect.height).into())
                }),
            );
            continue;
        }
        let size = window.geometry().size;
        let color = if focused {
            state.config.decoration.active_border
        } else {
            state.config.decoration.inactive_border
        };
        let rectangles = [
            (
                location.x - border,
                location.y - border,
                size.w + 2 * border,
                border,
            ),
            (
                location.x - border,
                location.y + size.h,
                size.w + 2 * border,
                border,
            ),
            (location.x - border, location.y, border, size.h),
            (location.x + size.w, location.y, border, size.h),
        ];
        for (buffer, (x, y, width, height)) in entry.borders.iter_mut().zip(rectangles) {
            buffer.update((width, height), color.0.map(|channel| channel * opacity));
            elements.push(
                SolidColorRenderElement::from_buffer(
                    buffer,
                    Point::from((x, y)),
                    1.0,
                    1.0,
                    Kind::Unspecified,
                )
                .into(),
            );
        }
        apply_visual(
            elements,
            element_start,
            content.to_physical_precise_round::<f64, i32>(1.0),
            visual.map(|rect| {
                Rectangle::new((rect.x, rect.y).into(), (rect.width, rect.height).into())
            }),
        );
    }

    if overview_scene {
        if let Some(size) = state
            .output
            .as_ref()
            .and_then(Output::current_mode)
            .map(|m| m.size)
        {
            elements.push(Element::Solid(SolidColorRenderElement::new(
                state.overview_dim_id.clone(),
                Rectangle::from_size(size),
                state.overview_dim_commit,
                Color32F::new(0.0, 0.0, 0.0, 0.55),
                Kind::Unspecified,
            )));
        }
        // Behind the dim overlay and all thumbnail windows, but still above
        // background layers. This includes any desktop-shell top/overlay
        // surfaces, not just Noctalia.
        layer_elements(state, renderer, &[Layer::Overlay, Layer::Top], elements);
    }

    // Layer elements are front-to-back, and the list continues front-to-back:
    // bottom layer first, wallpaper (background) last.
    layer_elements(
        state,
        renderer,
        &[Layer::Bottom, Layer::Background],
        elements,
    );
}

/// Render elements for the mapped surfaces of the given wlr layers, drawn in
/// the layer map's stacking (last mapped on top). Gets its geometry from the
/// map so anchors, margins and exclusive zones decide placement.
fn layer_elements(
    state: &Wisp,
    renderer: &mut GlesRenderer,
    kinds: &[Layer],
    elements: &mut Vec<Element>,
) {
    let Some(output) = state.output.as_ref() else {
        return;
    };
    let map = layer_map_for_output(output);
    for kind in kinds {
        for layer in map.layers_on(*kind).rev() {
            let Some(geometry) = map.layer_geometry(layer) else {
                continue;
            };
            // Smithay includes layer-attached xdg popups ahead of the parent;
            // rendering only the wl_surface tree would omit them.
            elements.extend(layer.render_elements(
                renderer,
                geometry.loc.to_physical(1),
                1.0.into(),
                1.0,
            ));
        }
    }
}

/// Send frame callbacks at the compositor's applied refresh cadence. This is
/// shared so both backends throttle callbacks identically, including commits
/// that produce no damage. Hidden-workspace windows are not included.
pub(super) fn send_frame_callbacks(state: &Wisp, output: &Output) {
    // A single timestamp for the entire presented frame: fewer clock reads
    // and all clients observe the same frame time.
    let timestamp = state.start_time.elapsed();
    if state.session_lock.is_some() {
        if let Some(surface) = state.lock_surface() {
            smithay::desktop::utils::send_frames_surface_tree(
                surface.wl_surface(),
                output,
                timestamp,
                Some(Duration::ZERO),
                |_, _| Some(output.clone()),
            );
        }
        return;
    }
    for window in state.space.elements() {
        window.send_frame(output, timestamp, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
    }
    // Layer surfaces never appear in the space; schedule their frame
    // callbacks directly from the map so panels and wallpaper redraw.
    for layer in layer_map_for_output(output).layers() {
        layer.send_frame(output, timestamp, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
    }
    if let CursorImageStatus::Surface(surface) = &state.cursor_status {
        smithay::desktop::utils::send_frames_surface_tree(
            surface,
            output,
            timestamp,
            Some(Duration::ZERO),
            |_, _| Some(output.clone()),
        );
    }
}
