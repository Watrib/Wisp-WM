//! Read-only DRM inspection used by `wisp --probe-direct`.
//!
//! This module intentionally never builds a `DrmDeviceFd`: that type tries to
//! become DRM master. A plain read-only `File` wrapper can query resources and
//! connector state while the user's desktop keeps ownership of the display.

use std::{
    fs::File,
    os::fd::{AsFd, BorrowedFd},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, ensure};
use smithay::{
    backend::{
        drm::{DrmNode, NodeType},
        udev::{all_gpus, primary_gpu},
    },
    output::Mode as OutputMode,
    reexports::drm::{
        self, Device as _,
        control::{Device as ControlDevice, ModeTypeFlags},
    },
};

use super::output;

struct CardFd(File);

impl AsFd for CardFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for CardFd {}
impl ControlDevice for CardFd {}

fn default_device(seat: &str) -> Result<PathBuf> {
    if let Some(path) = primary_gpu(seat).context("enumerating DRM devices")? {
        return Ok(path);
    }
    all_gpus(seat)
        .context("enumerating DRM devices")?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no DRM card found for seat {seat:?}"))
}

pub(super) fn run(device: Option<&Path>, connector: Option<&str>, seat: &str) -> Result<()> {
    let wanted = connector;
    let path = device_path(device, seat)?;
    let file = File::open(&path).with_context(|| {
        format!(
            "opening {} read-only (needs access to the DRM card; try the video group or an active logind session)",
            path.display()
        )
    })?;
    let card = CardFd(file);

    println!("DRM device: {}", path.display());
    match card.get_driver() {
        Ok(driver) => println!(
            "Driver: {} — {}",
            driver.name().to_string_lossy(),
            driver.description().to_string_lossy()
        ),
        Err(error) => println!("Driver: unavailable ({error})"),
    }

    let connectors = output::scan(&card, false).context("reading cached connectors (read-only)")?;
    ensure!(
        !connectors.is_empty(),
        "the DRM device reports no connectors"
    );
    if let Some(wanted) = wanted {
        ensure!(
            connectors
                .iter()
                .any(|connector| connector.name.eq_ignore_ascii_case(wanted)),
            "connector {wanted:?} not found; available: {}",
            connectors
                .iter()
                .map(|connector| connector.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    for info in &connectors {
        let selected = wanted.is_some_and(|wanted| info.name.eq_ignore_ascii_case(wanted));
        println!(
            "\nConnector {}{}: {}",
            info.name,
            if selected { " (selected)" } else { "" },
            if info.connected {
                "connected"
            } else {
                "disconnected"
            }
        );
        if !info.connected || info.modes.is_empty() {
            continue;
        }
        if let Some((width, height)) = info.size {
            println!("  physical size: {width}×{height} mm");
        }
        for mode in &info.modes {
            let output_mode = OutputMode::from(*mode);
            let preferred = mode.mode_type().contains(ModeTypeFlags::PREFERRED);
            println!(
                "  {:>5}×{:<5} @ {:>7.2} Hz{}",
                output_mode.size.w,
                output_mode.size.h,
                output_mode.refresh as f64 / 1000.0,
                if preferred { "  preferred" } else { "" }
            );
        }
        match output::select_mode(info) {
            Ok(mode) => {
                let mode = OutputMode::from(mode);
                println!(
                    "  selected mode: {}×{} @ {:.2} Hz",
                    mode.size.w,
                    mode.size.h,
                    mode.refresh as f64 / 1000.0
                );
            }
            Err(error) => println!("  selected mode: none ({error})"),
        }
        match output::select_crtc(&card, info) {
            Ok(crtc) => println!("  usable CRTC: {crtc:?}"),
            Err(error) => println!("  usable CRTC: none ({error})"),
        }
    }

    if let Some(wanted) = wanted {
        let connector = output::select_connector(&connectors, Some(wanted))?;
        let _ = output::select_mode(connector)?;
        let _ = output::select_crtc(&card, connector)?;
        println!(
            "\n{} has a usable mode and CRTC; scanout and seat access have not been tested.",
            connector.name
        );
    }
    println!("\nRead-only probe complete; DRM master was not requested.");
    Ok(())
}

/// Same defaulting rule as probe, used by the direct backend.
pub(super) fn device_path(device: Option<&Path>, seat: &str) -> Result<PathBuf> {
    let path = match device {
        Some(path) => path.to_owned(),
        None => default_device(seat)?,
    };
    let node = DrmNode::from_path(&path)
        .with_context(|| format!("identifying DRM device {}", path.display()))?;
    ensure!(
        node.ty() == NodeType::Primary,
        "{} is not a DRM card; use /dev/dri/cardN rather than a render node",
        path.display()
    );
    Ok(path)
}
