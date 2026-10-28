//! Test-only Wayland client for holding an X11 clipboard offer across a lock.
//! Run via scripts/smoke_xwayland_selection_lock.py, not on the host display.
use std::{
    io::{self, BufRead, Read, Write},
    os::fd::AsFd,
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop,
    protocol::{
        wl_buffer, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer,
        wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
    },
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
struct State {
    globals: Vec<(u32, String, u32)>,
    selection: Option<wl_data_offer::WlDataOffer>,
    mime_types: Vec<String>,
    configured: bool,
    buffer: Option<wl_buffer::WlBuffer>,
    surface: Option<wl_surface::WlSurface>,
}

impl State {
    fn bind<I: Proxy + 'static>(
        &self,
        registry: &wl_registry::WlRegistry,
        qh: &QueueHandle<Self>,
        version: u32,
    ) -> I
    where
        Self: Dispatch<I, ()>,
    {
        let (name, _, advertised) = self
            .globals
            .iter()
            .find(|(_, interface, _)| interface == I::interface().name)
            .unwrap_or_else(|| panic!("missing {}", I::interface().name));
        registry.bind(*name, version.min(*advertised), qh, ())
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            state.globals.push((name, interface, version));
        }
    }
}
impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { .. } => {}
            wl_data_device::Event::Selection { id } => {
                state.selection = id;
            }
            _ => {}
        }
    }
    wayland_client::event_created_child!(State, wl_data_device::WlDataDevice, [0 => (wl_data_offer::WlDataOffer, ())]);
}
impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state.mime_types.push(mime_type);
        }
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        proxy: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            proxy.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
            if let (Some(surface), Some(buffer)) = (&state.surface, &state.buffer) {
                surface.attach(Some(buffer), 0, 0);
                surface.commit();
            }
            state.configured = true;
        }
    }
}
delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_data_device_manager::WlDataDeviceManager);
delegate_noop!(State: ignore xdg_toplevel::XdgToplevel);

fn main() -> anyhow::Result<()> {
    let mode = std::env::args().nth(1).expect("use old-offer or in-flight");
    anyhow::ensure!(mode == "old-offer" || mode == "in-flight", "unknown mode");
    let connection = Connection::connect_to_env()?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let registry = connection.display().get_registry(&qh, ());
    let mut state = State::default();
    queue.roundtrip(&mut state)?;
    let seat = state.bind::<wl_seat::WlSeat>(&registry, &qh, 7);
    let device = state
        .bind::<wl_data_device_manager::WlDataDeviceManager>(&registry, &qh, 3)
        .get_data_device(&seat, &qh, ());
    let compositor = state.bind::<wl_compositor::WlCompositor>(&registry, &qh, 4);
    let shm = state.bind::<wl_shm::WlShm>(&registry, &qh, 1);
    let file = tempfile::tempfile_in("/tmp/opencode")?;
    file.set_len(64 * 64 * 4)?;
    let pool = shm.create_pool(file.as_fd(), 64 * 64 * 4, &qh, ());
    let buffer = pool.create_buffer(0, 64, 64, 64 * 4, wl_shm::Format::Argb8888, &qh, ());
    pool.destroy();
    let surface = compositor.create_surface(&qh, ());
    state.surface = Some(surface.clone());
    state.buffer = Some(buffer);
    let shell = state.bind::<xdg_wm_base::XdgWmBase>(&registry, &qh, 6);
    let xdg = shell.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title("Wisp held selection test".into());
    surface.commit();

    let deadline = Instant::now() + Duration::from_secs(12);
    while !state.configured || state.selection.is_none() {
        anyhow::ensure!(
            Instant::now() < deadline,
            "no focused X11 selection offer (configured={}, offers={}, mime_types={:?})",
            state.configured,
            state.selection.is_some(),
            state.mime_types
        );
        queue.roundtrip(&mut state)?;
    }
    let offer = state.selection.clone().unwrap();
    let mime = state
        .mime_types
        .iter()
        .find(|mime| mime.as_str() == "UTF8_STRING")
        .or_else(|| {
            state
                .mime_types
                .iter()
                .find(|mime| mime.starts_with("text/plain"))
        })
        .ok_or_else(|| anyhow::anyhow!("selection has no text mime: {:?}", state.mime_types))?
        .clone();
    println!("READY {mime}");
    io::stdout().flush()?;

    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    anyhow::ensure!(line.trim() == "receive", "expected receive command");
    let (mut reader, writer) = UnixStream::pair()?;
    reader.set_read_timeout(Some(Duration::from_secs(5)))?;
    offer.receive(mime, writer.as_fd());
    drop(writer);
    connection.flush()?;
    if mode == "in-flight" {
        let mut first = [0; 4096];
        let count = reader.read(&mut first)?;
        anyhow::ensure!(count > 0, "transfer did not start before locking");
        println!("STARTED {count}");
        io::stdout().flush()?;
        line.clear();
        io::stdin().lock().read_line(&mut line)?;
        anyhow::ensure!(line.trim() == "check", "expected check command");
    }
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    println!("AFTER_LOCK {}", bytes.len());
    io::stdout().flush()?;
    drop(device);
    Ok(())
}
