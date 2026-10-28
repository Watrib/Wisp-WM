//! Metadata-only toplevel icons, adapted from Smithay 0.7 (MIT).
//!
//! Its SHM hook removal compares hook IDs with != rather than ==, leaving the
//! icon lifetime check installed after destruction (and possibly removing a
//! different icon's check). Keep lifetime checks here via BufferHandler instead
//! of relying on private, broken hooks or modifying Cargo's registry sources.
use std::sync::Mutex;

use smithay::{
    reexports::{
        wayland_protocols::xdg::toplevel_icon::v1::server::{
            xdg_toplevel_icon_manager_v1::{self, XdgToplevelIconManagerV1},
            xdg_toplevel_icon_v1::{self, XdgToplevelIconV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
            protocol::wl_buffer::WlBuffer,
        },
    },
    wayland::{
        compositor::{Cacheable, with_states},
        shm::with_buffer_contents,
    },
};

use super::super::Wisp;

pub(crate) struct IconManager {
    live: Vec<Weak<XdgToplevelIconV1>>,
}

impl IconManager {
    pub(in crate::compositor) fn new(dh: &DisplayHandle) -> Self {
        dh.create_global::<Wisp, XdgToplevelIconManagerV1, _>(1, ());
        Self { live: Vec::new() }
    }

    pub(in crate::compositor) fn buffer_destroyed(&mut self, buffer: &WlBuffer) {
        self.live.retain(|weak| {
            let Ok(icon) = weak.upgrade() else {
                return false;
            };
            if icon
                .data::<IconData>()
                .unwrap()
                .inner
                .lock()
                .unwrap()
                .metadata
                .buffers
                .iter()
                .any(|entry| &entry.buffer == buffer)
            {
                icon.post_error(
                    xdg_toplevel_icon_v1::Error::NoBuffer,
                    "The provided buffer has been destroyed before the toplevel icon",
                );
            }
            true
        });
    }
}

#[derive(Clone, Debug)]
struct IconBuffer {
    buffer: WlBuffer,
    size: i32,
    scale: i32,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ToplevelIconCachedState {
    name: Option<String>,
    buffers: Vec<IconBuffer>,
}

impl ToplevelIconCachedState {
    pub(super) fn icon_name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    pub(super) fn buffer_count(&self) -> usize {
        self.buffers.len()
    }
}

impl Cacheable for ToplevelIconCachedState {
    fn commit(&mut self, _: &DisplayHandle) -> Self {
        self.clone()
    }
    fn merge_into(self, into: &mut Self, _: &DisplayHandle) {
        *into = self;
    }
}

#[derive(Default)]
struct IconInner {
    immutable: bool,
    metadata: ToplevelIconCachedState,
}

#[derive(Default)]
pub(crate) struct IconData {
    inner: Mutex<IconInner>,
}

impl GlobalDispatch<XdgToplevelIconManagerV1, ()> for Wisp {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<XdgToplevelIconManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        // No preferred pixel sizes: no icon-bearing taskbar/titlebar yet.
        data_init.init(resource, ()).done();
    }
}

impl Dispatch<XdgToplevelIconManagerV1, ()> for Wisp {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &XdgToplevelIconManagerV1,
        request: xdg_toplevel_icon_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_toplevel_icon_manager_v1::Request::CreateIcon { id } => {
                let icon = data_init.init(id, IconData::default());
                state
                    .toplevel_icon_state
                    .live
                    .retain(|weak| weak.upgrade().is_ok());
                state.toplevel_icon_state.live.push(icon.downgrade());
            }
            xdg_toplevel_icon_manager_v1::Request::SetIcon { toplevel, icon } => {
                let metadata = icon
                    .map(|icon| {
                        let mut data = icon.data::<IconData>().unwrap().inner.lock().unwrap();
                        data.immutable = true;
                        data.metadata.clone()
                    })
                    .unwrap_or_default();
                let Some(toplevel) = state
                    .xdg_shell_state
                    .toplevel_surfaces()
                    .iter()
                    .find(|surface| surface.xdg_toplevel() == &toplevel)
                else {
                    return;
                };
                let surface = toplevel.wl_surface();
                with_states(surface, |states| {
                    *states
                        .cached_state
                        .get::<ToplevelIconCachedState>()
                        .pending() = metadata;
                });
            }
            xdg_toplevel_icon_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl Dispatch<XdgToplevelIconV1, IconData> for Wisp {
    fn request(
        state: &mut Self,
        _: &Client,
        icon: &XdgToplevelIconV1,
        request: xdg_toplevel_icon_v1::Request,
        data: &IconData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use xdg_toplevel_icon_v1::{Error, Request};
        let mut data = data.inner.lock().unwrap();
        if matches!(request, Request::Destroy) {
            // Check before removing the live entry, even if buffer destruction
            // callbacks are deferred. Cached metadata is independent of this object.
            if data
                .metadata
                .buffers
                .iter()
                .any(|entry| !entry.buffer.is_alive())
            {
                icon.post_error(Error::NoBuffer, "Icon buffer destroyed before its icon");
            }
            state
                .toplevel_icon_state
                .live
                .retain(|weak| weak.id() != icon.id());
            return;
        }
        if data.immutable {
            icon.post_error(
                Error::Immutable,
                "Icon has already been assigned to a toplevel",
            );
            return;
        }
        match request {
            Request::SetName { icon_name } => data.metadata.name = Some(icon_name),
            Request::AddBuffer { buffer, scale } => {
                let Ok((width, height)) =
                    with_buffer_contents(&buffer, |_, _, meta| (meta.width, meta.height))
                else {
                    icon.post_error(Error::InvalidBuffer, "Icon buffer must be backed by wl_shm");
                    return;
                };
                if width != height || scale <= 0 {
                    icon.post_error(
                        Error::InvalidBuffer,
                        "Icon buffer must be square with positive scale",
                    );
                    return;
                }
                let entry = IconBuffer {
                    buffer,
                    size: width,
                    scale,
                };
                if let Some(existing) = data
                    .metadata
                    .buffers
                    .iter_mut()
                    .find(|existing| existing.size == width && existing.scale == scale)
                {
                    *existing = entry;
                } else {
                    data.metadata.buffers.push(entry);
                }
            }
            _ => unreachable!(),
        }
    }
}
