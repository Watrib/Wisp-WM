//! Affine rendering of a spring-resized window. Logical Wayland placement and
//! hit-testing remain at the target; only the final GLES destination moves.

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
        gles::{GlesError, GlesFrame, GlesRenderer},
        utils::{CommitCounter, DamageSet, OpaqueRegions},
    },
    utils::{Buffer, Physical, Rectangle, Scale, Transform},
};

/// A scene element whose destination is mapped between a window's content
/// rect and the spring's current visual rectangle.
pub(super) struct SpringElement<E> {
    inner: E,
    content: Rectangle<i32, Physical>,
    visual: Rectangle<i32, Physical>,
}

impl<E> SpringElement<E> {
    pub(super) fn new(
        inner: E,
        content: Rectangle<i32, Physical>,
        visual: Rectangle<i32, Physical>,
    ) -> Self {
        Self {
            inner,
            content,
            visual,
        }
    }
}

impl<E: Element> Element for SpringElement<E> {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        let geo = self.inner.geometry(scale);
        let sx = f64::from(self.visual.size.w) / f64::from(self.content.size.w.max(1));
        let sy = f64::from(self.visual.size.h) / f64::from(self.content.size.h.max(1));
        let map_x = |x: i32| f64::from(self.visual.loc.x) + f64::from(x - self.content.loc.x) * sx;
        let map_y = |y: i32| f64::from(self.visual.loc.y) + f64::from(y - self.content.loc.y) * sy;
        let x1 = map_x(geo.loc.x).round() as i32;
        let y1 = map_y(geo.loc.y).round() as i32;
        let x2 = map_x(geo.loc.x + geo.size.w).round() as i32;
        let y2 = map_y(geo.loc.y + geo.size.h).round() as i32;
        // Round edges rather than lengths: shared surface edges stay aligned.
        Rectangle::new((x1, y1).into(), ((x2 - x1).max(1), (y2 - y1).max(1)).into())
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
        _commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        // Smithay tracks the old geometry separately. Full current damage is
        // conservative while a spring moves, including sub-surfaces.
        DamageSet::from_slice(&[Rectangle::from_size(self.geometry(scale).size)])
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        // A transformed rounded corner must not hide what's behind it.
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl<E: RenderElement<GlesRenderer> + Element> RenderElement<GlesRenderer> for SpringElement<E> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        self.inner.draw(frame, src, dst, damage, &[])
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
