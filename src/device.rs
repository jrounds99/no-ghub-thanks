//! Background worker: finds the mouse, keeps lighting in the requested state
//! across reconnects/wakes/cable swaps, and reports battery status.

use crate::config::{Effect, Profile, parse_color};
use crate::hidpp::{self, Link};
use crate::onboard::{self, Descriptor};
use serde::Serialize;
use hidapi::HidApi;
use std::collections::HashSet;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// What the lighting should be.
#[derive(Debug, Clone, PartialEq)]
pub struct Light {
    pub enabled: bool,
    pub profile: Profile,
    pub brightness: u8,
    /// Battery saver: LEDs off at or below this % while discharging (0 = never).
    pub off_below: u8,
}

pub enum Cmd {
    Apply(Light),
    /// Read the mouse's onboard profiles (sends `Event::Onboard`).
    ReadOnboard,
    /// Write edited profiles + enabled flags, then make `active` current.
    WriteOnboard { profiles: Vec<onboard::Profile>, active: u8 },
    /// Write raw sectors back (backup restore): ("0001", bytes)...
    Restore(Vec<(u16, Vec<u8>)>),
    /// Report the live DPI level / active profile while a window shows them.
    WatchDpi(bool),
    Quit,
}

pub enum Event {
    Status(Option<Status>),
    /// Ok(data) or an error message.
    Onboard(Result<OnboardData, String>),
    /// Result of a write/restore: Ok(summary) or error.
    Wrote(Result<String, String>),
    /// Live position: active profile slot (1-based), DPI level index, DPI value.
    Dpi { profile: u8, index: u8, dpi: Option<u16> },
}

/// Everything the Mouse page needs, plus the raw sectors for backup.
#[derive(Debug, Clone, Serialize)]
pub struct OnboardData {
    pub descriptor: Descriptor,
    pub active: u8,
    pub profiles: Vec<onboard::Profile>,
    pub button_names: Vec<&'static str>,
    /// Hex of every user sector read (directory + profiles), for backups.
    pub raw: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Status {
    pub name: String,
    pub online: bool,
    pub wired: bool,
    pub percent: Option<u8>,
    pub charging: bool,
    pub full: bool,
}

/// RGB_EFFECTS effect ids (firmware-level, looked up per device).
const FX_DISABLED: u16 = 0x0000;
const FX_STATIC: u16 = 0x0001;
const FX_CYCLE: u16 = 0x0003;
const FX_BREATHE: u16 = 0x000A;

const BATTERY_POLL: Duration = Duration::from_secs(60);
const OFFLINE_PING: Duration = Duration::from_secs(4);
const RESCAN: Duration = Duration::from_secs(3);

struct Session {
    link: Link,
    online: bool,
    name: String,
    bat: Option<u8>,
    rgb: Option<u8>,
    obp: Option<u8>,
    /// (effect id, effect index) for cluster 0
    effects: Vec<(u16, u8)>,
    percent: Option<u8>,
    charging: bool,
    full: bool,
    last_battery: Instant,
    last_ping: Instant,
    /// What the lights were last set for: (battery bucket, battery-saver active).
    applied_sig: Option<(Option<u8>, bool)>,
    /// LED pipeline already claimed from firmware on this connection.
    claimed: bool,
    /// (profile, its 5 DPI levels) for the live DPI readout.
    dpi_cache: Option<(u8, [u16; 5])>,
}

impl Session {
    fn status(&self) -> Status {
        Status {
            name: self.name.clone(),
            online: self.online,
            wired: self.link.wired,
            percent: if self.online { self.percent } else { None },
            charging: self.charging,
            full: self.full,
        }
    }

    /// Resolve feature indices and effect table. Called on each (re)connect;
    /// cheap enough, and indices can differ between wired and wireless.
    fn discover(&mut self) -> hidpp::Result<()> {
        self.bat = self.link.feature(hidpp::FEAT_UNIFIED_BATTERY)?;
        self.rgb = self.link.feature(hidpp::FEAT_RGB_EFFECTS)?;
        self.obp = self.link.feature(hidpp::FEAT_ONBOARD_PROFILES)?;
        if self.name.is_empty() {
            self.name = self.read_name().unwrap_or_else(|| "Logitech mouse".into());
        }
        self.effects.clear();
        if let Some(rgb) = self.rgb {
            let z = self.link.request(rgb, 0, &[0x00, 0xFF, 0x00])?;
            let count = z[4];
            for e in 0..count {
                let r = self.link.request(rgb, 0, &[0x00, e, 0x00])?;
                self.effects.push((u16::from_be_bytes([r[2], r[3]]), e));
            }
        }
        Ok(())
    }

