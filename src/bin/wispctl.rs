use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use wisp::{
    ipc,
    layout::{Direction, LayoutKind},
    protocol::{Action, Request},
};

#[derive(Parser)]
#[command(version, about = "Control a running Wisp compositor; output is JSON")]
struct Args {
    /// IPC socket; required outside Wisp unless WISP_SOCKET is set
    #[arg(short, long)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Status,
    Windows,
    Workspaces,
    Terminal,
    Spawn {
        #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    FocusNext,
    FocusPrev,
    /// Focus the nearest window in a direction (or thumbnail in overview)
    FocusDirection {
        #[arg(value_enum)]
        direction: Direction,
    },
    SwapNext,
    /// Exchange the focused tile with its directional neighbor
    SwapDirection {
        #[arg(value_enum)]
        direction: Direction,
    },
    ToggleFloating,
    ToggleFullscreen,
    ToggleOverview,
    ToggleBlur,
    ToggleShadows,
    SendToScratchpad,
    ToggleScratchpad,
    Close,
    Workspace {
        index: u8,
    },
    WorkspaceNext,
    WorkspacePrev,
    WorkspaceBackAndForth,
    MoveToWorkspace {
        index: u8,
    },
    Layout {
        #[arg(value_enum)]
        kind: LayoutKind,
    },
    MasterRatio {
        ratio: f64,
    },
    Reload,
    Quit,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let path = args
        .socket
        .or_else(|| std::env::var_os("WISP_SOCKET").map(PathBuf::from))
        .context("set WISP_SOCKET or pass --socket (Wisp logs its socket path on startup)")?;
    let request = match args.command {
        Command::Status => Request::Status {},
        Command::Windows => Request::Windows {},
        Command::Workspaces => Request::Workspaces {},
        command => Request::Dispatch {
            action: match command {
                Command::Terminal => Action::Terminal {},
                Command::Spawn { argv } => Action::Spawn { argv },
                Command::FocusNext => Action::FocusNext {},
                Command::FocusPrev => Action::FocusPrev {},
                Command::FocusDirection { direction } => Action::FocusDirection { direction },
                Command::SwapNext => Action::SwapNext {},
                Command::SwapDirection { direction } => Action::SwapDirection { direction },
                Command::ToggleFloating => Action::ToggleFloating {},
                Command::ToggleFullscreen => Action::ToggleFullscreen {},
                Command::ToggleOverview => Action::ToggleOverview {},
                Command::ToggleBlur => Action::ToggleBlur {},
                Command::ToggleShadows => Action::ToggleShadows {},
                Command::SendToScratchpad => Action::SendToScratchpad {},
                Command::ToggleScratchpad => Action::ToggleScratchpad {},
                Command::Close => Action::Close {},
                Command::Workspace { index } => Action::Workspace { index },
                Command::WorkspaceNext => Action::WorkspaceNext {},
                Command::WorkspacePrev => Action::WorkspacePrev {},
                Command::WorkspaceBackAndForth => Action::WorkspaceBackAndForth {},
                Command::MoveToWorkspace { index } => Action::MoveToWorkspace { index },
                Command::Layout { kind } => Action::Layout { kind },
                Command::MasterRatio { ratio } => Action::MasterRatio { ratio },
                Command::Reload => Action::Reload {},
                Command::Quit => Action::Quit {},
                Command::Status | Command::Windows | Command::Workspaces => unreachable!(),
            },
        },
    };
    let response = ipc::send(&path, &request)?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    if !response.ok {
        bail!("{}", response.error.as_deref().unwrap_or("command failed"));
    }
    Ok(())
}
