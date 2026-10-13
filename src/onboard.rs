//! HID++ 0x8100 onboard profiles: the settings the mouse keeps in its own
//! flash (DPI, report rate, buttons, G-shift layer, onboard lighting).
//!
//! Layout (profile format 0x05, G502 X family; cross-checked against
//! libratbag and a dump of a real G502 X PLUS):
//!
//!   sector 0x0000  directory: per profile [addr_hi, addr_lo, enabled, 0xFF], 0xFFFF end
//!   sector 0x0001.. user profiles, 0x0101.. ROM (factory) profiles
//!   profile:  0 report rate (ms)   1 default DPI index   2 DPI-shift index
//!             3..13 five DPI levels (u16 LE)   13..16 color   ...
//!             32..96 16 button bindings (4 bytes)   96..160 G-shift bindings
//!             160..208 name   208..252 4 LED slots (11 bytes)   252 custom anim
//!   last 2 bytes of every sector: CRC-CCITT (seed 0xFFFF), big-endian
//!
//! Edits are applied onto the raw sector bytes, so anything not understood
//! here (macros, reserved fields) is written back untouched.

use serde::{Deserialize, Serialize};

pub const FN_INFO: u8 = 0x0;
pub const FN_SET_PROFILE: u8 = 0x3;
pub const FN_GET_PROFILE: u8 = 0x4;
pub const FN_READ: u8 = 0x5;
pub const FN_WRITE_START: u8 = 0x6;
pub const FN_WRITE: u8 = 0x7;
pub const FN_WRITE_END: u8 = 0x8;

const OFF_BUTTONS: usize = 32;
const OFF_GSHIFT: usize = 96;
const OFF_LEDS: usize = 208;
const LED_SIZE: usize = 11;
const OFF_CUSTOM_ANIM: usize = 252;

/// Physical names for the G502 X family, by binding slot.
pub const G502X_BUTTONS: &[&str] = &[
    "Left click",
    "Right click",
    "Wheel click",
    "Back (G4)",
    "Sniper (G6)",
    "Forward (G5)",
    "Wheel tilt left",
    "Wheel tilt right",
    "Profile (G9)",
    "DPI up (G8)",
    "DPI down (G7)",
    "Wheel scroll down",
    "Wheel scroll up",
];

/// Binding slots in a profile (the mouse reports 11 buttons, but slots 11-12
/// hold the wheel's scroll actions, which are remappable too).
pub const SLOTS: usize = 16;

pub fn crc_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        let t = (crc >> 8) ^ b as u16;
        crc <<= 8;
        let mut q = t ^ (t >> 4);
        crc ^= q;
        q <<= 5;
        crc ^= q;
        q <<= 7;
        crc ^= q;
    }
    crc
}

pub fn crc_ok(sector: &[u8]) -> bool {
    let n = sector.len();
    n > 2 && crc_ccitt(&sector[..n - 2]) == u16::from_be_bytes([sector[n - 2], sector[n - 1]])
}

pub fn seal(sector: &mut [u8]) {
    let n = sector.len();
    let c = crc_ccitt(&sector[..n - 2]);
    sector[n - 2..].copy_from_slice(&c.to_be_bytes());
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Descriptor {
    pub profile_format: u8,
    pub profile_count: u8,
    pub rom_profile_count: u8,
    pub button_count: u8,
    pub sector_size: u16,
}

impl Descriptor {
    pub fn parse(r: &[u8]) -> Descriptor {
        Descriptor {
            profile_format: r[1],
            profile_count: r[3],
            rom_profile_count: r[4],
            button_count: r[5],
            sector_size: u16::from_be_bytes([r[7], r[8]]),
        }
    }
}

/// What a button does. Unknown encodings (macros etc.) round-trip as Raw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    Mouse { button: u8 },
    Key { modifiers: u8, key: u8 },
    Media { code: u16 },
    Special { code: u8 },
    Disabled,
    Raw { bytes: String },
}