    fn read_name(&mut self) -> Option<String> {
        let f = self.link.feature(hidpp::FEAT_DEVICE_NAME).ok()??;
        let len = self.link.request(f, 0, &[]).ok()?[0] as usize;
        let mut name = Vec::new();
        while name.len() < len {
            let chunk = self.link.request(f, 1, &[name.len() as u8]).ok()?;
            let take = (len - name.len()).min(16);
            name.extend_from_slice(&chunk[..take]);
        }
        Some(String::from_utf8_lossy(&name).trim_end_matches('\0').trim().to_string())
    }

    fn read_battery(&mut self) -> hidpp::Result<()> {
        if let Some(b) = self.bat {
            let r = self.link.request(b, 1, &[])?;
            self.set_battery(&r);
        }
        self.last_battery = Instant::now();
        Ok(())
    }

    /// UNIFIED_BATTERY status: [percent, level flags, charging status, ext power]
    fn set_battery(&mut self, r: &[u8]) {
        self.percent = if r[0] > 0 { Some(r[0].min(100)) } else { level_estimate(r[1]) };
        self.charging = matches!(r[2], 1 | 2 | 4);
        self.full = r[2] == 3;
    }

    fn effect_index(&self, id: u16) -> Option<u8> {
        self.effects.iter().find(|(e, _)| *e == id).map(|(_, i)| *i)
    }

    fn apply(&mut self, light: &Light) -> hidpp::Result<()> {
        let Some(rgb) = self.rgb else { return Ok(()) };
        // Back to onboard mode so DPI/buttons come from the mouse's own profile,
        // then take the LED pipeline from firmware.
        if !self.claimed {
            if let Some(o) = self.obp {
                let _ = self.link.request(o, 1, &[0x01]);
            }
            self.link.request(rgb, 5, &[0x01, 0x03, 0x04])?;
            self.claimed = true;
        }

        let (id, params) = self.effect_params(light);
        let idx = self
            .effect_index(id)
            .or_else(|| self.effect_index(FX_STATIC))
            .or_else(|| self.effect_index(FX_DISABLED))
            .unwrap_or(0);
        let mut payload = [0u8; 13];
        payload[0] = 0; // cluster
        payload[1] = idx;
        payload[2..12].copy_from_slice(&params);
        payload[12] = 0x01;
        self.link.request(rgb, 1, &payload)?;
        self.applied_sig = Some(self.sig(light));
        Ok(())
    }

    /// Battery saver currently forcing the lights off?
    fn saver_active(&self, light: &Light) -> bool {
        light.off_below > 0 && !self.charging && self.percent.is_some_and(|p| p <= light.off_below)
    }

    /// The battery-dependent inputs to the lighting decision.
    fn sig(&self, light: &Light) -> (Option<u8>, bool) {
        let bucket = if light.profile.effect == Effect::Battery {
            battery_bucket(self.percent, self.charging)
        } else {
            None
        };
        (bucket, self.saver_active(light))
    }

    /// Battery changed in a way that changes what the LEDs should show?
    fn needs_reapply(&self, light: &Light) -> bool {
        light.enabled && self.rgb.is_some() && self.applied_sig != Some(self.sig(light))
    }

    fn effect_params(&self, light: &Light) -> (u16, [u8; 10]) {
        let mut p = [0u8; 10];
        if !light.enabled || light.profile.effect == Effect::Off || self.saver_active(light) {
            return (FX_DISABLED, p);
        }
        let scale = |c: u8| ((c as u16 * light.brightness as u16) / 100) as u8;
        let period = light.profile.period_ms.max(500).to_be_bytes();
        match light.profile.effect {
            Effect::Off => (FX_DISABLED, p),
            Effect::Static => {
                let (r, g, b) = parse_color(&light.profile.color);
                p[..3].copy_from_slice(&[scale(r), scale(g), scale(b)]);
                (FX_STATIC, p)
            }
            Effect::Breathe => {
                let (r, g, b) = parse_color(&light.profile.color);
                p[..3].copy_from_slice(&[r, g, b]);
                p[3..5].copy_from_slice(&period);
                p[5] = 0; // waveform
                p[6] = light.brightness;
                (FX_BREATHE, p)
            }
            Effect::Cycle => {
                p[5..7].copy_from_slice(&period);
                p[7] = light.brightness;
                (FX_CYCLE, p)
            }
            Effect::Battery => {
                let (r, g, b) = match battery_bucket(self.percent, self.charging) {
                    Some(3) => (0x00, 0xFF, 0x00),
                    Some(2) => (0xFF, 0xB0, 0x00),
                    Some(1) => (0xFF, 0x00, 0x00),
                    _ => (0x00, 0xFF, 0x00), // charging or unknown
                };
                p[..3].copy_from_slice(&[scale(r), scale(g), scale(b)]);
                if self.charging {
                    p[..3].copy_from_slice(&[r, g, b]);
                    p[3..5].copy_from_slice(&2000u16.to_be_bytes());
                    p[6] = light.brightness;
                    (FX_BREATHE, p)
                } else {
                    (FX_STATIC, p)
                }
            }
        }
    }

