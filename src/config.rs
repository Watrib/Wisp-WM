use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use smithay::input::keyboard::{ModifiersState, xkb};

use crate::{layout::LayoutKind, protocol::Action};

pub const EXAMPLE: &str = include_str!("../config/wisp.toml");

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub input: Input,
    pub layout: Layout,
    pub decoration: Decoration,
    pub render: Render,
    pub bindings: Option<BTreeMap<String, Action>>,
    /// Additional shortcuts, overriding matching chords in the base bindings.
    pub extra_bindings: BTreeMap<String, Action>,
    pub rules: Vec<WindowRule>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    pub modifier: Modifier,
    pub terminal: Vec<String>,
    /// Argv lists launched once after the compositor is ready, never on reload.
    pub autostart: Vec<Vec<String>>,
    pub workspaces: u8,
    /// Experimental ext-session-lock protocol; requires an external locker.
    pub session_lock_protocol: bool,
    /// Experimental XWayland/X11 window management; starts only on launch.
    pub xwayland: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            modifier: Modifier::Alt,
            terminal: vec!["foot".into()],
            autostart: Vec::new(),
            workspaces: u8::MAX,
            session_lock_protocol: false,
            xwayland: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modifier {
    Alt,
    Super,
    Ctrl,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Input {
    pub focus_follows_mouse: bool,
    pub repeat_delay: i32,
    pub repeat_rate: i32,
    pub xkb_layout: String,
    pub xkb_options: Option<String>,
}

impl Default for Input {
    fn default() -> Self {
        Self {
            focus_follows_mouse: false,
            repeat_delay: 250,
            repeat_rate: 35,
            xkb_layout: "us".into(),
            xkb_options: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Layout {
    pub default: LayoutKind,
    pub inner_gap: i32,
    pub outer_gap: i32,
    /// Remove outer gaps when exactly one tiled window is visible.
    pub smart_gaps: bool,
    /// Interpolate tiled window geometry with a critically damped spring.
    pub spring_resize: bool,
    /// Animate entry and exit from Exposé; independent of resize springs.
    pub overview_animation: bool,
    /// Optional horizontal animation when changing numbered workspaces.
    pub workspace_slide: bool,
    /// Duration for the discrete workspace slide, in milliseconds.
    pub workspace_slide_ms: u32,
    /// Fade new toplevel windows into the scene (not existing windows on workspace switches).
    pub window_open_fade: bool,
    /// Duration of the opt-in new-window fade, in milliseconds.
    pub window_open_fade_ms: u32,
    /// Natural frequency of resize springs in cycles per second.
    pub spring_frequency_hz: f64,
    pub master_ratio: f64,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            default: LayoutKind::Dwindle,
            inner_gap: 8,
            outer_gap: 12,
            smart_gaps: false,
            spring_resize: false,
            overview_animation: true,
            workspace_slide: false,
            workspace_slide_ms: 240,
            window_open_fade: false,
            window_open_fade_ms: 160,
            spring_frequency_hz: 4.0,
            master_ratio: 0.5,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Decoration {
    pub border_width: i32,
    /// Corner radius in logical pixels. 0 disables rounded corners.
    pub corner_radius: u16,
    pub active_border: Color,
    pub inactive_border: Color,
    /// Dual-color/gradient border used by the rounded border shader.
    pub gradient: BorderGradient,
    pub shadow: Shadow,
    /// Fraction of black overlaid on inactive toplevels' content areas.
    /// 0 disables dimming; fullscreen windows are never dimmed.
    pub dim_strength: f64,
    /// Optional (expensive) backdrop blur behind normal windows.
    pub blur: Blur,
    pub background: Color,
}

impl Default for Decoration {
    fn default() -> Self {
        Self {
            border_width: 2,
            corner_radius: 8,
            active_border: Color::hex("#89b4fa").unwrap(),
            inactive_border: Color::hex("#313244").unwrap(),
            gradient: BorderGradient::default(),
            shadow: Shadow::default(),
            dim_strength: 0.3,
            blur: Blur::default(),
            background: Color::hex("#11111b").unwrap(),
        }
    }
}

/// Dual-filter Kawase blur applied to the contents behind toplevels.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Blur {
    pub enabled: bool,
    pub passes: u8,
    pub offset: f64,
}

impl Default for Blur {
    fn default() -> Self {
        Self {
            enabled: false,
            passes: 2,
            offset: 2.0,
        }
    }
}

/// Gradient border colors. When disabled, borders use the active/inactive
/// colors. The angle is measured in degrees; 0 fades top to bottom and 90
/// fades left to right.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BorderGradient {
    pub enabled: bool,
    pub from: Color,
    pub to: Color,
    pub angle: u16,
}

impl Default for BorderGradient {
    fn default() -> Self {
        Self {
            enabled: false,
            from: Color::hex("#89b4fa").unwrap(),
            to: Color::hex("#f5c2e7").unwrap(),
            angle: 0,
        }
    }
}

/// Soft drop shadow behind tiled windows. `sigma` is the blur width in
/// logical pixels; the drawn area extends 2.5 sigma around the window.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Shadow {
    pub enabled: bool,
    pub sigma: f64,
    pub strength_active: f64,
    pub strength_inactive: f64,
    pub color: Color,
}

impl Default for Shadow {
    fn default() -> Self {
        Self {
            enabled: true,
            sigma: 8.0,
            strength_active: 0.55,
            strength_inactive: 0.3,
            color: Color::hex("#000000").unwrap(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Render {
    pub max_fps: u32,
    pub width: u32,
    pub height: u32,
    /// Optional direct-connector mode, WIDTHxHEIGHT or WIDTHxHEIGHT@HZ.
    pub direct_mode: Option<String>,
}

impl Default for Render {
    fn default() -> Self {
        Self {
            max_fps: 60,
            width: 1280,
            height: 800,
            direct_mode: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DirectMode {
    pub width: u16,
    pub height: u16,
    pub refresh_hz: Option<u32>,
}

impl DirectMode {
    pub fn parse(text: &str) -> Result<Self> {
        let (dimensions, refresh_hz) = match text.split_once('@') {
            Some((size, hz)) => (
                size,
                Some(
                    hz.parse::<u32>()
                        .context("invalid direct-mode refresh rate")?,
                ),
            ),
            None => (text, None),
        };
        let (width, height) = dimensions
            .split_once('x')
            .context("direct_mode must be WIDTHxHEIGHT or WIDTHxHEIGHT@HZ")?;
        let width = width.parse::<u16>().context("invalid direct-mode width")?;
        let height = height
            .parse::<u16>()
            .context("invalid direct-mode height")?;
        ensure!(
            width > 0 && height > 0,
            "direct-mode dimensions must be positive"
        );
        ensure!(
            refresh_hz.is_none_or(|hz| (1..=360).contains(&hz)),
            "direct-mode refresh must be between 1 and 360 Hz"
        );
        Ok(Self {
            width,
            height,
            refresh_hz,
        })
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(try_from = "String")]
pub struct Color(pub [f32; 4]);

impl Color {
    fn hex(value: &str) -> Result<Self> {
        let value = value
            .strip_prefix('#')
            .context("colors must start with #")?;
        ensure!(
            value.is_ascii() && matches!(value.len(), 6 | 8),
            "expected #rrggbb or #rrggbbaa"
        );
        let mut channels = [1.0; 4];
        for (index, channel) in channels.iter_mut().enumerate().take(value.len() / 2) {
            *channel = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)? as f32 / 255.0;
        }
        // Smithay expects premultiplied alpha.
        let alpha = channels[3];
        for channel in &mut channels[..3] {
            *channel *= alpha;
        }
        Ok(Self(channels))
    }
}

impl TryFrom<String> for Color {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self> {
        Self::hex(&value)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowRule {
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub floating: Option<bool>,
    pub workspace: Option<u8>,
    pub size: Option<[i32; 2]>,
}

#[derive(Debug, Clone)]
pub struct Binding {
    modifiers: u8,
    symbol: xkb::Keysym,
    pub action: Action,
}

impl Binding {
    fn parse(chord: &str, modifier: Modifier, action: Action) -> Result<Self> {
        let mut parts: Vec<_> = chord.split('+').collect();
        let name = parts.pop().context("empty key binding")?;
        ensure!(!name.is_empty(), "binding is missing a key: {chord}");
        let symbol = xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE);
        ensure!(symbol.raw() != 0, "unknown XKB key {name}");
        let mut modifiers = 0;
        for part in parts {
            let bit = match part.to_ascii_lowercase().as_str() {
                "mod" => modifier.bit(),
                "alt" => 1,
                "super" | "logo" => 2,
                "ctrl" | "control" => 4,
                "shift" => 8,
                _ => bail!("unknown modifier {part}"),
            };
            ensure!(modifiers & bit == 0, "repeated modifier in {chord}");
            modifiers |= bit;
        }
        Ok(Self {
            modifiers,
            symbol,
            action,
        })
    }

    pub fn matches(&self, mods: &ModifiersState, symbols: &[xkb::Keysym]) -> bool {
        self.modifiers == modifier_bits(mods) && symbols.contains(&self.symbol)
    }
}

impl Modifier {
    fn bit(self) -> u8 {
        match self {
            Self::Alt => 1,
            Self::Super => 2,
            Self::Ctrl => 4,
        }
    }
    pub fn held(self, mods: &ModifiersState) -> bool {
        modifier_bits(mods) & self.bit() != 0
    }
}

fn modifier_bits(mods: &ModifiersState) -> u8 {
    u8::from(mods.alt)
        | (u8::from(mods.logo) << 1)
        | (u8::from(mods.ctrl) << 2)
        | (u8::from(mods.shift) << 3)
}

impl Config {
    pub fn load(path: &Path, required: bool) -> Result<Self> {
        let config = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("invalid config: {}", path.display()))?,
            Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => {
                Self::default()
            }
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.general.workspaces > 0,
            "workspaces must be between 1 and 255"
        );
        Action::Spawn {
            argv: self.general.terminal.clone(),
        }
        .validate(self.general.workspaces)?;
        for (index, argv) in self.general.autostart.iter().enumerate() {
            Action::Spawn { argv: argv.clone() }
                .validate(self.general.workspaces)
                .with_context(|| format!("autostart entry {}", index + 1))?;
        }
        ensure!(
            (0..=128).contains(&self.layout.inner_gap)
                && (0..=128).contains(&self.layout.outer_gap),
            "gaps must be between 0 and 128"
        );
        ensure!(
            self.layout.spring_frequency_hz.is_finite()
                && (1.0..=12.0).contains(&self.layout.spring_frequency_hz),
            "spring_frequency_hz must be between 1 and 12"
        );
        ensure!(
            (80..=500).contains(&self.layout.workspace_slide_ms),
            "workspace_slide_ms must be between 80 and 500"
        );
        ensure!(
            (60..=400).contains(&self.layout.window_open_fade_ms),
            "window_open_fade_ms must be between 60 and 400"
        );
        ensure!(
            (0..=16).contains(&self.decoration.border_width),
            "border_width must be between 0 and 16"
        );
        ensure!(
            self.decoration.corner_radius <= 64,
            "corner_radius must be between 0 and 64"
        );
        ensure!(
            (0..=359).contains(&self.decoration.gradient.angle),
            "gradient angle must be between 0 and 359"
        );
        ensure!(
            self.decoration.shadow.sigma.is_finite()
                && (0.0..=64.0).contains(&self.decoration.shadow.sigma),
            "shadow sigma must be between 0 and 64"
        );
        ensure!(
            (0.0..=1.0).contains(&self.decoration.shadow.strength_active)
                && (0.0..=1.0).contains(&self.decoration.shadow.strength_inactive),
            "shadow strengths must be between 0 and 1"
        );
        ensure!(
            self.decoration.dim_strength.is_finite()
                && (0.0..=1.0).contains(&self.decoration.dim_strength),
            "dim_strength must be between 0 and 1"
        );
        ensure!(
            (1..=4).contains(&self.decoration.blur.passes),
            "blur passes must be between 1 and 4"
        );
        ensure!(
            self.decoration.blur.offset.is_finite()
                && (0.0..=8.0).contains(&self.decoration.blur.offset),
            "blur offset must be between 0 and 8"
        );
        Action::MasterRatio {
            ratio: self.layout.master_ratio,
        }
        .validate(self.general.workspaces)?;
        ensure!(
            (1..=360).contains(&self.render.max_fps),
            "max_fps must be between 1 and 360"
        );
        ensure!(
            (256..=8192).contains(&self.render.width) && (256..=8192).contains(&self.render.height),
            "nested dimensions must be between 256 and 8192"
        );
        if let Some(mode) = &self.render.direct_mode {
            DirectMode::parse(mode).context("invalid [render] direct_mode")?;
        }
        ensure!(
            (0..=1000).contains(&self.input.repeat_rate)
                && (0..=5000).contains(&self.input.repeat_delay),
            "invalid keyboard repeat settings"
        );
        ensure!(
            !self.input.xkb_layout.contains('\0')
                && self
                    .input
                    .xkb_options
                    .as_ref()
                    .is_none_or(|s| !s.contains('\0')),
            "XKB settings cannot contain NUL bytes"
        );
        for rule in &self.rules {
            ensure!(
                rule.app_id.is_some() || rule.title.is_some(),
                "a window rule needs app_id or title"
            );
            if let Some(index) = rule.workspace {
                Action::Workspace { index }.validate(self.general.workspaces)?;
            }
            if let Some([width, height]) = rule.size {
                ensure!(
                    (64..=8192).contains(&width) && (64..=8192).contains(&height),
                    "rule size must be between 64 and 8192"
                );
            }
        }
        self.compile_bindings()?;
        Ok(())
    }

    pub fn compile_bindings(&self) -> Result<Vec<Binding>> {
        let defaults = default_bindings(self.general.workspaces);
        let mut compiled = BTreeMap::new();
        for table in [
            self.bindings.as_ref().unwrap_or(&defaults),
            &self.extra_bindings,
        ] {
            let mut seen = HashSet::new();
            for (chord, action) in table {
                action.validate(self.general.workspaces)?;
                let binding = Binding::parse(chord, self.general.modifier, action.clone())
                    .with_context(|| format!("binding {chord}"))?;
                let key = (binding.modifiers, binding.symbol.raw());
                ensure!(seen.insert(key), "duplicate key binding: {chord}");
                // Normalize chords before overlaying so Mod+p and Alt+p refer
                // to the same shortcut when the configured modifier is Alt.
                compiled.insert(key, binding);
            }
        }
        Ok(compiled.into_values().collect())
    }
}

pub fn default_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("wisp/config.toml"));
    }
    Ok(
        PathBuf::from(std::env::var_os("HOME").context("HOME is not set; pass --config")?)
            .join(".config/wisp/config.toml"),
    )
}

fn default_bindings(workspaces: u8) -> BTreeMap<String, Action> {
    let mut bindings = BTreeMap::from([
        ("Mod+Return".into(), Action::Terminal {}),
        ("Mod+q".into(), Action::Close {}),
        ("Mod+j".into(), Action::FocusNext {}),
        ("Mod+k".into(), Action::FocusPrev {}),
        ("Mod+Tab".into(), Action::FocusNext {}),
        ("Mod+Shift+j".into(), Action::SwapNext {}),
        ("Mod+space".into(), Action::ToggleFloating {}),
        ("Mod+f".into(), Action::ToggleFullscreen {}),
        ("Mod+e".into(), Action::ToggleOverview {}),
        (
            "Mod+d".into(),
            Action::Layout {
                kind: LayoutKind::Dwindle,
            },
        ),
        (
            "Mod+m".into(),
            Action::Layout {
                kind: LayoutKind::Master,
            },
        ),
        ("Mod+Shift+r".into(), Action::Reload {}),
        ("Mod+Shift+Escape".into(), Action::Quit {}),
    ]);
    for index in 1..=workspaces.min(9) {
        bindings.insert(format!("Mod+{index}"), Action::Workspace { index });
        bindings.insert(
            format!("Mod+Shift+{index}"),
            Action::MoveToWorkspace { index },
        );
    }
    bindings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_and_empty_config_are_valid() {
        let example: Config = toml::from_str(EXAMPLE).unwrap();
        example.validate().unwrap();
        assert!(!example.general.xwayland);
        assert!(!example.general.session_lock_protocol);
        let empty: Config = toml::from_str("").unwrap();
        empty.validate().unwrap();
        assert!(!empty.general.xwayland);
        assert!(!empty.general.session_lock_protocol);
        let x11: Config = toml::from_str("[general]\nxwayland = true").unwrap();
        x11.validate().unwrap();
        assert!(x11.general.xwayland);
        assert_eq!(empty.compile_bindings().unwrap().len(), 31);
        Config::default().validate().unwrap();
        let small: Config = toml::from_str("[general]\nworkspaces = 3").unwrap();
        small.validate().unwrap();
        assert_eq!(small.compile_bindings().unwrap().len(), 19);
    }

    #[test]
    fn direct_mode_accepts_explicit_resolution_and_optional_refresh() {
        assert_eq!(
            DirectMode::parse("1366x768@60").unwrap().refresh_hz,
            Some(60)
        );
        assert_eq!(DirectMode::parse("1920x1080").unwrap().refresh_hz, None);
        for mode in [
            "",
            "1920",
            "0x1080",
            "1920x0",
            "1920x1080@0",
            "1920x1080@361",
            "1920x1080@60@60",
        ] {
            assert!(
                DirectMode::parse(mode).is_err(),
                "accepted invalid mode {mode}"
            );
        }
    }

    #[test]
    fn rejects_typos_and_invalid_values() {
        assert!(toml::from_str::<Config>("[layout]\ninnner_gap = 8").is_err());
        assert!(toml::from_str::<Config>("[layout]\nsmart_gaps = 1").is_err());
        let smart: Config = toml::from_str("[layout]\nsmart_gaps = true").unwrap();
        smart.validate().unwrap();
        assert!(smart.layout.smart_gaps);
        assert!(!Config::default().layout.smart_gaps);
        assert!(!Config::default().layout.window_open_fade);
        assert!(!Config::default().layout.workspace_slide);
        for value in ["59", "401"] {
            let config: Config =
                toml::from_str(&format!("[layout]\nwindow_open_fade_ms = {value}")).unwrap();
            assert!(config.validate().is_err());
        }
        for value in ["0", "13", "nan"] {
            let config: Config =
                toml::from_str(&format!("[layout]\nspring_frequency_hz = {value}")).unwrap();
            assert!(config.validate().is_err());
        }
        let config: Config = toml::from_str("[render]\nmax_fps = 0").unwrap();
        assert!(config.validate().is_err());
        assert!(Color::hex("#é0000").is_err());
        assert!(Color::hex("#zz0000").is_err());
    }

    #[test]
    fn decoration_effects_validate_ranges_and_structure() {
        let config: Config = toml::from_str("[decoration]\ncorner_radius = 65").unwrap();
        assert!(config.validate().is_err());
        let config: Config = toml::from_str("[decoration.gradient]\nangle = 400").unwrap();
        assert!(config.validate().is_err());
        let config: Config = toml::from_str("[decoration.shadow]\nsigma = -1.0").unwrap();
        assert!(config.validate().is_err());
        let config: Config = toml::from_str("[decoration.shadow]\nstrength_active = 1.5").unwrap();
        assert!(config.validate().is_err());
        let config: Config = toml::from_str("[decoration]\ndim_strength = 2.0").unwrap();
        assert!(config.validate().is_err());
        let config: Config = toml::from_str("[decoration]\ndim_strength = -0.1").unwrap();
        assert!(config.validate().is_err());
        let config: Config = toml::from_str("[decoration]\ndim_strength = 0").unwrap();
        config.validate().unwrap();
        assert_eq!(config.decoration.dim_strength, 0.0);
        for text in [
            "[decoration.blur]\npasses = 0",
            "[decoration.blur]\npasses = 5",
            "[decoration.blur]\noffset = -0.5",
            "[decoration.blur]\noffset = 9.0",
        ] {
            let config: Config = toml::from_str(text).unwrap();
            assert!(config.validate().is_err(), "accepted {text}");
        }
        // Nested blocks reject unknown fields too.
        let config = toml::from_str::<Config>("[decoration.shadow]\nenabled = true").unwrap();
        config.validate().unwrap();
        let config = toml::from_str::<Config>("[decoration]\nstroke = 2");
        assert!(config.is_err());
        let config = toml::from_str::<Config>("[decoration.shadow]\nsaturate = 1");
        assert!(config.is_err());
    }

    #[test]
    fn duplicate_normalized_bindings_are_rejected() {
        let mut config: Config = toml::from_str("").unwrap();
        config.bindings = Some(default_bindings(9));
        config
            .bindings
            .as_mut()
            .unwrap()
            .insert("Alt+Return".into(), Action::Close {});
        assert!(config.validate().is_err());
    }

    #[test]
    fn explicit_bindings_replace_defaults() {
        let empty: Config = toml::from_str("[bindings]").unwrap();
        assert!(empty.compile_bindings().unwrap().is_empty());
        let custom: Config =
            toml::from_str("[bindings]\n\"Mod+Return\" = { name = \"terminal\" }").unwrap();
        assert_eq!(custom.compile_bindings().unwrap().len(), 1);
    }

    #[test]
    fn extra_bindings_preserve_defaults_and_override_normalized_chords() {
        let config: Config = toml::from_str(
            r#"[extra_bindings]
"Mod+p" = { name = "spawn", argv = ["foot"] }
"Alt+Return" = { name = "toggle-overview" }
"#,
        )
        .unwrap();
        config.validate().unwrap();
        let bindings = config.compile_bindings().unwrap();
        assert_eq!(
            bindings.len(),
            Config::default().compile_bindings().unwrap().len() + 1
        );
        let mods = ModifiersState {
            alt: true,
            ..Default::default()
        };
        let action_for = |key| {
            &bindings
                .iter()
                .find(|binding| {
                    binding.matches(&mods, &[xkb::keysym_from_name(key, xkb::KEYSYM_NO_FLAGS)])
                })
                .unwrap()
                .action
        };
        assert_eq!(action_for("Return"), &Action::ToggleOverview {});
        assert_eq!(action_for("q"), &Action::Close {});
        assert_eq!(
            action_for("p"),
            &Action::Spawn {
                argv: vec!["foot".into()]
            }
        );

        let custom: Config = toml::from_str(
            r#"[bindings]
"Mod+Return" = { name = "terminal" }
[extra_bindings]
"Alt+Return" = { name = "toggle-overview" }
"Mod+p" = { name = "close" }
"#,
        )
        .unwrap();
        assert_eq!(custom.compile_bindings().unwrap().len(), 2);
    }

    #[test]
    fn extra_bindings_reject_ambiguous_chords_and_invalid_actions() {
        for source in [
            "[extra_bindings]\n\"Mod+p\" = { name = \"close\" }\n\"Alt+p\" = { name = \"terminal\" }",
            "[extra_bindings]\n\"Mod+p\" = { name = \"workspace\", index = 0 }",
            "[extra_bindings]\n\"Unknown+p\" = { name = \"close\" }",
        ] {
            let config: Config = toml::from_str(source).unwrap();
            assert!(config.validate().is_err(), "accepted {source}");
        }
    }

    #[test]
    fn colors_are_premultiplied() {
        let Color(c) = Color::hex("#ff000080").unwrap();
        assert!((c[0] - c[3]).abs() < f32::EPSILON);
        assert_eq!(c[1], 0.0);
    }

    #[test]
    fn autostart_requires_valid_argv_lists_and_preserves_literal_arguments() {
        assert!(Config::default().general.autostart.is_empty());
        let config: Config = toml::from_str(
            "[general]\nautostart = [[\"foot\", \"--title\", \"hello world\"], [\"printf\", \"$HOME\"]]",
        ).unwrap();
        config.validate().unwrap();
        assert_eq!(config.general.autostart[0][2], "hello world");
        assert_eq!(config.general.autostart[1][1], "$HOME");
        for argv in [
            vec![],
            vec![String::new()],
            vec!["foot".into(), "bad\0argument".into()],
        ] {
            let mut config = Config::default();
            config.general.autostart.push(argv);
            let error = config.validate().unwrap_err();
            assert!(format!("{error:#}").contains("autostart entry 1"));
        }
    }

    #[test]
    fn bindings_ignore_lock_modifiers_but_not_extra_modifiers() {
        let binding = Binding::parse("Mod+Shift+j", Modifier::Alt, Action::SwapNext {}).unwrap();
        let mut mods = ModifiersState {
            alt: true,
            shift: true,
            caps_lock: true,
            ..Default::default()
        };
        let syms = [xkb::keysym_from_name("j", xkb::KEYSYM_NO_FLAGS)];
        assert!(binding.matches(&mods, &syms));
        mods.ctrl = true;
        assert!(!binding.matches(&mods, &syms));
    }
}
