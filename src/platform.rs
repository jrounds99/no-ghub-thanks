//! Windows-only integration: autostart, toasts, LampArray service check.
//! Non-Windows builds get no-op stubs so the core can be type-checked anywhere.

#[cfg(windows)]
mod imp {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "NoGHubThanks";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub fn autostart_enabled() -> bool {
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(RUN_KEY, KEY_READ)
            .and_then(|k| k.get_value::<String, _>(VALUE))
            .is_ok()
    }

    pub fn set_autostart(on: bool) {
        let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_WRITE) else {
            return;
        };
        if on {
            if let Ok(exe) = std::env::current_exe() {
                let _ = key.set_value(VALUE, &format!("\"{}\"", exe.display()));
            }
        } else {
            let _ = key.delete_value(VALUE);
        }
    }

    pub fn toast(title: &str, body: &str) {
        let _ = notify_rust::Notification::new().summary(title).body(body).show();
    }

    /// Logitech's Windows Dynamic Lighting bridge also drives the LEDs and
    /// will fight us. It ships with the device driver, not G HUB.
    pub fn lamparray_running() -> bool {
        Command::new("sc")
            .args(["query", "logi_lamparray_service"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("RUNNING"))
            .unwrap_or(false)
    }

    pub fn open_in_editor(path: &std::path::Path) {
        let _ = Command::new("notepad").arg(path).spawn();
    }

    fn dword(path: &str, name: &str) -> Option<u32> {
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(path, KEY_READ)
            .and_then(|k| k.get_value::<u32, _>(name))
            .ok()
    }

    /// Taskbar / system theme (not app theme): drives tray icon contrast.
    pub fn taskbar_light() -> bool {
        dword(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize", "SystemUsesLightTheme") == Some(1)
    }

    /// App theme (light/dark mode for apps).
    pub fn apps_light() -> bool {
        dword(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize", "AppsUseLightTheme") != Some(0)
    }

    /// Windows accent color as "#RRGGBB".
    pub fn accent_color() -> String {
        match dword(r"Software\Microsoft\Windows\DWM", "AccentColor") {
            // stored as 0xAABBGGRR
            Some(v) => format!("#{:02X}{:02X}{:02X}", v & 0xFF, (v >> 8) & 0xFF, (v >> 16) & 0xFF),
            None => "#3B82F6".into(),
        }
    }

    /// Win11 rounded corners on a borderless window.
    pub fn round_corners(hwnd: isize) {
        use windows_sys::Win32::Graphics::Dwm::{DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute};
        let pref = DWMWCP_ROUND;
        unsafe {
            DwmSetWindowAttribute(
                hwnd as _,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                &pref as *const _ as *const _,
                std::mem::size_of_val(&pref) as u32,
            );
        }
    }

    /// Windows 11 keeps each tray icon's "show in taskbar" toggle under
    /// HKCU\Control Panel\NotifyIconSettings\<id> (ExecutablePath, IsPromoted).
    /// The entry appears after the icon has been shown once. Returns whether
    /// a matching entry was found.
    pub fn set_tray_promoted(on: bool) -> bool {
        use winreg::enums::KEY_ALL_ACCESS;
        let Ok(exe) = std::env::current_exe() else { return false };
        let Some(file) = exe.file_name().map(|f| f.to_string_lossy().to_lowercase()) else { return false };
        let Ok(root) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(r"Control Panel\NotifyIconSettings") else {
            return false;
        };
        let mut found = false;
        for name in root.enum_keys().flatten() {
            let Ok(k) = root.open_subkey_with_flags(&name, KEY_ALL_ACCESS) else { continue };
            let path: String = k.get_value("ExecutablePath").unwrap_or_default();
            if path.to_lowercase().ends_with(&file) {
                let _ = k.set_value("IsPromoted", &(on as u32));
                found = true;
            }
        }
        found
    }

    /// Returns false if another instance already holds the mutex.
    pub fn single_instance() -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let name: Vec<u16> = "Local\\NoGHubThanks\0".encode_utf16().collect();
        unsafe {
            let h = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
            if h.is_null() {
                return false;
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                // Don't keep the other instance's mutex alive (matters when
                // waiting for the old exe to exit after an update).
                CloseHandle(h);
                return false;
            }
            true // handle intentionally held for the process lifetime
        }
    }
}

#[cfg(not(windows))]
#[allow(dead_code)]
mod imp {
    pub fn autostart_enabled() -> bool {
        false
    }
    pub fn set_autostart(_on: bool) {}
    pub fn toast(title: &str, body: &str) {
        eprintln!("{title}: {body}");
    }
    pub fn lamparray_running() -> bool {
        false
    }
    pub fn open_in_editor(path: &std::path::Path) {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
    pub fn taskbar_light() -> bool {
        false
    }
    pub fn accent_color() -> String {
        "#3B82F6".into()
    }
    pub fn apps_light() -> bool {
        true
    }
    pub fn round_corners(_hwnd: isize) {}
    pub fn single_instance() -> bool {
        true
    }
    pub fn set_tray_promoted(_on: bool) -> bool {
        false
    }
}

pub use imp::*;

/// Admin one-liner the user can paste to disable the LampArray service.
pub const LAMPARRAY_FIX: &str = "Start-Process powershell -Verb RunAs -ArgumentList '-NoProfile -Command \"Get-Service logi_lamparray* | Stop-Service -Force -PassThru | Set-Service -StartupType Disabled\"'";