    fn obp_index(&self) -> Result<u8, String> {
        self.obp.ok_or_else(|| "this mouse has no onboard profiles".to_string())
    }

    fn descriptor(&mut self) -> Result<Descriptor, String> {
        let o = self.obp_index()?;
        let r = self.link.request(o, onboard::FN_INFO, &[]).map_err(err)?;
        Ok(Descriptor::parse(&r))
    }

    fn read_sector(&mut self, sector: u16, size: usize) -> Result<Vec<u8>, String> {
        let o = self.obp_index()?;
        let mut data = vec![0u8; size];
        let mut off = 0;
        while off < size {
            let at = off.min(size - 16);
            let p = [(sector >> 8) as u8, sector as u8, (at >> 8) as u8, at as u8];
            let r = self.link.request(o, onboard::FN_READ, &p).map_err(err)?;
            data[at..at + 16].copy_from_slice(&r[..16]);
            off += 16;
        }
        Ok(data)
    }

    /// Write a full sector, then read it back and compare.
    fn write_sector(&mut self, sector: u16, data: &[u8]) -> Result<(), String> {
        let o = self.obp_index()?;
        let n = data.len();
        let start = [(sector >> 8) as u8, sector as u8, 0, 0, (n >> 8) as u8, n as u8];
        self.link.request(o, onboard::FN_WRITE_START, &start).map_err(err)?;
        for chunk in data.chunks(16) {
            let mut c = [0xFFu8; 16];
            c[..chunk.len()].copy_from_slice(chunk);
            self.link.request(o, onboard::FN_WRITE, &c).map_err(err)?;
        }
        self.link.request(o, onboard::FN_WRITE_END, &[]).map_err(err)?;
        let back = self.read_sector(sector, n)?;
        if back != data {
            return Err(format!("sector {sector:#06x} did not verify after writing"));
        }
        Ok(())
    }

    fn active_profile(&mut self) -> Result<u8, String> {
        let o = self.obp_index()?;
        let r = self.link.request(o, onboard::FN_GET_PROFILE, &[]).map_err(err)?;
        Ok(r[1].max(1))
    }

    /// (active profile, DPI level index, that level's DPI from the profile).
    fn dpi_state(&mut self) -> Result<(u8, u8, Option<u16>), String> {
        let o = self.obp_index()?;
        let profile = self.active_profile()?;
        let index = self.link.request(o, 0xB, &[]).map_err(err)?[0];
        if self.dpi_cache.map(|(p, _)| p) != Some(profile) {
            // Levels of the active profile, read once per profile switch.
            let size = self.descriptor()?.sector_size as usize;
            let sec = self.read_sector(profile as u16, size)?;
            let levels = if onboard::crc_ok(&sec) {
                std::array::from_fn(|i| u16::from_le_bytes([sec[3 + 2 * i], sec[4 + 2 * i]]))
            } else {
                [0; 5]
            };
            self.dpi_cache = Some((profile, levels));
        }
        let dpi = self.dpi_cache.and_then(|(_, l)| l.get(index as usize).copied()).filter(|&d| d > 0);
        Ok((profile, index, dpi))
    }

    fn set_active_profile(&mut self, slot: u8) -> Result<(), String> {
        let o = self.obp_index()?;
        self.link.request(o, onboard::FN_SET_PROFILE, &[0x00, slot]).map_err(err)?;
        Ok(())
    }

