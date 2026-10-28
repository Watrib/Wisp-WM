//! Connector, mode and CRTC selection for the single-output direct backend.
//!
//! The selection helpers are kept free of hardware side effects so they can be
//! tested without a DRM device. The caller resolves the returned index back to
//! the concrete connector.

use crate::config::DirectMode;
use anyhow::{Context, Result, anyhow, bail};
use smithay::{
    output::Subpixel,
    reexports::drm::control::{
        Device as ControlDevice, Mode, ModeTypeFlags,
        connector::{self, State as ConnectionState},
        crtc, encoder,
    },
};

pub(super) struct ConnectorInfo {
    pub handle: connector::Handle,
    pub name: String,
    pub connected: bool,
    pub modes: Vec<Mode>,
    pub size: Option<(u32, u32)>,
    pub subpixel: Subpixel,
    pub encoders: Vec<encoder::Handle>,
}

impl ConnectorInfo {
    pub fn has_modes(&self) -> bool {
        !self.modes.is_empty()
    }
}

/// Read connectors and modes. Only the active direct backend may force a probe
/// at startup or on hotplug; the CLI reads the driver's cached state.
pub(super) fn scan<D: ControlDevice>(device: &D, force_probe: bool) -> Result<Vec<ConnectorInfo>> {
    let resources = device
        .resource_handles()
        .context("querying DRM resources")?;
    let mut connectors = Vec::with_capacity(resources.connectors().len());
    for handle in resources.connectors() {
        let info = device
            .get_connector(*handle, force_probe)
            .with_context(|| format!("querying connector {handle:?}"))?;
        connectors.push(ConnectorInfo {
            handle: info.handle(),
            name: format!("{}-{}", info.interface().as_str(), info.interface_id()),
            connected: info.state() == ConnectionState::Connected,
            modes: info.modes().to_vec(),
            size: info.size(),
            subpixel: info.subpixel().into(),
            encoders: info.encoders().to_vec(),
        });
    }
    Ok(connectors)
}

#[derive(Clone, Copy)]
struct Choice<'a> {
    name: &'a str,
    connected: bool,
    has_modes: bool,
}

