//! Background worker: finds the mouse, keeps lighting in the requested state
//! across reconnects/wakes/cable swaps, and reports battery status.

use crate::config::{Effect, Profile, parse_color};
use crate::hidpp::{self, Link};
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
}

pub enum Cmd {
    Apply(Light),
    Quit,
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
    applied_bucket: Option<u8>,
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
        if let Some(o) = self.obp {
            let _ = self.link.request(o, 1, &[0x01]);
        }
        self.link.request(rgb, 5, &[0x01, 0x03, 0x04])?;

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
        self.applied_bucket = battery_bucket(self.percent, self.charging);
        Ok(())
    }

    fn effect_params(&self, light: &Light) -> (u16, [u8; 10]) {
        let mut p = [0u8; 10];
        if !light.enabled || light.profile.effect == Effect::Off {
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

    /// Device came (back) online: rediscover, enforce lighting, read battery.
    fn bring_online(&mut self, light: &Light) -> hidpp::Result<()> {
        self.discover()?;
        self.read_battery()?;
        self.apply(light)?;
        self.online = true;
        self.last_ping = Instant::now();
        Ok(())
    }
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
        applied_bucket: None,
    }
}

/// Run the worker on the current thread until `Cmd::Quit`.
pub fn run(rx: Receiver<Cmd>, mut light: Light, emit: impl Fn(Option<Status>)) {
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
    let mut report = |s: Option<Status>| {
        if last_emit.as_ref() != Some(&s) {
            emit(s.clone());
            last_emit = Some(s);
        }
    };

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
                if light.enabled
                    && light.profile.effect == Effect::Battery
                    && s.applied_bucket != battery_bucket(s.percent, s.charging)
                {
                    let _ = s.apply(&light);
                }
            }
        }

        if s.online {
            if s.last_battery.elapsed() >= BATTERY_POLL && s.read_battery().is_err() {
                s.online = false;
            } else if light.enabled
                && light.profile.effect == Effect::Battery
                && s.applied_bucket != battery_bucket(s.percent, s.charging)
            {
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

        report(Some(s.status()));
    }
}