    fn read_onboard(&mut self) -> Result<OnboardData, String> {
        let d = self.descriptor()?;
        if d.profile_format != 0x05 {
            return Err(format!(
                "onboard profile format {:#04x} isn't supported yet (built for the G502 X family)",
                d.profile_format
            ));
        }
        let size = d.sector_size as usize;
        let count = d.profile_count as usize;
        let mut raw = std::collections::BTreeMap::new();
        let dir = self.read_sector(0x0000, size)?;
        let enabled = if onboard::crc_ok(&dir) {
            onboard::parse_directory(&dir, count)
        } else {
            vec![true; count.min(1)].into_iter().chain(std::iter::repeat(false)).take(count).collect()
        };
        raw.insert("0000".into(), onboard::hex(&dir));
        let mut profiles = Vec::new();
        for i in 0..count {
            let slot = i as u8 + 1;
            let user = self.read_sector(slot as u16, size)?;
            raw.insert(format!("{:04X}", slot), onboard::hex(&user));
            let (sector, from_rom) = if onboard::crc_ok(&user) {
                (user, false)
            } else {
                // Unwritten user slot: show the factory profile it would fall back to.
                let rom = 0x0100 + (i.min(d.rom_profile_count.saturating_sub(1) as usize) as u16) + 1;
                (self.read_sector(rom, size)?, true)
            };
            profiles.push(onboard::Profile::parse(slot, enabled[i], &sector, from_rom));
        }
        let active = self.active_profile().unwrap_or(1);
        Ok(OnboardData {
            descriptor: d,
            active,
            profiles,
            button_names: onboard::G502X_BUTTONS.to_vec(),
            raw,
        })
    }

    fn write_onboard(&mut self, profiles: &[onboard::Profile], active: u8, light: &Light) -> Result<String, String> {
        let current = self.read_onboard()?;
        // Always keep a backup of what was on the mouse before this write.
        backup(&current)?;
        let size = current.descriptor.sector_size as usize;
        let mut written = 0;
        for p in profiles {
            let key = format!("{:04X}", p.slot);
            let base = onboard::unhex(current.raw.get(&key).ok_or("unknown profile slot")?).unwrap_or_default();
            // A slot that was never written starts from its factory profile.
            let base = if onboard::crc_ok(&base) {
                base
            } else {
                let rom = 0x0101 + (p.slot as u16 - 1).min(current.descriptor.rom_profile_count.saturating_sub(1) as u16);
                self.read_sector(rom, size)?
            };
            let new = p.apply_to(&base)?;
            if new != base {
                self.write_sector(p.slot as u16, &new)?;
                written += 1;
            }
        }
        let mut enabled: Vec<bool> = current.profiles.iter().map(|p| p.enabled).collect();
        for p in profiles {
            if let Some(e) = enabled.get_mut(p.slot as usize - 1) {
                *e = p.enabled;
            }
        }
        if !enabled.iter().any(|&e| e) {
            enabled[0] = true;
        }
        let dir_base = onboard::unhex(&current.raw["0000"]).unwrap_or_default();
        let dir = onboard::directory(&enabled, size, onboard::crc_ok(&dir_base).then_some(dir_base.as_slice()));
        if onboard::hex(&dir) != current.raw["0000"] {
            self.write_sector(0x0000, &dir)?;
            written += 1;
        }
        let active = if enabled.get(active as usize - 1).copied().unwrap_or(false) { active } else { 1 };
        self.set_active_profile(active)?;
        // Reloading a profile hands the LEDs back to firmware: take them again.
        self.claimed = false;
        self.dpi_cache = None;
        let _ = self.apply(light);
        Ok(if written == 0 { "Nothing changed".into() } else { format!("Saved to the mouse ({written} sector{})", if written == 1 { "" } else { "s" }) })
    }

    fn restore(&mut self, sectors: &[(u16, Vec<u8>)], light: &Light) -> Result<String, String> {
        let current = self.read_onboard()?;
        backup(&current)?;
        let size = current.descriptor.sector_size as usize;
        for (sec, data) in sectors {
            if data.len() != size || !onboard::crc_ok(data) || !(*sec <= current.descriptor.profile_count as u16) {
                return Err(format!("backup sector {sec:#06x} is not valid for this mouse"));
            }
        }
        for (sec, data) in sectors {
            self.write_sector(*sec, data)?;
        }
        let _ = self.set_active_profile(1);
        self.claimed = false;
        let _ = self.apply(light);
        Ok(format!("Restored {} sectors", sectors.len()))
    }

