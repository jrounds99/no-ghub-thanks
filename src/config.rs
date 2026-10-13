use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    /// LEDs dark.
    Off,
    /// Solid color.
    Static,
    /// Fade in/out ("pulse").
    Breathe,
    /// Hue rotation.
    Cycle,
    /// Solid color that follows battery level (green / yellow / red),
    /// pulsing green while charging.
    Battery,
}

/// What the tray icon shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrayIcon {
    /// Battery percentage + level bar.
    #[default]
    Battery,
    /// The app's own icon (battery still in the tooltip and panel).
    App,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub effect: Effect,
    /// "#RRGGBB" (static / breathe)
    #[serde(default = "default_color")]
    pub color: String,
    /// Breathe / cycle period in milliseconds.
    #[serde(default = "default_period")]
    pub period_ms: u16,
}

fn default_color() -> String {
    "#FFFFFF".into()
}
fn default_period() -> u16 {
    3000
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Master switch. false = LEDs off, re-enforced on every connect and wake.
    pub lighting_enabled: bool,
    /// Profile applied when lighting is enabled.
    pub active_profile: String,
    /// 10..=100 percent.
    pub brightness: u8,
    /// Toast when battery falls to this % while discharging (0 = never).
    pub low_battery_alert: u8,
    /// Count mouse clicks for the stats page (local only).
    #[serde(default)]
    pub count_clicks: bool,
    /// Battery saver: lighting off at or below this % while discharging (0 = never).
    #[serde(default)]
    pub lights_off_below: u8,
    /// Keep the icon in the visible part of the tray (Windows 11 per-app setting).
    #[serde(default = "default_true")]
    pub always_show_in_tray: bool,
    /// Install new builds without asking (otherwise: notification + button).
    #[serde(default)]
    pub auto_update: bool,
    #[serde(default)]
    pub tray_icon: TrayIcon,
    pub profiles: Vec<Profile>,
}

impl Default for Config {
    fn default() -> Self {
        let p = |name: &str, effect, color: &str, period_ms| Profile {
            name: name.into(),
            effect,
            color: color.into(),
            period_ms,
        };
        Config {
            lighting_enabled: false,
            active_profile: "White".into(),
            brightness: 100,
            low_battery_alert: 15,
            count_clicks: false,
            lights_off_below: 0,
            always_show_in_tray: true,
            auto_update: false,
            tray_icon: TrayIcon::Battery,
            profiles: vec![
                p("White", Effect::Static, "#FFFFFF", 3000),
                p("Red", Effect::Static, "#FF0000", 3000),
                p("Blue", Effect::Static, "#0040FF", 3000),
                p("Blue pulse", Effect::Breathe, "#0040FF", 3000),
                p("Red pulse", Effect::Breathe, "#FF0000", 2000),
                p("Rainbow", Effect::Cycle, "#FFFFFF", 6000),
                p("Battery level", Effect::Battery, "#FFFFFF", 3000),
            ],
        }
    }
}

impl Config {
    pub fn dir() -> PathBuf {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| ".".into());
        base.join("no-ghub-thanks")
    }

    pub fn path() -> PathBuf {
        Self::dir().join("config.json")
    }

    /// Load config. A missing file is created with defaults; a malformed file
    /// is left untouched on disk and Err carries the parse message.
    pub fn load() -> Result<Config, String> {
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => {
                let mut c: Config = serde_json::from_str(&s).map_err(|e| e.to_string())?;
                c.brightness = c.brightness.clamp(10, 100);
                if c.profiles.is_empty() {
                    c.profiles = Config::default().profiles;
                }
                Ok(c)
            }
            Err(_) => {
                let c = Config::default();
                c.save();
                Ok(c)
            }
        }
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(Self::dir());
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(), s);
        }
    }

    /// Index of the active profile (falls back to the first).
    pub fn active_index(&self) -> usize {
        self.profiles.iter().position(|p| p.name == self.active_profile).unwrap_or(0)
    }

    /// A profile name not already taken, based on `base`.
    pub fn unique_name(&self, base: &str) -> String {
        let base = base.trim();
        let base = if base.is_empty() { "Custom" } else { base };
        if !self.profiles.iter().any(|p| p.name == base) {
            return base.to_string();
        }
        (2..).map(|n| format!("{base} {n}")).find(|n| !self.profiles.iter().any(|p| &p.name == n)).unwrap()
    }

    pub fn active(&self) -> Profile {
        self.profiles
            .iter()
            .find(|p| p.name == self.active_profile)
            .cloned()
            .unwrap_or_else(|| self.profiles[0].clone())
    }
}

pub fn parse_color(s: &str) -> (u8, u8, u8) {
    let h = s.trim().trim_start_matches('#');
    let v = u32::from_str_radix(h, 16).unwrap_or(0xFFFFFF);
    ((v >> 16) as u8, (v >> 8) as u8, v as u8)
}