fn choose_choice(choices: &[Choice<'_>], wanted: Option<&str>) -> Result<usize> {
    if let Some(wanted) = wanted {
        let Some(index) = choices
            .iter()
            .position(|choice| choice.name.eq_ignore_ascii_case(wanted))
        else {
            bail!(
                "connector {wanted:?} not found; available connectors: {}",
                available(choices)
            );
        };
        let choice = choices[index];
        if !choice.connected {
            bail!("connector {} is disconnected", choice.name);
        }
        if !choice.has_modes {
            bail!(
                "connector {} is connected but reports no modes",
                choice.name
            );
        }
        return Ok(index);
    }

    choices
        .iter()
        .position(|choice| choice.connected && choice.has_modes)
        .ok_or_else(|| {
            anyhow!(
                "no connected connector with modes; available connectors: {}",
                available(choices)
            )
        })
}

fn available(choices: &[Choice<'_>]) -> String {
    if choices.is_empty() {
        return "none".into();
    }
    choices
        .iter()
        .map(|choice| {
            let state = if choice.connected {
                "connected"
            } else {
                "disconnected"
            };
            format!("{} ({state})", choice.name)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Pick the requested connector, or the first connected one with a mode.
pub(super) fn select_connector<'a>(
    connectors: &'a [ConnectorInfo],
    wanted: Option<&str>,
) -> Result<&'a ConnectorInfo> {
    let choices = connectors
        .iter()
        .map(|connector| Choice {
            name: &connector.name,
            connected: connector.connected,
            has_modes: connector.has_modes(),
        })
        .collect::<Vec<_>>();
    let index = choose_choice(&choices, wanted)?;
    Ok(&connectors[index])
}

/// Prefer the mode the driver marks preferred, otherwise use the first mode.
pub(super) fn choose_mode_index(preferred: &[bool]) -> Option<usize> {
    preferred
        .iter()
        .position(|preferred| *preferred)
        .or_else(|| (!preferred.is_empty()).then_some(0))
}

fn choose_requested_mode_index(
    modes: &[(u16, u16, u32, bool)],
    requested: DirectMode,
) -> Option<usize> {
    let matches = |(_, (width, height, hz, _)): &(usize, &(u16, u16, u32, bool))| {
        *width == requested.width
            && *height == requested.height
            && requested.refresh_hz.is_none_or(|refresh| *hz == refresh)
    };
    modes
        .iter()
        .enumerate()
        .filter(matches)
        .find(|(_, (_, _, _, preferred))| *preferred)
        .or_else(|| modes.iter().enumerate().find(matches))
        .map(|(index, _)| index)
}

pub(super) fn select_mode(connector: &ConnectorInfo) -> Result<Mode> {
    select_mode_configured(connector, None)
}

/// Explicit mode requests never silently fall back to another resolution.
/// A refresh-free request prefers the driver's preferred matching mode.
pub(super) fn select_mode_configured(
    connector: &ConnectorInfo,
    requested: Option<DirectMode>,
) -> Result<Mode> {
    if let Some(requested) = requested {
        let modes = connector
            .modes
            .iter()
            .map(|mode| {
                let (width, height) = mode.size();
                (
                    width,
                    height,
                    mode.vrefresh(),
                    mode.mode_type().contains(ModeTypeFlags::PREFERRED),
                )
            })
            .collect::<Vec<_>>();
        return choose_requested_mode_index(&modes, requested)
            .map(|index| connector.modes[index])
            .ok_or_else(|| {
                anyhow!(
                    "connector {} does not offer requested mode {}x{}{}",
                    connector.name,
                    requested.width,
                    requested.height,
                    requested
                        .refresh_hz
                        .map_or(String::new(), |hz| format!("@{hz}"))
                )
            });
    }
    let preferred = connector
        .modes
        .iter()
        .map(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .collect::<Vec<_>>();
    let index = choose_mode_index(&preferred)
        .ok_or_else(|| anyhow!("connector {} reports no modes", connector.name))?;
    Ok(connector.modes[index])
}

/// Find a CRTC reachable through one of the connector's encoders.
pub(super) fn select_crtc<D: ControlDevice>(
    device: &D,
    connector: &ConnectorInfo,
) -> Result<crtc::Handle> {
    let resources = device
        .resource_handles()
        .context("querying DRM resources for CRTC selection")?;
    for encoder in &connector.encoders {
        let encoder = device
            .get_encoder(*encoder)
            .with_context(|| format!("querying encoder {encoder:?}"))?;
        if let Some(crtc) = resources
            .filter_crtcs(encoder.possible_crtcs())
            .into_iter()
            .next()
        {
            return Ok(crtc);
        }
    }
    bail!(
        "connector {} has no encoder/CRTC combination",
        connector.name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_mode_uses_matching_resolution_and_refresh_without_fallback() {
        let modes = [
            (1920, 1080, 60, true),
            (1280, 720, 60, false),
            (1280, 720, 75, true),
        ];
        let request = DirectMode::parse("1280x720").unwrap();
        assert_eq!(choose_requested_mode_index(&modes, request), Some(2));
        assert_eq!(
            choose_requested_mode_index(&modes, DirectMode::parse("1280x720@60").unwrap()),
            Some(1)
        );
        assert_eq!(
            choose_requested_mode_index(&modes, DirectMode::parse("1280x720@144").unwrap()),
            None
        );
        assert_eq!(
            choose_requested_mode_index(&modes, DirectMode::parse("2560x1440").unwrap()),
            None
        );
    }

    fn choices(entries: &[(&'static str, bool, bool)]) -> Vec<Choice<'static>> {
        entries
            .iter()
            .map(|(name, connected, has_modes)| Choice {
                name,
                connected: *connected,
                has_modes: *has_modes,
            })
            .collect()
    }

    #[test]
    fn explicit_connector_selection_is_case_insensitive() {
        let list = choices(&[("LVDS-1", true, true), ("HDMI-A-1", false, false)]);
        assert_eq!(choose_choice(&list, Some("lvds-1")).unwrap(), 0);
    }

    #[test]
    fn disconnected_or_unknown_explicit_connector_is_rejected() {
        let list = choices(&[("LVDS-1", false, true), ("HDMI-A-1", true, false)]);
        assert!(choose_choice(&list, Some("LVDS-1")).is_err());
        assert!(choose_choice(&list, Some("HDMI-A-1")).is_err());
        let error = choose_choice(&list, Some("DP-9")).unwrap_err().to_string();
        assert!(error.contains("not found"));
        assert!(error.contains("LVDS-1"));
    }

    #[test]
    fn automatic_selection_skips_disconnected_and_modeless_connectors() {
        let list = choices(&[
            ("DP-1", false, true),
            ("HDMI-A-1", true, false),
            ("LVDS-1", true, true),
        ]);
        assert_eq!(choose_choice(&list, None).unwrap(), 2);
        assert!(choose_choice(&list[..2], None).is_err());
    }

    #[test]
    fn preferred_mode_wins_otherwise_first() {
        assert_eq!(choose_mode_index(&[false, true, false]), Some(1));
        assert_eq!(choose_mode_index(&[false, false]), Some(0));
        assert_eq!(choose_mode_index(&[]), None);
    }
}