    /// Device came (back) online: rediscover, enforce lighting, read battery.
    fn bring_online(&mut self, light: &Light) -> hidpp::Result<()> {
        self.claimed = false;
        self.discover()?;
        self.read_battery()?;
        self.apply(light)?;
        self.online = true;
        self.last_ping = Instant::now();
        Ok(())
    }
}

fn err(e: hidpp::Error) -> String {
    match e {
        hidpp::Error::Timeout => "the mouse didn't answer (asleep?)".into(),
        hidpp::Error::Device(c) => format!("the mouse rejected the request (error {c:#04x})"),
        hidpp::Error::Io(s) => s,
    }
}

/// Save the raw onboard sectors to %APPDATA%\no-ghub-thanks\backups.
pub fn backup(d: &OnboardData) -> Result<std::path::PathBuf, String> {
    let dir = crate::config::Config::dir().join("backups");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("onboard {}.json", crate::clicks::local_stamp()));
    let body = serde_json::json!({ "descriptor": d.descriptor, "sectors": d.raw });
    std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap()).map_err(|e| e.to_string())?;
    Ok(path)
}

/// 3 = high, 2 = medium, 1 = low, 0 = charging, None = unknown.
fn battery_bucket(percent: Option<u8>, charging: bool) -> Option<u8> {
    if charging {
        return Some(0);
    }
    percent.map(|p| match p {
        50.. => 3,
        20.. => 2,
        _ => 1,
    })
}

fn level_estimate(flags: u8) -> Option<u8> {
    match flags {
        f if f & 0x08 != 0 => Some(90),
        f if f & 0x04 != 0 => Some(50),
        f if f & 0x02 != 0 => Some(20),
        f if f & 0x01 != 0 => Some(5),
        _ => None,
    }
}

/// Try to attach to a mouse. Wired devices are tried before receivers.
fn connect(api: &HidApi, skip: &mut HashSet<u16>, light: &Light) -> Option<Session> {
    for pid in hidpp::candidates(api) {
        if skip.contains(&pid) {
            continue;
        }
        let receiver = hidpp::RECEIVER_PIDS.contains(&pid);
        let indices: Vec<u8> = if receiver { (1..=6).collect() } else { vec![0xFF] };
        for &idx in &indices {
            let Ok(link) = Link::open(api, pid, idx) else { continue };
            let mut s = new_session(link);
            if s.link.ping().is_err() {
                continue;
            }
            match s.bring_online(light) {
                Ok(()) if s.rgb.is_some() || s.bat.is_some() => return Some(s),
                _ => continue,
            }
        }
        if receiver {
            // Receiver present but mouse asleep/off: hold the receiver and
            // wait for its link notification.
            if let Ok(link) = Link::open(api, pid, 1) {
                return Some(new_session(link));
            }
        } else {
            // Some other Logitech HID++ device (keyboard, headset, webcam...)
            skip.insert(pid);
        }
    }
    None
}

fn new_session(link: Link) -> Session {
    Session {
        link,
        online: false,
        name: String::new(),
        bat: None,
        rgb: None,
        obp: None,
        effects: Vec::new(),
        percent: None,
        charging: false,
        full: false,
        last_battery: Instant::now(),
        last_ping: Instant::now(),
        applied_sig: None,
        claimed: false,
        dpi_cache: None,
    }
}

