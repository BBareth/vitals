//! User configuration, persisted as TOML in %APPDATA%\Vitals\config.toml.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub window: WindowCfg,
    pub hotkey: HotkeyCfg,
    pub update: UpdateCfg,
    pub panels: PanelCfg,
    pub bridge: BridgeCfg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowCfg {
    /// 1 = primary, 2 = second monitor, and so on (in Windows' enumeration order).
    pub monitor: u32,
    /// top-left | top-right | bottom-left | bottom-right | center
    pub anchor: String,
    pub margin_x: i32,
    pub margin_y: i32,
    pub width: u32,
    pub opacity: f32,
    /// When true the panel ignores the mouse entirely; clicks land on whatever is beneath.
    pub click_through: bool,
    pub start_hidden: bool,
    /// Extra multiplier applied on top of the monitor's own DPI scaling.
    pub scale: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyCfg {
    /// Modifiers joined with '+', e.g. "Win+Alt+O", "Ctrl+Shift+F12".
    pub toggle: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateCfg {
    pub interval_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PanelCfg {
    pub cpu: bool,
    pub cpu_cores: bool,
    pub gpu: bool,
    pub memory: bool,
    pub disk: bool,
    pub network: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BridgeCfg {
    /// auto | none | hwinfo | lhm  -- optional source for CPU package temperature.
    pub source: String,
    pub lhm_url: String,
}

impl Default for WindowCfg {
    fn default() -> Self {
        Self {
            monitor: 2,
            anchor: "top-right".into(),
            margin_x: 24,
            margin_y: 24,
            width: 300,
            opacity: 1.0,
            click_through: true,
            start_hidden: false,
            scale: 1.0,
        }
    }
}

impl Default for HotkeyCfg {
    fn default() -> Self {
        // Win+Alt+G is taken by Game Bar's "record that"; Win+Alt+O is free.
        Self { toggle: "Win+Alt+O".into() }
    }
}

impl Default for UpdateCfg {
    fn default() -> Self {
        Self { interval_ms: 1000 }
    }
}

impl Default for PanelCfg {
    fn default() -> Self {
        // Core bars and the network rows are off by default to keep the
        // panel compact; both are one flag away.
        Self { cpu: true, cpu_cores: false, gpu: true, memory: true, disk: true, network: false }
    }
}

impl Default for BridgeCfg {
    fn default() -> Self {
        Self { source: "auto".into(), lhm_url: "http://127.0.0.1:8085/data.json".into() }
    }
}

pub fn config_dir() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("Vitals")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

impl Config {
    /// Loads the config, writing a commented default file if none exists yet.
    /// A malformed file is never overwritten -- we fall back to defaults and say so.
    pub fn load() -> (Self, Option<String>) {
        let path = config_path();
        if !path.exists() {
            let cfg = Config::default();
            let _ = std::fs::create_dir_all(config_dir());
            let _ = std::fs::write(&path, DEFAULT_TOML);
            return (cfg, None);
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(cfg) => (cfg, None),
                Err(e) => (Config::default(), Some(format!("config.toml is invalid ({e}); using defaults"))),
            },
            Err(e) => (Config::default(), Some(format!("could not read config.toml ({e}); using defaults"))),
        }
    }
}

/// Rewrites a single `key = value` line in place.
///
/// The config file is meant to be read and edited by hand, so re-serialising
/// the whole struct (which would discard every comment) is not acceptable for
/// the small settings the tray menu can change.
pub fn patch_scalar(key: &str, value: &str) {
    let path = config_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let mut changed = false;
    let out: Vec<String> = text
        .lines()
        .map(|line| {
            if changed {
                return line.to_string();
            }
            let trimmed = line.trim_start();
            let Some(rest) = trimmed.strip_prefix(key) else {
                return line.to_string();
            };
            // Guard against `margin_x` matching a hypothetical `margin_xy`.
            if !rest.trim_start().starts_with('=') {
                return line.to_string();
            }
            changed = true;
            let indent = &line[..line.len() - trimmed.len()];
            match line.split_once('#') {
                Some((_, comment)) => format!("{indent}{key} = {value}  #{comment}"),
                None => format!("{indent}{key} = {value}"),
            }
        })
        .collect();

    if changed {
        let _ = std::fs::write(&path, out.join("\r\n") + "\r\n");
    }
}

pub const DEFAULT_TOML: &str = r#"# Vitals - configuration
# Edit, then pick "Reload config" from the tray menu (no restart needed).

[window]
monitor       = 2            # 1 = primary display, 2 = second, ...
anchor        = "top-right"  # top-left | top-right | bottom-left | bottom-right | center
margin_x      = 24
margin_y      = 24
width         = 300          # logical pixels; scaled by the monitor's DPI
opacity       = 1.0          # lower for a translucent panel; text bleeds through below ~0.95
click_through = true         # false lets you drag the panel with the mouse
start_hidden  = false
scale         = 1.0          # extra size multiplier on top of DPI scaling

[hotkey]
toggle = "Win+Alt+O"         # show/hide, like Win+G for the Game Bar

[update]
interval_ms = 1000

[panels]
cpu       = true
cpu_cores = false            # per-thread P/E core bars; adds about 35px
gpu       = true
memory    = true
disk      = true
network   = false            # down/up throughput rows

[bridge]
# CPU package temperature needs kernel-level MSR access, which Vitals
# deliberately does not ship a driver for. If you run HWiNFO or
# LibreHardwareMonitor, Vitals will read the temperature from it.
#   auto   - try HWiNFO shared memory, then LibreHardwareMonitor
#   none   - never look; CPU temp shows as a dash
#   hwinfo - HWiNFO only (enable Settings > Shared Memory Support)
#   lhm    - LibreHardwareMonitor only (enable Options > Remote Web Server)
source  = "auto"
lhm_url = "http://127.0.0.1:8085/data.json"
"#;
