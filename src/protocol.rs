use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::layout::{Direction, LayoutKind};

pub const MAX_REQUEST_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Action {
    Terminal {},
    Spawn { argv: Vec<String> },
    FocusNext {},
    FocusPrev {},
    FocusDirection { direction: Direction },
    SwapNext {},
    SwapDirection { direction: Direction },
    ToggleFloating {},
    ToggleFullscreen {},
    ToggleOverview {},
    ToggleBlur {},
    ToggleShadows {},
    SendToScratchpad {},
    ToggleScratchpad {},
    Close {},
    Workspace { index: u8 },
    WorkspaceNext {},
    WorkspacePrev {},
    WorkspaceBackAndForth {},
    MoveToWorkspace { index: u8 },
    Layout { kind: LayoutKind },
    MasterRatio { ratio: f64 },
    Reload {},
    Quit {},
}

impl Action {
    pub fn validate(&self, workspaces: u8) -> Result<()> {
        match self {
            Self::Workspace { index } | Self::MoveToWorkspace { index } => {
                ensure!(
                    (1..=workspaces).contains(index),
                    "workspace must be between 1 and {workspaces}"
                );
            }
            Self::Spawn { argv } => {
                ensure!(
                    !argv.is_empty() && !argv[0].is_empty(),
                    "spawn requires an executable"
                );
                ensure!(
                    argv.iter().all(|s| !s.contains('\0')),
                    "arguments cannot contain NUL bytes"
                );
            }
            Self::MasterRatio { ratio } => {
                ensure!(
                    ratio.is_finite() && (0.1..=0.9).contains(ratio),
                    "ratio must be between 0.1 and 0.9"
                );
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    Status {},
    Windows {},
    Workspaces {},
    Dispatch { action: Action },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn success(data: impl Serialize) -> Self {
        Self {
            ok: true,
            data: Some(serde_json::to_value(data).expect("serializable IPC response")),
            error: None,
        }
    }

    pub fn failure(error: impl std::fmt::Display) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipc_roundtrips() {
        let request = Request::Dispatch {
            action: Action::Workspace { index: 3 },
        };
        let json = serde_json::to_string(&request).unwrap();
        let decoded: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            Request::Dispatch {
                action: Action::Workspace { index: 3 }
            }
        ));
    }

    #[test]
    fn rejects_unknown_fields_and_invalid_actions() {
        assert!(serde_json::from_str::<Request>(r#"{"command":"status","typo":1}"#).is_err());
        assert!(serde_json::from_str::<Action>(r#"{"name":"quit","typo":1}"#).is_err());
        assert!(Action::Workspace { index: 0 }.validate(9).is_err());
        assert!(Action::MasterRatio { ratio: f64::NAN }.validate(9).is_err());
        assert!(Action::Spawn { argv: vec![] }.validate(9).is_err());
    }
}
