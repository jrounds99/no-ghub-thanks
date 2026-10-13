//! Bakes the display name (what Task Manager shows) and icon into the exe.
//!
//! Customize the name with an env var at build time:
//!   PowerShell:  $env:APP_DISPLAY_NAME = "Mouse Buddy"; cargo build --release
//! or from GitHub: Actions -> build -> Run workflow -> "Display name".

const DEFAULT_NAME: &str = "No G HUB Mouse Tray";

fn main() {
    println!("cargo:rerun-if-env-changed=APP_DISPLAY_NAME");
    println!("cargo:rerun-if-changed=assets/app.ico");

    let name = std::env::var("APP_DISPLAY_NAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_NAME.to_string());
    println!("cargo:rustc-env=APP_DISPLAY_NAME={name}");

    // Commit this exe was built from; the updater compares it to the release.
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    let sha = std::env::var("GITHUB_SHA")
        .ok()
        .or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .unwrap_or_else(|| "dev".into());
    println!("cargo:rustc-env=BUILD_SHA={sha}");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico")
            .set("FileDescription", &name)
            .set("ProductName", &name)
            .set("CompanyName", "jrounds99")
            .set("LegalCopyright", "MIT License")
            .set("OriginalFilename", "no-ghub-thanks.exe");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed Windows resources: {e}");
        }
    }
}