impl Action {
    pub fn decode(b: &[u8]) -> Action {
        match (b[0], b[1]) {
            (0x80, 0x01) => {
                let mask = u16::from_be_bytes([b[2], b[3]]);
                if mask.count_ones() == 1 {
                    Action::Mouse { button: mask.trailing_zeros() as u8 + 1 }
                } else {
                    Action::Raw { bytes: hex(b) }
                }
            }
            (0x80, 0x02) => Action::Key { modifiers: b[2], key: b[3] },
            (0x80, 0x03) => Action::Media { code: u16::from_be_bytes([b[2], b[3]]) },
            (0x90, code) => Action::Special { code },
            (0xFF, _) => Action::Disabled,
            _ => Action::Raw { bytes: hex(b) },
        }
    }

    pub fn encode(&self) -> Option<[u8; 4]> {
        Some(match self {
            Action::Mouse { button } if (1..=16).contains(button) => {
                let m = (1u16 << (button - 1)).to_be_bytes();
                [0x80, 0x01, m[0], m[1]]
            }
            Action::Mouse { .. } => return None,
            Action::Key { modifiers, key } => [0x80, 0x02, *modifiers, *key],
            Action::Media { code } => {
                let c = code.to_be_bytes();
                [0x80, 0x03, c[0], c[1]]
            }
            Action::Special { code } => [0x90, *code, 0x00, 0x00],
            Action::Disabled => [0xFF; 4],
            Action::Raw { bytes } => {
                let v = unhex(bytes)?;
                v.try_into().ok()?
            }
        })
    }
}

/// Onboard lighting: what the mouse shows when no software is driving it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum OnboardLight {
    Off,
    Static { color: String },
    Breathe { color: String, period_ms: u16 },
    Cycle { period_ms: u16 },
    /// Firmware animation or anything else: left exactly as stored.
    Other { code: u8 },
}

impl OnboardLight {
    fn decode(l: &[u8]) -> OnboardLight {
        let color = || format!("#{:02X}{:02X}{:02X}", l[1], l[2], l[3]);
        match l[0] {
            0x00 => OnboardLight::Off,
            0x01 => OnboardLight::Static { color: color() },
            0x0A => OnboardLight::Breathe { color: color(), period_ms: u16::from_be_bytes([l[4], l[5]]) },
            0x03 => OnboardLight::Cycle { period_ms: u16::from_be_bytes([l[6], l[7]]) },
            code => OnboardLight::Other { code },
        }
    }

    /// None = leave the slot untouched.
    fn encode(&self) -> Option<[u8; LED_SIZE]> {
        let mut l = [0u8; LED_SIZE];
        let rgb = |c: &str| crate::config::parse_color(c);
        match self {
            OnboardLight::Off => {}
            OnboardLight::Static { color } => {
                let (r, g, b) = rgb(color);
                l[..4].copy_from_slice(&[0x01, r, g, b]);
            }
            OnboardLight::Breathe { color, period_ms } => {
                let (r, g, b) = rgb(color);
                let p = period_ms.clamp(&500, &20000).to_be_bytes();
                l[..7].copy_from_slice(&[0x0A, r, g, b, p[0], p[1], 0x00]);
                l[7] = 0; // 0 = 100% intensity
            }
            OnboardLight::Cycle { period_ms } => {
                let p = period_ms.clamp(&1000, &20000).to_be_bytes();
                l[0] = 0x03;
                l[6..8].copy_from_slice(&p);
                l[8] = 0;
            }
            OnboardLight::Other { .. } => return None,
        }
        Some(l)
    }
}

/// Editable view of one profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub slot: u8, // 1-based
    pub enabled: bool,
    pub report_rate_hz: u16,
    pub dpi: [u16; 5],
    pub default_dpi: u8,
    pub shift_dpi: u8,
    pub buttons: Vec<Action>,
    pub gshift: Vec<Action>,
    pub lighting: OnboardLight,
    /// Read from a factory ROM sector because the user sector was invalid.
    pub from_rom: bool,
}

