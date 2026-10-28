use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Parser;
use wisp::{
    compositor::{self, BackendKind, Options},
    config::{self, Config},
};

#[derive(Parser)]
#[command(version, about = "Wisp — a small, keyboard-first Wayland compositor")]
struct Args {
    /// TOML config (default: $XDG_CONFIG_HOME/wisp/config.toml)
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Rendering backend: nested (default) or direct DRM/KMS
    #[arg(long, value_enum, default_value_t = BackendKind::Nested)]
    backend: BackendKind,
    /// DRM card to use with --backend direct, e.g. /dev/dri/card1
    #[arg(long)]
    drm_device: Option<PathBuf>,
    /// Output connector for direct mode, e.g. LVDS-1 (default: first connected)
    #[arg(long)]
    connector: Option<String>,
    /// Expected libseat seat name; libseat itself chooses the seat
    #[arg(long)]
    seat: Option<String>,
    /// Inspect DRM connectors read-only and exit; does not request DRM master
    #[arg(long)]
    probe_direct: bool,
    /// Validate configuration and exit without opening a window
    #[arg(long)]
    check_config: bool,
    /// Stream changed workspace/window state as JSON lines on stdout (for bars)
    #[arg(long)]
    status_stdout: bool,
    /// Print an annotated default config and exit
    #[arg(long)]
    print_default_config: bool,
    /// Start this argv inside Wisp, e.g. --command foot
    #[arg(long, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.print_default_config {
        print!("{}", config::EXAMPLE);
        return Ok(());
    }
    if !args.probe_direct
        && args.backend == BackendKind::Nested
        && (args.drm_device.is_some() || args.connector.is_some() || args.seat.is_some())
    {
        bail!("--drm-device, --connector and --seat require --backend direct");
    }
    if args.probe_direct {
        let seat = args
            .seat
            .clone()
            .or_else(|| std::env::var("XDG_SEAT").ok())
            .unwrap_or_else(|| "seat0".into());
        return compositor::probe_direct(
            args.drm_device.as_deref(),
            args.connector.as_deref(),
            &seat,
        );
    }
    let path = match &args.config {
        Some(path) => path.clone(),
        None => config::default_path()?,
    };
    let config = Config::load(&path, args.config.is_some())?;
    if args.check_config {
        println!("Configuration valid: {}", path.display());
        return Ok(());
    }
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "wisp=info,smithay=warn".into());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    compositor::run(
        config,
        path,
        args.command,
        Options {
            backend: args.backend,
            drm_device: args.drm_device,
            connector: args.connector,
            seat: args.seat,
            status_stdout: args.status_stdout,
        },
    )
}
