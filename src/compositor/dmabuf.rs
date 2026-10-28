//! Direct-only v4 linux-dmabuf validation. The renderer used here is the same
//! GLES instance used for composition; do not accept a buffer based on metadata
//! alone, since EGL/driver imports can still fail for advertised formats.

use smithay::{
    backend::{allocator::dmabuf::Dmabuf, renderer::ImportDma},
    delegate_dmabuf,
    wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
};
use tracing::debug;

use super::Wisp;

impl DmabufHandler for Wisp {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(&mut self, _: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        let imported = self.dmabuf_session_active
            && self
                .dmabuf_renderer
                .as_ref()
                .and_then(|weak| weak.upgrade())
                .and_then(|renderer| {
                    renderer
                        .try_borrow_mut()
                        .ok()
                        .map(|mut renderer| renderer.import_dmabuf(&dmabuf, None))
                })
                .is_some_and(|result| {
                    if let Err(error) = &result {
                        debug!(%error, "rejecting dma-buf that GLES cannot import");
                    }
                    result.is_ok()
                });
        if imported {
            if notifier.successful::<Self>().is_err() {
                debug!("dma-buf import completed after client disconnected");
            }
        } else {
            notifier.failed();
        }
    }
}

delegate_dmabuf!(Wisp);