impl Profile {
    pub fn parse(slot: u8, enabled: bool, s: &[u8], from_rom: bool) -> Profile {
        let rate_ms = s[0].max(1) as u16;
        let dpi: [u16; 5] = std::array::from_fn(|i| u16::from_le_bytes([s[3 + 2 * i], s[4 + 2 * i]]));
        let n = SLOTS;
        Profile {
            slot,
            enabled,
            report_rate_hz: 1000 / rate_ms,
            dpi,
            default_dpi: s[1].min(4),
            shift_dpi: s[2].min(4),
            buttons: (0..n).map(|i| Action::decode(&s[OFF_BUTTONS + 4 * i..OFF_BUTTONS + 4 * i + 4])).collect(),
            gshift: (0..n).map(|i| Action::decode(&s[OFF_GSHIFT + 4 * i..OFF_GSHIFT + 4 * i + 4])).collect(),
            // G502 X PLUS: the active LED is the 2nd slot.
            lighting: OnboardLight::decode(&s[OFF_LEDS + LED_SIZE..OFF_LEDS + 2 * LED_SIZE]),
            from_rom,
        }
    }

    /// Write this profile's editable fields onto a copy of `base` (the raw
    /// sector it was read from) and reseal the CRC.
    pub fn apply_to(&self, base: &[u8]) -> Result<Vec<u8>, String> {
        let mut s = base.to_vec();
        if s.len() < OFF_CUSTOM_ANIM + 3 {
            return Err("sector too small".into());
        }
        s[0] = match self.report_rate_hz {
            1000 => 1,
            500 => 2,
            250 => 4,
            125 => 8,
            other => return Err(format!("unsupported report rate {other} Hz")),
        };
        s[1] = self.default_dpi.min(4);
        s[2] = self.shift_dpi.min(4);
        for (i, d) in self.dpi.iter().enumerate() {
            if !(100..=25600).contains(d) {
                return Err(format!("DPI {d} out of range (100-25600)"));
            }
            s[3 + 2 * i..5 + 2 * i].copy_from_slice(&d.to_le_bytes());
        }
        for (i, a) in self.buttons.iter().enumerate().take(16) {
            let b = a.encode().ok_or_else(|| format!("button {} has an invalid action", i + 1))?;
            s[OFF_BUTTONS + 4 * i..OFF_BUTTONS + 4 * i + 4].copy_from_slice(&b);
        }
        for (i, a) in self.gshift.iter().enumerate().take(16) {
            let b = a.encode().ok_or_else(|| format!("G-shift {} has an invalid action", i + 1))?;
            s[OFF_GSHIFT + 4 * i..OFF_GSHIFT + 4 * i + 4].copy_from_slice(&b);
        }
        if let Some(l) = self.lighting.encode() {
            // Same effect in all four slots (normal + alternate, both zones),
            // and no firmware custom animation, so what's set is what shows.
            for k in 0..4 {
                s[OFF_LEDS + k * LED_SIZE..OFF_LEDS + (k + 1) * LED_SIZE].copy_from_slice(&l);
            }
            s[OFF_CUSTOM_ANIM] = 0x00;
        }
        seal(&mut s);
        Ok(s)
    }
}

/// Directory sector with the given enabled flags (profile addresses 1..=n).
pub fn directory(enabled: &[bool], sector_size: usize, base: Option<&[u8]>) -> Vec<u8> {
    let mut s = vec![0xFFu8; sector_size];
    for (i, &on) in enabled.iter().enumerate() {
        // Keep the 4th byte as the mouse had it (G HUB writes 0xFF there).
        let tail = base.map(|b| b[4 * i + 3]).unwrap_or(0xFF);
        s[4 * i..4 * i + 4].copy_from_slice(&[0x00, i as u8 + 1, on as u8, tail]);
    }
    // End marker: 0xFFFF; the rest of the sector stays 0xFF (as the mouse has it).
    seal(&mut s);
    s
}

