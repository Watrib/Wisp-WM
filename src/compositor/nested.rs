use std::time::Instant;

use anyhow::Result;
use smithay::{
    backend::{
        egl::context::GlAttributes,
        renderer::{damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent, WinitEventLoop, WinitGraphicsBackend},
    },
    input::pointer::CursorImageStatus,
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::winit::{dpi::LogicalSize, window::Window},
    utils::Transform,
};

use super::{Wisp, scene};

pub(super) struct Nested {
    // Drop cached scene elements before the GL backend and renderer.
    elements: Vec<scene::Element>,
    backend: WinitGraphicsBackend<GlesRenderer>,
    output: Output,
    damage: OutputDamageTracker,
    presented: bool,
}

impl Nested {
    pub fn new(state: &mut Wisp) -> Result<(Self, WinitEventLoop)> {
        let attributes = Window::default_attributes()
            .with_title("Wisp • nested Wayland")
            .with_inner_size(LogicalSize::new(
                state.config.render.width,
                state.config.render.height,
            ));
        let (backend, events) = winit::init_from_attributes_with_gl_attr(
            attributes,
            GlAttributes {
                version: (3, 0),
                profile: None,
                debug: cfg!(debug_assertions),
                vsync: true,
            },
        )
        .map_err(|error| anyhow::anyhow!("initializing nested backend: {error}"))?;
        let mode = Mode {
            size: backend.window_size(),
            refresh: state.config.render.max_fps as i32 * 1000,
        };
        let output = Output::new(
            "WISP-1".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Wisp".into(),
                model: "Nested".into(),
            },
        );
        output.create_global::<Wisp>(&state.display_handle);
        output.change_current_state(
            Some(mode),
            Some(Transform::Flipped180),
            None,
            Some((0, 0).into()),
        );
        output.set_preferred(mode);
        state.space.map_output(&output, (0, 0));
        state.output = Some(output.clone());
        state.backend_name = "nested-winit-gles";
        let damage = OutputDamageTracker::from_output(&output);
        Ok((
            Self {
                elements: Vec::new(),
                backend,
                output,
                damage,
                presented: false,
            },
            events,
        ))
    }

    pub fn event(&mut self, state: &mut Wisp, event: WinitEvent) {
        if state.shutdown.is_some() {
            return;
        }
        match event {
            WinitEvent::Resized { size, .. } => {
                if size.w > 0 && size.h > 0 {
                    state.cancel_workspace_slide();
                    self.output.change_current_state(
                        Some(Mode {
                            size,
                            refresh: state.config.render.max_fps as i32 * 1000,
                        }),
                        None,
                        None,
                        None,
                    );
                    state.reconfigure_lock_surface();
                    state.arrange();
                }
            }
            WinitEvent::Input(event) => state.process_input(event),
            WinitEvent::Focus(focused) => state.host_focus(focused),
            WinitEvent::Redraw => {
                state.stats.host_redraws += 1;
                state.dirty = true;
            }
            WinitEvent::CloseRequested => state.begin_shutdown("host window closed"),
        }
    }

    pub fn render(&mut self, state: &mut Wisp) -> Result<()> {
        let size = self.backend.window_size();
        if size.w <= 0 || size.h <= 0 {
            return Ok(());
        }
        let start = Instant::now();
        match &state.cursor_status {
            _ if state.session_lock.is_some() => self.backend.window().set_cursor_visible(false),
            CursorImageStatus::Named(icon) => {
                self.backend.window().set_cursor_visible(true);
                self.backend.window().set_cursor(*icon);
            }
            _ => self.backend.window().set_cursor_visible(false),
        }
        // EGL may not have a native surface until the first swap. Do not query
        // its age before that; some drivers correctly report EGL_BAD_SURFACE.
        let bind_start = Instant::now();
        let age = if self.presented {
            {
                let _ = self.backend.bind()?;
            }
            self.backend.buffer_age().unwrap_or(0)
        } else {
            0
        };
        let damage = {
            let (renderer, mut framebuffer) = self.backend.bind()?;
            state.stats.bind_wall_us += bind_start.elapsed().as_micros() as u64;
            let scene_start = Instant::now();
            scene::elements(state, renderer, &mut self.elements);
            state.stats.scene_wall_us += scene_start.elapsed().as_micros() as u64;
            let compose_start = Instant::now();
            let damage = self
                .damage
                .render_output(
                    renderer,
                    &mut framebuffer,
                    age,
                    &self.elements,
                    if state.session_lock.is_some() {
                        [0.0, 0.0, 0.0, 1.0]
                    } else {
                        state.config.decoration.background.0
                    },
                )?
                .damage
                .cloned();
            state.stats.compose_wall_us += compose_start.elapsed().as_micros() as u64;
            // CPU-only cadence check: excludes the nested present parking
            // measured above, so throttling is not confused with slow work.
            let cpu_us = bind_start.elapsed().as_micros() as u64;
            state.stats.render_cpu_us += cpu_us;
            state.stats.cpu_over_8ms += u64::from(cpu_us > 8_000);
            state.stats.cpu_over_16ms += u64::from(cpu_us > 16_000);
            damage
        };
        let present_start = Instant::now();
        if let Some(damage) = damage {
            self.backend.submit(Some(&damage))?;
            self.presented = true;
            state.stats.rendered += 1;
            state.confirm_session_lock();
        } else {
            state.stats.no_damage += 1;
            state.confirm_session_lock();
        }
        state.stats.present_wall_us += present_start.elapsed().as_micros() as u64;
        // Throttle callbacks via the on-demand frame scheduler, including damage-free
        // commits. Hidden-workspace windows do not receive frame callbacks.
        scene::send_frame_callbacks(state, &self.output);
        state.stats.last_cpu_render_us = start.elapsed().as_micros() as u64;
        state.stats.render_over_8ms += u64::from(state.stats.last_cpu_render_us > 8_000);
        state.stats.render_over_16ms += u64::from(state.stats.last_cpu_render_us > 16_000);
        state.display_handle.flush_clients()?;
        Ok(())
    }
}
