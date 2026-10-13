//! Click counting from Windows Raw Input.
//!
//! Reads the same mouse events every app already receives (no device traffic),
//! on its own thread with a message-only window. Raw Input is delivered
//! alongside normal input, so it never sits in the input path the way a
//! low-level hook would.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg_attr(not(windows), allow(dead_code))]
pub const LEFT: usize = 0;
#[cfg_attr(not(windows), allow(dead_code))]
pub const RIGHT: usize = 1;
#[cfg_attr(not(windows), allow(dead_code))]
pub const MIDDLE: usize = 2;
#[cfg_attr(not(windows), allow(dead_code))]
pub const BACK: usize = 3;
#[cfg_attr(not(windows), allow(dead_code))]
pub const FORWARD: usize = 4;
pub const WHEEL: usize = 5;
pub const KINDS: usize = 6;

static PENDING: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];
static ENABLED: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
    if on && !STARTED.swap(true, Ordering::SeqCst) {
        std::thread::spawn(imp::run);
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn bump(kind: usize, n: u64) {
    if ENABLED.load(Ordering::Relaxed) {
        PENDING[kind].fetch_add(n, Ordering::Relaxed);
    }
}

/// Take everything counted since the last call.
pub fn drain() -> [u64; KINDS] {
    std::array::from_fn(|i| PENDING[i].swap(0, Ordering::Relaxed))
}

/// Lives in %APPDATA%\no-ghub-thanks\clicks.json, separate from the exe, so
/// updates never touch it. Unknown/missing fields default, so newer and older
/// versions can read each other's files.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    /// "YYYY-MM-DD" (local) -> [left, right, middle, back, forward, wheel]
    pub days: BTreeMap<String, [u64; KINDS]>,
    /// Clicks (not wheel) by local hour of day.
    pub hours: [u64; 24],
}

impl Stats {
    fn path() -> std::path::PathBuf {
        crate::config::Config::dir().join("clicks.json")
    }

    /// Load stats. A file that can't be parsed is moved aside (never
    /// overwritten); a good one is copied to clicks.backup.json first.
    pub fn load() -> Stats {
        let path = Self::path();
        let Ok(text) = std::fs::read_to_string(&path) else { return Stats::default() };
        match serde_json::from_str::<Stats>(&text) {
            Ok(s) => {
                let _ = std::fs::copy(&path, path.with_file_name("clicks.backup.json"));
                s
            }
            Err(_) => {
                let aside = path.with_file_name(format!("clicks.corrupt-{}.json", local_stamp()));
                let _ = std::fs::rename(&path, aside);
                // Fall back to the last good copy if there is one.
                std::fs::read_to_string(path.with_file_name("clicks.backup.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or_default()
            }
        }
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(crate::config::Config::dir());
        if let Ok(s) = serde_json::to_string(self) {
            let tmp = Self::path().with_extension("tmp");
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(tmp, Self::path());
            }
        }
    }

    /// Fold a drained batch in. Returns true if anything changed.
    pub fn add(&mut self, batch: [u64; KINDS]) -> bool {
        if batch.iter().all(|&n| n == 0) {
            return false;
        }
        let (day, hour) = local_now();
        let row = self.days.entry(day).or_default();
        for i in 0..KINDS {
            row[i] += batch[i];
        }
        self.hours[hour] += batch[..WHEEL].iter().sum::<u64>();
        true
    }
}

/// Local date "YYYY-MM-DD" and hour.
pub fn local_now() -> (String, usize) {
    imp::local_now()
}

/// Local "YYYY-MM-DD HH-MM-SS" (file-name safe).
pub fn local_stamp() -> String {
    imp::local_stamp()
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, SYSTEMTIME, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    use windows_sys::Win32::UI::Input::{
        GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK,
        RIM_TYPEMOUSE, RegisterRawInputDevices,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG, RI_MOUSE_BUTTON_4_DOWN,
        RI_MOUSE_BUTTON_5_DOWN, RI_MOUSE_LEFT_BUTTON_DOWN, RI_MOUSE_MIDDLE_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_DOWN,
        RI_MOUSE_WHEEL, RegisterClassW, WM_INPUT, WNDCLASSW,
    };

    pub fn local_now() -> (String, usize) {
        let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
        unsafe { GetLocalTime(&mut t) };
        (format!("{:04}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay), t.wHour as usize)
    }

    pub fn local_stamp() -> String {
        let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
        unsafe { GetLocalTime(&mut t) };
        format!(
            "{:04}-{:02}-{:02} {:02}-{:02}-{:02}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
        )
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_INPUT {
            let mut raw: RAWINPUT = unsafe { std::mem::zeroed() };
            let mut size = std::mem::size_of::<RAWINPUT>() as u32;
            let got = unsafe {
                GetRawInputData(
                    lp as HRAWINPUT,
                    RID_INPUT,
                    &mut raw as *mut _ as *mut _,
                    &mut size,
                    std::mem::size_of::<RAWINPUTHEADER>() as u32,
                )
            };
            if got != u32::MAX && raw.header.dwType == RIM_TYPEMOUSE {
                let b = unsafe { raw.data.mouse.Anonymous.Anonymous };
                let f = b.usButtonFlags as u32;
                if f & RI_MOUSE_LEFT_BUTTON_DOWN != 0 {
                    bump(LEFT, 1);
                }
                if f & RI_MOUSE_RIGHT_BUTTON_DOWN != 0 {
                    bump(RIGHT, 1);
                }
                if f & RI_MOUSE_MIDDLE_BUTTON_DOWN != 0 {
                    bump(MIDDLE, 1);
                }
                if f & RI_MOUSE_BUTTON_4_DOWN != 0 {
                    bump(BACK, 1);
                }
                if f & RI_MOUSE_BUTTON_5_DOWN != 0 {
                    bump(FORWARD, 1);
                }
                if f & RI_MOUSE_WHEEL != 0 {
                    let delta = (b.usButtonData as i16).unsigned_abs() as u64;
                    bump(WHEEL, (delta / 120).max(1));
                }
            }
        }
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    pub fn run() {
        unsafe {
            let class: Vec<u16> = "NoGHubThanksRawInput\0".encode_utf16().collect();
            let hinst = GetModuleHandleW(std::ptr::null());
            let mut wc: WNDCLASSW = std::mem::zeroed();
            wc.lpfnWndProc = Some(wndproc);
            wc.hInstance = hinst;
            wc.lpszClassName = class.as_ptr();
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return;
            }
            let dev = RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x02, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd };
            RegisterRawInputDevices(&dev, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32);
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                DispatchMessageW(&msg);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn local_now() -> (String, usize) {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let days = (secs / 86400) as i64;
        // civil-from-days (Howard Hinnant), UTC
        let z = days + 719468;
        let era = z.div_euclid(146097);
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
        (format!("{y:04}-{m:02}-{d:02}"), ((secs % 86400) / 3600) as usize)
    }
    pub fn local_stamp() -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (day, h) = local_now();
        format!("{day} {h:02}-{:02}-{:02}", (secs / 60) % 60, secs % 60)
    }
    pub fn run() {}
}
