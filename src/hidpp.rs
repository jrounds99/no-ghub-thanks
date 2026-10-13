//! Minimal HID++ 2.0 transport over hidapi.
//!
//! On Windows the vendor interface (usage page 0xFF00) is split into separate
//! top-level collections: usage 0x0001 = short reports (0x10, 7 bytes),
//! usage 0x0002 = long reports (0x11, 20 bytes). Feature requests and replies
//! use the long collection; receiver link notifications (0x41) arrive on the
//! short one.

use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

pub const LOGITECH_VID: u16 = 0x046D;
const SWID: u8 = 0x0A;
const LONG: u8 = 0x11;

/// Known LIGHTSPEED / Unifying / Bolt receiver product ids. Anything else
/// with an HID++ collection is treated as a directly connected device (0xFF).
pub const RECEIVER_PIDS: &[u16] = &[
    0xC547, 0xC539, 0xC53A, 0xC53D, 0xC53F, 0xC541, 0xC545, 0xC548, 0xC52B, 0xC532, 0xC534,
];

pub const FEAT_ROOT: u16 = 0x0000;
pub const FEAT_DEVICE_NAME: u16 = 0x0005;
pub const FEAT_UNIFIED_BATTERY: u16 = 0x1004;
pub const FEAT_RGB_EFFECTS: u16 = 0x8071;
pub const FEAT_ONBOARD_PROFILES: u16 = 0x8100;

#[derive(Debug)]
#[allow(dead_code)] // payloads are for Debug output
pub enum Error {
    Timeout,
    Device(u8),
    Io(String),
}

impl From<hidapi::HidError> for Error {
    fn from(e: hidapi::HidError) -> Self {
        Error::Io(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Link {
    long: HidDevice,
    short: Option<HidDevice>,
    pub index: u8,
    pub wired: bool,
    pending: Vec<Vec<u8>>,
}

/// All Logitech PIDs with an HID++ long collection, receivers last.
pub fn candidates(api: &HidApi) -> Vec<u16> {
    let mut v: Vec<u16> = api
        .device_list()
        .filter(|d| d.vendor_id() == LOGITECH_VID && d.usage_page() == 0xFF00 && d.usage() == 0x0002)
        .map(|d| d.product_id())
        .collect();
    v.sort_by_key(|p| (RECEIVER_PIDS.contains(p), *p));
    v.dedup();
    v
}

impl Link {
    pub fn open(api: &HidApi, pid: u16, index: u8) -> Result<Link> {
        let mut long = None;
        let mut short = None;
        for d in api.device_list() {
            if d.vendor_id() != LOGITECH_VID || d.product_id() != pid || d.usage_page() != 0xFF00 {
                continue;
            }
            match d.usage() {
                0x0002 if long.is_none() => long = Some(d.open_device(api)?),
                0x0001 if short.is_none() => short = d.open_device(api).ok(),
                _ => {}
            }
        }
        let long = long.ok_or(Error::Io("no HID++ long collection".into()))?;
        Ok(Link { long, short, index, wired: index == 0xFF, pending: Vec::new() })
    }

    /// Send a feature request and wait for its reply. Unrelated reports that
    /// arrive meanwhile are queued and handed out by `poll`.
    pub fn request(&mut self, feat_idx: u8, func: u8, params: &[u8]) -> Result<Vec<u8>> {
        let mut pkt = [0u8; 20];
        pkt[0] = LONG;
        pkt[1] = self.index;
        pkt[2] = feat_idx;
        pkt[3] = (func << 4) | SWID;
        pkt[4..4 + params.len()].copy_from_slice(params);
        self.long.write(&pkt)?;

        let fn_sw = (func << 4) | SWID;
        let deadline = Instant::now() + Duration::from_millis(1200);
        let mut buf = [0u8; 64];
        while Instant::now() < deadline {
            let n = self.long.read_timeout(&mut buf, 100)?;
            if n < 6 {
                continue;
            }
            let r = &buf[..n];
            if r[1] == self.index && r[2] == 0xFF && r[3] == feat_idx && r[4] == fn_sw {
                return Err(Error::Device(r[5]));
            }
            if r[1] == self.index && r[2] == feat_idx && r[3] == fn_sw {
                return Ok(r[4..].to_vec());
            }
            // HID++ 1.0 error for this device (e.g. receiver says "unknown device")
            if r[1] == self.index && r[2] == 0x8F {
                return Err(Error::Device(r[5]));
            }
            self.pending.push(r.to_vec());
        }
        Err(Error::Timeout)
    }

    /// Look up a feature's index in the device's feature table.
    pub fn feature(&mut self, id: u16) -> Result<Option<u8>> {
        let r = self.request(0, 0, &[(id >> 8) as u8, id as u8])?;
        Ok(if r[0] == 0 && id != FEAT_ROOT { None } else { Some(r[0]) })
    }

    /// ROOT ping. Cheap liveness check.
    pub fn ping(&mut self) -> Result<()> {
        self.request(0, 1, &[0, 0, 0x5A]).map(|_| ())
    }

    /// Collect unsolicited reports from both collections. Blocks up to `wait_ms`.
    /// Returns Err only when the device is gone (unplugged).
    pub fn poll(&mut self, wait_ms: i32) -> Result<Vec<Vec<u8>>> {
        let mut out = std::mem::take(&mut self.pending);
        let mut buf = [0u8; 64];
        if let Some(s) = self.short.as_ref() {
            while let Ok(n) = s.read_timeout(&mut buf, 0) {
                if n == 0 {
                    break;
                }
                out.push(buf[..n].to_vec());
            }
        }
        let n = self.long.read_timeout(&mut buf, if out.is_empty() { wait_ms } else { 0 })?;
        if n > 0 {
            out.push(buf[..n].to_vec());
        }
        Ok(out)
    }
}