pub fn parse_directory(s: &[u8], count: usize) -> Vec<bool> {
    (0..count)
        .map(|i| {
            let d = &s[4 * i..4 * i + 4];
            u16::from_be_bytes([d[0], d[1]]) != 0xFFFF && d[2] != 0
        })
        .collect()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real sectors dumped from a G502 X PLUS (factory profiles).
    fn fixture(sector: &str) -> Vec<u8> {
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/g502xplus-onboard.json")).unwrap();
        unhex(v["sectors"][sector].as_str().unwrap()).unwrap()
    }

    #[test]
    fn device_crcs_verify() {
        for s in ["0000", "0001", "0002"] {
            assert!(crc_ok(&fixture(s)), "sector {s}");
        }
    }

    #[test]
    fn parse_profile_1() {
        let s = fixture("0001");
        let p = Profile::parse(1, true, &s, false);
        assert_eq!(p.report_rate_hz, 1000);
        assert_eq!(p.dpi, [800, 1200, 1600, 2400, 3200]);
        assert_eq!((p.default_dpi, p.shift_dpi), (2, 0));
        assert_eq!(p.buttons[0], Action::Mouse { button: 1 });
        assert_eq!(p.buttons[4], Action::Special { code: 0x07 }); // DPI shift
        assert_eq!(p.buttons[5], Action::Mouse { button: 5 });
        assert_eq!(p.buttons[10], Action::Special { code: 0x04 }); // DPI down
        assert_eq!(p.gshift[0], Action::Disabled);
        assert_eq!(p.lighting, OnboardLight::Other { code: 0x0F });
    }

    #[test]
    fn parse_profile_2_gshift() {
        let p = Profile::parse(2, true, &fixture("0002"), false);
        assert_eq!(p.report_rate_hz, 500);
        assert_eq!(p.buttons[4], Action::Special { code: 0x0B }); // G-shift
        assert_eq!(p.gshift[2], Action::Key { modifiers: 0x01, key: 0x17 }); // Ctrl+T
        assert_eq!(p.gshift[11], Action::Media { code: 0x00EA }); // volume down
    }

    #[test]
    fn unchanged_profile_round_trips_byte_for_byte() {
        for sec in ["0001", "0002"] {
            let s = fixture(sec);
            let p = Profile::parse(1, true, &s, false);
            assert_eq!(p.apply_to(&s).unwrap(), s, "sector {sec}");
        }
    }

    #[test]
    fn edits_touch_only_their_fields() {
        let s = fixture("0001");
        let mut p = Profile::parse(1, true, &s, false);
        p.dpi[0] = 400;
        p.report_rate_hz = 500;
        p.buttons[8] = Action::Key { modifiers: 0x01, key: 0x06 }; // Ctrl+C
        p.lighting = OnboardLight::Off;
        let out = p.apply_to(&s).unwrap();
        assert!(crc_ok(&out));
        assert_eq!(out[0], 2);
        assert_eq!(&out[3..5], &400u16.to_le_bytes());
        assert_eq!(&out[64..68], &[0x80, 0x02, 0x01, 0x06]);
        assert!(out[208..252].iter().all(|&b| b == 0));
        assert_eq!(out[252], 0);
        // everything else identical
        for i in (5..32).chain(68..208) {
            if i != 0 {
                assert_eq!(out[i], s[i], "byte {i}");
            }
        }
    }

    #[test]
    fn directory_matches_device() {
        let dev = fixture("0000");
        assert_eq!(parse_directory(&dev, 5), vec![true, true, false, false, false]);
        assert_eq!(directory(&[true, true, false, false, false], 255, Some(&dev)), dev);
    }
}
