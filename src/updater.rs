//! Self-update from the repo's newest versioned GitHub release (vX.Y.Z).
//! The rolling `latest` pre-release is for testing and is never offered.
//!
//! Uses Windows' built-in curl.exe (Windows 10 1803+), so no TLS stack ships
//! in the exe. The downloaded exe is verified against the release's .sha256
//! asset, then swapped in place: a running exe can be renamed on Windows, so
//! the old one moves aside, the new one takes its path, and it's relaunched.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const REPO: &str = "jrounds99/no-ghub-thanks";
const ASSET: &str = "no-ghub-thanks.exe";
pub const BUILD_SHA: &str = env!("BUILD_SHA");
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// "v1.2.3" / "1.2.3" -> (1, 2, 3). Missing parts are 0.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core = s.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u32>());
    let major = it.next()?.ok()?;
    let minor = it.next().unwrap_or(Ok(0)).ok()?;
    let patch = it.next().unwrap_or(Ok(0)).ok()?;
    Some((major, minor, patch))
}

pub fn build_short() -> String {
    BUILD_SHA.chars().take(7).collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct Available {
    pub version: String,
    pub exe_url: String,
    pub hash_url: Option<String>,
}

fn curl(args: &[&str]) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new(if cfg!(windows) { "curl.exe" } else { "curl" });
    cmd.args(["-sfL", "--max-time", "120", "-H", "User-Agent: no-ghub-thanks"]).args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().map_err(|e| format!("curl: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!("download failed (curl exit {})", out.status.code().unwrap_or(-1)))
    }
}

/// Ask GitHub for the newest version. Ok(None) = already up to date.
pub fn check() -> Result<Option<Available>, String> {
    // /releases/latest skips pre-releases, i.e. the rolling test build.
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = curl(&["-H", "Accept: application/vnd.github+json", &url])?;
    let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| format!("bad response: {e}"))?;
    let tag = v["tag_name"].as_str().unwrap_or_default();
    let (Some(remote), Some(local)) = (parse_version(tag), parse_version(VERSION)) else {
        return Err(format!("unrecognized release version '{tag}'"));
    };
    if remote <= local {
        return Ok(None);
    }
    let assets = v["assets"].as_array().cloned().unwrap_or_default();
    let url_of = |name: &str| {
        assets
            .iter()
            .find(|a| a["name"].as_str() == Some(name))
            .and_then(|a| a["browser_download_url"].as_str())
            .map(String::from)
    };
    let exe_url = url_of(ASSET).ok_or("release has no exe")?;
    Ok(Some(Available {
        version: format!("{}.{}.{}", remote.0, remote.1, remote.2),
        exe_url,
        hash_url: url_of(&format!("{ASSET}.sha256")),
    }))
}

fn sha256_hex(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    Ok(Sha256::digest(&bytes).iter().map(|b| format!("{b:02X}")).collect())
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let stem = exe.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "no-ghub-thanks".into());
    exe.with_file_name(format!("{stem}.{suffix}.exe"))
}

/// Download, verify and swap in the new exe, then launch it. On Ok the
/// caller should exit promptly so the new instance can take over.
pub fn install(a: &Available) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let new = sibling(&exe, "new");
    let old = sibling(&exe, "old");
    let _ = std::fs::remove_file(&new);

    curl(&["-o", &new.to_string_lossy(), &a.exe_url])?;
    if let Some(h) = &a.hash_url {
        let expected = String::from_utf8_lossy(&curl(&[h])?).trim().to_uppercase();
        let actual = sha256_hex(&new)?;
        if expected.len() == 64 && expected != actual {
            let _ = std::fs::remove_file(&new);
            return Err("checksum mismatch, update discarded".into());
        }
    }

    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("could not move current exe aside: {e}"))?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe); // put it back
        return Err(format!("could not place new exe: {e}"));
    }
    Command::new(&exe).arg("--updated").spawn().map_err(|e| format!("could not start new version: {e}"))?;
    Ok(())
}

/// Remove the previous exe left behind by an update (it's unlocked once that
/// process has exited).
pub fn cleanup_old() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(sibling(&exe, "old"));
        let _ = std::fs::remove_file(sibling(&exe, "new"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions_compare() {
        assert_eq!(parse_version("v1.0.0"), Some((1, 0, 0)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("v2.0.1-beta"), Some((2, 0, 1)));
        assert!(parse_version("v1.0.10") > parse_version("v1.0.9"));
        assert!(parse_version("latest").is_none());
    }
}