/// Run the worker on the current thread until `Cmd::Quit`.
pub fn run(rx: Receiver<Cmd>, mut light: Light, emit: impl Fn(Event)) {
    let mut api = loop {
        match HidApi::new() {
            Ok(a) => break a,
            Err(_) => std::thread::sleep(Duration::from_secs(2)),
        }
    };
    let mut session: Option<Session> = None;
    let mut skip = HashSet::new();
    let mut last_scan = Instant::now() - RESCAN;
    let mut last_emit: Option<Option<Status>> = None;
    let emit = &emit;
    let mut report = |s: Option<Status>| {
        if last_emit.as_ref() != Some(&s) {
            emit(Event::Status(s.clone()));
            last_emit = Some(s);
        }
    };
    let offline = || "the mouse isn't connected right now".to_string();
    let mut watch_dpi = false;
    let mut last_dpi_poll = Instant::now();
    let mut last_dpi: Option<(u8, u8, Option<u16>)> = None;

    loop {
        // Commands from the tray.
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Cmd::Quit => return,
                Cmd::Apply(l) => {
                    light = l;
                    if let Some(s) = session.as_mut().filter(|s| s.online) {
                        let _ = s.apply(&light);
                    }
                }
                Cmd::ReadOnboard => {
                    let r = match session.as_mut().filter(|s| s.online) {
                        Some(s) => s.read_onboard(),
                        None => Err(offline()),
                    };
                    emit(Event::Onboard(r));
                }
                Cmd::WriteOnboard { profiles, active } => {
                    let r = match session.as_mut().filter(|s| s.online) {
                        Some(s) => s.write_onboard(&profiles, active, &light),
                        None => Err(offline()),
                    };
                    emit(Event::Wrote(r));
                    if let Some(s) = session.as_mut().filter(|s| s.online) {
                        emit(Event::Onboard(s.read_onboard()));
                    }
                }
                Cmd::WatchDpi(on) => {
                    watch_dpi = on;
                    last_dpi = None; // re-send current state to the new viewer
                }
                Cmd::Restore(sectors) => {
                    let r = match session.as_mut().filter(|s| s.online) {
                        Some(s) => s.restore(&sectors, &light),
                        None => Err(offline()),
                    };
                    emit(Event::Wrote(r));
                    if let Some(s) = session.as_mut().filter(|s| s.online) {
                        emit(Event::Onboard(s.read_onboard()));
                    }
                }
            }
        }

        let Some(s) = session.as_mut() else {
            if last_scan.elapsed() >= RESCAN {
                last_scan = Instant::now();
                let _ = api.refresh_devices();
                session = connect(&api, &mut skip, &light);
                report(session.as_ref().map(Session::status));
            } else {
                std::thread::sleep(Duration::from_millis(200));
            }
            continue;
        };

        // Cable plugged in while on the receiver: the wireless link drops and
        // a wired HID++ device appears. Switch to it.
        if !s.link.wired && !s.online && last_scan.elapsed() >= RESCAN {
            last_scan = Instant::now();
            let _ = api.refresh_devices();
            let wired_present = hidpp::candidates(&api)
                .iter()
                .any(|p| !hidpp::RECEIVER_PIDS.contains(p) && !skip.contains(p));
            if wired_present {
                session = None;
                continue;
            }
        }

        let notices = match s.link.poll(250) {
            Ok(n) => n,
            Err(_) => {
                // Device unplugged (wired) or receiver removed.
                session = None;
                last_scan = Instant::now() - RESCAN;
                report(None);
                continue;
            }
        };

        for n in notices {
            if n.len() < 7 {
                continue;
            }
            // Receiver device-connection notification (HID++ 1.0, sub id 0x41).
            if n[2] == 0x41 && (1..=6).contains(&n[1]) {
                let link_up = n[4] & 0x40 == 0;
                if link_up {
                    s.link.index = n[1];
                    std::thread::sleep(Duration::from_millis(300));
                    if s.bring_online(&light).is_err() {
                        s.online = false;
                    }
                } else {
                    s.online = false;
                }
                continue;
            }
            // Battery change event (function 0, swid 0) from UNIFIED_BATTERY.
            if s.online && Some(n[2]) == s.bat && n[3] == 0x00 && n[1] == s.link.index {
                s.set_battery(&n[4..]);
                if s.needs_reapply(&light) {
                    let _ = s.apply(&light);
                }
            }
        }

        if s.online {
            if s.last_battery.elapsed() >= BATTERY_POLL && s.read_battery().is_err() {
                s.online = false;
            } else if s.needs_reapply(&light) {
                let _ = s.apply(&light);
            }
        } else if !s.link.wired && s.last_ping.elapsed() >= OFFLINE_PING {
            // Missed the link notification (or mouse was asleep at start):
            // probe each slot.
            s.last_ping = Instant::now();
            for idx in 1..=6 {
                s.link.index = idx;
                if s.link.ping().is_ok() {
                    if s.bring_online(&light).is_err() {
                        s.online = false;
                    }
                    break;
                }
            }
            if !s.online {
                s.link.index = 1;
            }
        }

        // Live DPI readout: cheap (two short requests) and only while watched.
        if watch_dpi && s.online && s.obp.is_some() && last_dpi_poll.elapsed() >= Duration::from_millis(350) {
            last_dpi_poll = Instant::now();
            if let Ok(st) = s.dpi_state() {
                if last_dpi != Some(st) {
                    last_dpi = Some(st);
                    emit(Event::Dpi { profile: st.0, index: st.1, dpi: st.2 });
                }
            }
        }

        report(Some(s.status()));
    }
}
