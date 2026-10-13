#![cfg_attr(windows, windows_subsystem = "windows")]
//! No G HUB Mouse Tray: battery % in the tray, lighting control and a click
//! counter for Logitech HID++ 2.0 mice, without G HUB.

mod clicks;
mod config;
mod device;
mod hidpp;
mod icon;
mod onboard;
mod platform;
mod updater;

use config::Config;
use device::{Cmd, Light, Status};
use serde::Deserialize;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};
use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, Rect, TrayIcon, TrayIconBuilder, TrayIconEvent};
use wry::WebView;

/// Set at build time (see build.rs). Also what Task Manager shows.
const APP_NAME: &str = env!("APP_DISPLAY_NAME");

enum UserEvent {
    Device(device::Event),
    Mouse(String),
    Tray(TrayIconEvent),
    Menu(MenuEvent),
    Ui(String),
    Stats(String),
    Update(UpdateEv),
}

enum UpdateEv {
    /// (result, user asked)
    Checked(Result<Option<updater::Available>, String>, bool),
    Installed(Result<(), String>),
}

struct UpdateState {
    status: &'static str, // idle | checking | uptodate | available | installing | error
    latest: Option<updater::Available>,
    error: String,
    notified: String,
}

#[derive(Deserialize)]
struct UiMsg {
    cmd: String,
    #[serde(default)]
    on: bool,
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: f64,
    #[serde(default)]
    effect: String,
    #[serde(default)]
    color: String,
    #[serde(default)]
    period: f64,
    #[serde(default)]
    commit: bool,
    #[serde(default)]
    profiles: Vec<onboard::Profile>,
}

const WIDTH: f64 = 340.0;
const TICK: Duration = Duration::from_secs(5);
const STATS_SAVE: Duration = Duration::from_secs(15);
const LAMPARRAY_CHECK: Duration = Duration::from_secs(60);
const FIRST_UPDATE_CHECK: Duration = Duration::from_secs(60);
const UPDATE_INTERVAL: Duration = Duration::from_secs(24 * 3600);
const PIN_ATTEMPTS: u32 = 24; // ~2 minutes of 5 s ticks

fn light_of(cfg: &Config) -> Light {
    Light {
        enabled: cfg.lighting_enabled,
        profile: cfg.active(),
        brightness: cfg.brightness,
        off_below: cfg.lights_off_below,
    }
}

fn config_mtime() -> Option<SystemTime> {
    std::fs::metadata(Config::path()).and_then(|m| m.modified()).ok()
}

fn tooltip(status: Option<&Status>) -> String {
    format!("{}\nv{}", tooltip_status(status), updater::VERSION)
}

fn tooltip_status(status: Option<&Status>) -> String {
    match status {
        None => format!("{APP_NAME}: no Logitech mouse found"),
        Some(s) if !s.online => format!("{} — asleep", s.name),
        Some(s) => {
            let pct = s.percent.map(|p| format!("{p}%")).unwrap_or_else(|| "?".into());
            let state = if s.charging {
                ", charging"
            } else if s.full {
                ", full"
            } else {
                ""
            };
            let link = if s.wired { "wired" } else { "wireless" };
            format!("{} — {pct}{state} ({link})", s.name)
        }
    }
}

fn click_totals(stats: &clicks::Stats) -> (u64, u64) {
    let today = clicks::local_now().0;
    let sum = |r: &[u64; clicks::KINDS]| r[..clicks::WHEEL].iter().sum::<u64>();
    let total = stats.days.values().map(sum).sum();
    let t = stats.days.get(&today).map(sum).unwrap_or(0);
    (t, total)
}

fn ui_state(
    cfg: &Config,
    status: Option<&Status>,
    lamparray: bool,
    stats: &clicks::Stats,
    upd: &UpdateState,
    live: Option<(u8, Option<u16>)>,
) -> String {
    let (today, total) = click_totals(stats);
    let saver_active = cfg.lighting_enabled
        && cfg.lights_off_below > 0
        && status.is_some_and(|s| s.online && !s.charging && s.percent.is_some_and(|p| p <= cfg.lights_off_below));
    serde_json::json!({
        "found": status.is_some(),
        "name": status.map(|s| s.name.as_str()).unwrap_or(""),
        "online": status.is_some_and(|s| s.online),
        "wired": status.is_some_and(|s| s.wired),
        "percent": status.and_then(|s| s.percent),
        "charging": status.is_some_and(|s| s.charging),
        "full": status.is_some_and(|s| s.full),
        "enabled": cfg.lighting_enabled,
        "active": cfg.active().name,
        "profiles": cfg.profiles,
        "brightness": cfg.brightness,
        "autostart": platform::autostart_enabled(),
        "lamparray": lamparray,
        "accent": platform::accent_color(),
        "fix": platform::LAMPARRAY_FIX,
        "clicks": { "enabled": cfg.count_clicks, "today": today, "total": total },
        "off_below": cfg.lights_off_below,
        "saver_active": saver_active,
        "low_alert": cfg.low_battery_alert,
        "pin": cfg.always_show_in_tray,
        "auto_update": cfg.auto_update,
        "tray_icon": cfg.tray_icon,
        "dpi": live.and_then(|l| l.1),
        "live_profile": live.map(|l| l.0),
        "update": {
            "current": updater::VERSION,
            "build": updater::build_short(),
            "status": upd.status,
            "latest": upd.latest.as_ref().map(|a| a.version.clone()),
            "error": upd.error,
        },
    })
    .to_string()
}

fn stats_state(cfg: &Config, stats: &clicks::Stats) -> String {
    serde_json::json!({
        "days": stats.days,
        "hours": stats.hours,
        "today": clicks::local_now().0,
        "enabled": cfg.count_clicks,
        "accent": platform::accent_color(),
    })
    .to_string()
}

/// Tray icon image for the current style.
fn tray_image(cfg: &Config, status: Option<&Status>, light_taskbar: bool) -> Option<Icon> {
    match cfg.tray_icon {
        config::TrayIcon::App => Icon::from_rgba(include_bytes!("../assets/app64.rgba").to_vec(), 64, 64).ok(),
        config::TrayIcon::Battery => {
            Icon::from_rgba(icon::render(status, light_taskbar), icon::ICON_SIZE, icon::ICON_SIZE).ok()
        }
    }
}

fn app_icon() -> Option<tao::window::Icon> {
    tao::window::Icon::from_rgba(include_bytes!("../assets/app64.rgba").to_vec(), 64, 64).ok()
}

fn webview_bg() -> (u8, u8, u8, u8) {
    if platform::apps_light() { (243, 243, 243, 255) } else { (28, 28, 28, 255) }
}

fn open_stats_window(
    target: &EventLoopWindowTarget<UserEvent>,
    proxy: EventLoopProxy<UserEvent>,
) -> Option<(Window, WebView)> {
    let window = WindowBuilder::new()
        .with_title(format!("{APP_NAME} — Click stats"))
        .with_window_icon(app_icon())
        .with_inner_size(LogicalSize::new(900.0, 740.0))
        .with_min_inner_size(LogicalSize::new(760.0, 600.0))
        .build(target)
        .ok()?;
    let webview = wry::WebViewBuilder::new()
        .with_html(include_str!("stats.html"))
        .with_background_color(webview_bg())
        .with_ipc_handler(move |req| {
            let _ = proxy.send_event(UserEvent::Stats(req.body().clone()));
        })
        .build(&window)
        .ok()?;
    Some((window, webview))
}

fn spawn_check(proxy: EventLoopProxy<UserEvent>, manual: bool) {
    std::thread::spawn(move || {
        let _ = proxy.send_event(UserEvent::Update(UpdateEv::Checked(updater::check(), manual)));
    });
}

fn spawn_install(proxy: EventLoopProxy<UserEvent>, a: updater::Available) {
    std::thread::spawn(move || {
        let _ = proxy.send_event(UserEvent::Update(UpdateEv::Installed(updater::install(&a))));
    });
}

fn open_mouse_window(
    target: &EventLoopWindowTarget<UserEvent>,
    proxy: EventLoopProxy<UserEvent>,
) -> Option<(Window, WebView)> {
    let window = WindowBuilder::new()
        .with_title(format!("{APP_NAME} — Mouse settings"))
        .with_window_icon(app_icon())
        .with_inner_size(LogicalSize::new(860.0, 900.0))
        .with_min_inner_size(LogicalSize::new(760.0, 600.0))
        .build(target)
        .ok()?;
    let webview = wry::WebViewBuilder::new()
        .with_html(include_str!("mouse.html"))
        .with_background_color(webview_bg())
        .with_ipc_handler(move |req| {
            let _ = proxy.send_event(UserEvent::Mouse(req.body().clone()));
        })
        .build(&window)
        .ok()?;
    Some((window, webview))
}

fn backups_dir() -> std::path::PathBuf {
    Config::dir().join("backups")
}

/// Newest first: (file name, label).
fn list_backups() -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = std::fs::read_dir(backups_dir())
        .map(|it| {
            it.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let label = name.strip_prefix("onboard ")?.strip_suffix(".json")?.to_string();
                    let label = match label.split_once(' ') {
                        Some((d, t)) => format!("{d} at {}", t.replacen('-', ":", 2)),
                        None => label,
                    };
                    Some((name, label))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v
}

/// Sectors from a backup file that belong in user memory (directory + profiles).
fn load_backup(name: &str) -> Result<Vec<(u16, Vec<u8>)>, String> {
    if name.contains(['/', '\\']) || name.contains("..") {
        return Err("bad backup name".into());
    }
    let text = std::fs::read_to_string(backups_dir().join(name)).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (k, hexs) in v["sectors"].as_object().ok_or("backup has no sectors")? {
        let sec = u16::from_str_radix(k, 16).map_err(|_| "bad sector id")?;
        if sec >= 0x0100 {
            continue; // ROM: read-only
        }
        let bytes = onboard::unhex(hexs.as_str().unwrap_or("")).ok_or("bad sector data")?;
        if onboard::crc_ok(&bytes) {
            out.push((sec, bytes));
        }
    }
    out.sort_by_key(|(s, _)| *s);
    if out.is_empty() {
        return Err("backup contains no valid sectors".into());
    }
    Ok(out)
}

fn main() {
    // After a self-update the new exe starts while the old one is still
    // shutting down: wait (briefly) for it to release the instance lock.
    let after_update = std::env::args().any(|a| a == "--updated");
    let mut first = platform::single_instance();
    if !first && after_update {
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(250));
            if platform::single_instance() {
                first = true;
                break;
            }
        }
    }
    if !first {
        return;
    }
    updater::cleanup_old();

    let (mut cfg, cfg_error) = match Config::load() {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(e)),
    };
    let mut stats = clicks::Stats::load();
    clicks::set_enabled(cfg.count_clicks);

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();

    let proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Tray(e));
    }));
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Menu(e));
    }));

    // Device worker.
    let (tx, rx) = mpsc::channel::<Cmd>();
    let proxy = event_loop.create_proxy();
    let initial = light_of(&cfg);
    std::thread::spawn(move || {
        device::run(rx, initial, move |e| {
            let _ = proxy.send_event(UserEvent::Device(e));
        })
    });

    // Flyout window.
    let builder = WindowBuilder::new()
        .with_title(APP_NAME)
        .with_window_icon(app_icon())
        .with_decorations(false)
        .with_resizable(false)
        .with_visible(false)
        .with_always_on_top(true)
        .with_inner_size(LogicalSize::new(WIDTH, 460.0));
    #[cfg(windows)]
    let builder = {
        use tao::platform::windows::WindowBuilderExtWindows;
        builder.with_skip_taskbar(true).with_undecorated_shadow(true)
    };
    let window = builder.build(&event_loop).expect("window");
    #[cfg(windows)]
    {
        use tao::platform::windows::WindowExtWindows;
        platform::round_corners(window.hwnd());
    }

    let proxy = event_loop.create_proxy();
    let webview = wry::WebViewBuilder::new()
        .with_html(include_str!("ui.html"))
        .with_background_color(webview_bg())
        .with_ipc_handler(move |req| {
            let _ = proxy.send_event(UserEvent::Ui(req.body().clone()));
        })
        .build(&window)
        .expect("webview");

    // Tray menu (right click).
    let open_i = MenuItem::new("Open", true, None);
    let light_i = CheckMenuItem::new("Lighting", true, cfg.lighting_enabled, None);
    let count_i = CheckMenuItem::new("Count clicks", true, cfg.count_clicks, None);
    let stats_i = MenuItem::new("Click stats…", true, None);
    let mouse_i = MenuItem::new("Mouse settings…", true, None);
    let auto_i = CheckMenuItem::new("Start with Windows", true, platform::autostart_enabled(), None);
    let edit_i = MenuItem::new("Edit profiles…", true, None);
    let quit_i = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    let _ = menu.append_items(&[
        &open_i,
        &mouse_i,
        &stats_i,
        &PredefinedMenuItem::separator(),
        &light_i,
        &count_i,
        &auto_i,
        &edit_i,
        &PredefinedMenuItem::separator(),
        &quit_i,
    ]);

    let stats_proxy = event_loop.create_proxy();
    let upd_proxy = event_loop.create_proxy();
    let mut upd = UpdateState { status: "idle", latest: None, error: String::new(), notified: String::new() };
    let started = Instant::now();
    let mut next_update_check = Instant::now() + FIRST_UPDATE_CHECK;
    let mut pin_tries = if cfg.always_show_in_tray { PIN_ATTEMPTS } else { 0 };
    let mut cleaned_old = false;
    let mut stats_win: Option<(Window, WebView)> = None;
    let mouse_proxy = event_loop.create_proxy();
    let mut mouse_win: Option<(Window, WebView)> = None;
    let mut mouse_ready = false;
    let mut stats_ready = false;

    let mut tray: Option<TrayIcon> = None;
    let mut menu = Some(menu);
    let mut status: Option<Status> = None;
    let mut ui_ready = false;
    let mut low_alerted = false;
    let mut lamparray = platform::lamparray_running();
    let mut last_lamp_check = Instant::now();
    let mut light_taskbar = platform::taskbar_light();
    let mut cfg_mtime = config_mtime();
    let mut height = 460.0;
    let mut anchor: Option<Rect> = None;
    let mut hidden_at = Instant::now() - Duration::from_secs(1);
    // Windows bounces focus to the taskbar right after a tray click (and back
    // from the tray menu), so only auto-hide once the panel really had focus.
    let mut shown_at = Instant::now();
    let mut got_focus = false;
    // Live DPI readout runs only while the panel or Mouse settings is open.
    let mut watching_dpi = false;
    let mut live: Option<(u8, Option<u16>)> = None; // (profile, dpi)
    let mut last_tick = Instant::now();
    let mut stats_dirty = false;
    let mut last_stats_save = Instant::now();

    let place = |window: &Window, rect: &Rect, height: f64| {
        let scale = window.scale_factor();
        let (w, h) = (WIDTH * scale, height * scale);
        let (ix, iy) = (rect.position.x, rect.position.y);
        let (iw, ih) = (rect.size.width as f64, rect.size.height as f64);
        let mon = window
            .available_monitors()
            .find(|m| {
                let (p, s) = (m.position(), m.size());
                ix >= p.x as f64
                    && ix < (p.x + s.width as i32) as f64
                    && iy >= p.y as f64
                    && iy < (p.y + s.height as i32) as f64
            })
            .or_else(|| window.primary_monitor());
        let gap = 12.0 * scale;
        let mut x = ix + iw / 2.0 - w / 2.0;
        let mut y = iy - h - gap;
        if let Some(m) = mon {
            let (p, s) = (m.position(), m.size());
            let (mx, my, mw, mh) = (p.x as f64, p.y as f64, s.width as f64, s.height as f64);
            if iy < my + mh / 2.0 {
                y = iy + ih + gap; // taskbar on top
            }
            x = x.clamp(mx + gap, mx + mw - w - gap);
            y = y.clamp(my + gap, my + mh - h - gap);
        }
        window.set_outer_position(PhysicalPosition::new(x, y));
    };

    event_loop.run(move |event, target, control_flow| {
        let mut refresh_icon = false;
        let mut refresh_ui = false;
        let mut refresh_stats = false;
        let mut apply = false;
        let mut quit = false;

        // Periodic work runs on whatever event arrives once the tick is due,
        // so a busy event stream can't starve it.
        if last_tick.elapsed() >= TICK {
            last_tick = Instant::now();

            if cfg.count_clicks && stats.add(clicks::drain()) {
                stats_dirty = true;
                refresh_ui = window.is_visible();
                refresh_stats = stats_win.is_some();
            }
            if stats_dirty && last_stats_save.elapsed() >= STATS_SAVE {
                stats.save();
                stats_dirty = false;
                last_stats_save = Instant::now();
            }

            // Keep the icon visible in the tray (entry appears once it's shown).
            if pin_tries > 0 && tray.is_some() {
                pin_tries -= 1;
                if platform::set_tray_promoted(true) {
                    pin_tries = 0;
                }
            }
            if !cleaned_old && started.elapsed() > Duration::from_secs(15) {
                cleaned_old = true;
                updater::cleanup_old();
            }
            if Instant::now() >= next_update_check {
                next_update_check = Instant::now() + UPDATE_INTERVAL;
                spawn_check(upd_proxy.clone(), false);
            }

            let lt = platform::taskbar_light();
            if lt != light_taskbar {
                light_taskbar = lt;
                refresh_icon = true;
            }
            if config_mtime() != cfg_mtime {
                cfg_mtime = config_mtime();
                match Config::load() {
                    Ok(c) => {
                        cfg = c;
                        refresh_icon = true;
                        clicks::set_enabled(cfg.count_clicks);
                        light_i.set_checked(cfg.lighting_enabled);
                        count_i.set_checked(cfg.count_clicks);
                        apply = true;
                        refresh_ui = true;
                    }
                    Err(e) => platform::toast("config.json has an error", &e),
                }
            }
            if last_lamp_check.elapsed() >= LAMPARRAY_CHECK {
                last_lamp_check = Instant::now();
                let running = platform::lamparray_running();
                if running && !lamparray {
                    platform::toast(
                        "Logitech LampArray Service is running",
                        "It can override your lighting. Open the panel for the one-line fix.",
                    );
                }
                if running != lamparray {
                    lamparray = running;
                    refresh_ui = true;
                }
            }
        }
        *control_flow = ControlFlow::WaitUntil(last_tick + TICK);

        match event {
            Event::NewEvents(StartCause::Init) => {
                tray = TrayIconBuilder::new()
                    .with_menu(Box::new(menu.take().unwrap()))
                    .with_menu_on_left_click(false)
                    .with_tooltip(tooltip(None))
                    .with_icon(tray_image(&cfg, None, light_taskbar).unwrap())
                    .build()
                    .ok();
                if let Some(e) = &cfg_error {
                    platform::toast("config.json has an error — using defaults", e);
                }
                if after_update {
                    platform::toast(APP_NAME, &format!("Updated to version {}", updater::VERSION));
                }
            }

            Event::UserEvent(UserEvent::Update(ev)) => {
                match ev {
                    UpdateEv::Checked(Ok(None), _) => {
                        upd.status = "uptodate";
                        upd.latest = None;
                        upd.error.clear();
                    }
                    UpdateEv::Checked(Ok(Some(a)), _) => {
                        upd.error.clear();
                        if cfg.auto_update {
                            upd.status = "installing";
                            spawn_install(upd_proxy.clone(), a.clone());
                        } else {
                            upd.status = "available";
                            if upd.notified != a.version {
                                upd.notified = a.version.clone();
                                platform::toast(
                                    "Update available",
                                    &format!("Version {} is ready. Open the panel to install.", a.version),
                                );
                            }
                        }
                        upd.latest = Some(a);
                    }
                    UpdateEv::Checked(Err(e), manual) => {
                        if manual {
                            upd.status = "error";
                        }
                        upd.error = e;
                    }
                    UpdateEv::Installed(Ok(())) => quit = true,
                    UpdateEv::Installed(Err(e)) => {
                        upd.status = "error";
                        platform::toast("Update failed", &e);
                        upd.error = e;
                    }
                }
                refresh_ui = true;
            }

            Event::UserEvent(UserEvent::Device(device::Event::Onboard(r))) => {
                if let (Some((_, wv)), true) = (&mouse_win, mouse_ready) {
                    let js = match r {
                        Ok(d) => format!("onboard({})", serde_json::json!({ "ok": true, "data": d })),
                        Err(e) => format!("onboard({})", serde_json::json!({ "ok": false, "error": e })),
                    };
                    let _ = wv.evaluate_script(&js);
                }
            }

            Event::UserEvent(UserEvent::Device(device::Event::Dpi { profile, index, dpi })) => {
                live = Some((profile, dpi));
                refresh_ui = window.is_visible();
                if let (Some((_, wv)), true) = (&mouse_win, mouse_ready) {
                    let js = format!(
                        "dpi({})",
                        serde_json::json!({ "profile": profile, "index": index, "dpi": dpi })
                    );
                    let _ = wv.evaluate_script(&js);
                }
            }

            Event::UserEvent(UserEvent::Device(device::Event::Wrote(r))) => {
                if let (Some((_, wv)), true) = (&mouse_win, mouse_ready) {
                    let js = match r {
                        Ok(m) => format!("wrote({})", serde_json::json!({ "ok": true, "msg": m })),
                        Err(e) => format!("wrote({})", serde_json::json!({ "ok": false, "error": e })),
                    };
                    let _ = wv.evaluate_script(&js);
                }
            }

            Event::UserEvent(UserEvent::Mouse(body)) => {
                if let Ok(msg) = serde_json::from_str::<UiMsg>(&body) {
                    let page = |js: String| {
                        if let Some((_, wv)) = &mouse_win {
                            let _ = wv.evaluate_script(&js);
                        }
                    };
                    match msg.cmd.as_str() {
                        "ready" => {
                            mouse_ready = true;
                            page(format!("accent({})", serde_json::json!(platform::accent_color())));
                            let _ = tx.send(Cmd::ReadOnboard);
                        }
                        "save" => {
                            let _ = tx.send(Cmd::WriteOnboard { profiles: msg.profiles, active: (msg.value as u8).max(1) });
                        }
                        "backups" => {
                            let list: Vec<_> = list_backups()
                                .into_iter()
                                .map(|(name, label)| serde_json::json!({ "name": name, "label": label }))
                                .collect();
                            page(format!("backups({})", serde_json::Value::Array(list)));
                        }
                        "restore" => match load_backup(&msg.name) {
                            Ok(sectors) => {
                                let _ = tx.send(Cmd::Restore(sectors));
                            }
                            Err(e) => page(format!("wrote({})", serde_json::json!({ "ok": false, "error": e }))),
                        },
                        "open_backups" => {
                            let _ = std::fs::create_dir_all(backups_dir());
                            let _ = std::process::Command::new("explorer").arg(backups_dir()).spawn();
                        }
                        _ => {}
                    }
                }
            }

            Event::UserEvent(UserEvent::Device(device::Event::Status(s))) => {
                if let Some(st) = &s {
                    if st.charging {
                        low_alerted = false;
                    } else if let (true, Some(p)) = (st.online, st.percent) {
                        let limit = cfg.low_battery_alert;
                        if limit > 0 && p <= limit && !low_alerted {
                            platform::toast(&format!("{} battery low", st.name), &format!("{p}% left. Time to plug in."));
                            low_alerted = true;
                        } else if p > limit.saturating_add(5) {
                            low_alerted = false;
                        }
                    }
                }
                status = s;
                refresh_icon = true;
                refresh_ui = true;
            }

            Event::UserEvent(UserEvent::Tray(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            })) => {
                if window.is_visible() {
                    window.set_visible(false);
                    hidden_at = Instant::now();
                } else if hidden_at.elapsed() > Duration::from_millis(300) {
                    anchor = Some(rect);
                    place(&window, &rect, height);
                    window.set_visible(true);
                    window.set_focus();
                    shown_at = Instant::now();
                    got_focus = false;
                    refresh_ui = true;
                }
            }

            Event::UserEvent(UserEvent::Menu(e)) => {
                if e.id == *open_i.id() {
                    if let Some(r) = anchor.or_else(|| tray.as_ref().and_then(|t| t.rect())) {
                        anchor = Some(r);
                        place(&window, &r, height);
                    }
                    window.set_visible(true);
                    window.set_focus();
                    shown_at = Instant::now();
                    got_focus = false;
                    refresh_ui = true;
                } else if e.id == *mouse_i.id() {
                    if mouse_win.is_none() {
                        mouse_win = open_mouse_window(target, mouse_proxy.clone());
                        mouse_ready = false;
                    } else if let Some((w, _)) = &mouse_win {
                        w.set_focus();
                    }
                } else if e.id == *stats_i.id() {
                    if stats_win.is_none() {
                        stats_win = open_stats_window(target, stats_proxy.clone());
                        stats_ready = false;
                    } else if let Some((w, _)) = &stats_win {
                        w.set_focus();
                    }
                } else if e.id == *light_i.id() {
                    cfg.lighting_enabled = light_i.is_checked();
                    cfg.save();
                    apply = true;
                    refresh_ui = true;
                } else if e.id == *count_i.id() {
                    cfg.count_clicks = count_i.is_checked();
                    clicks::set_enabled(cfg.count_clicks);
                    cfg.save();
                    refresh_ui = true;
                    refresh_stats = true;
                } else if e.id == *auto_i.id() {
                    platform::set_autostart(auto_i.is_checked());
                    refresh_ui = true;
                } else if e.id == *edit_i.id() {
                    platform::open_in_editor(&Config::path());
                } else if e.id == *quit_i.id() {
                    quit = true;
                }
                cfg_mtime = config_mtime();
            }

            Event::UserEvent(UserEvent::Ui(body)) => {
                if let Ok(msg) = serde_json::from_str::<UiMsg>(&body) {
                    match msg.cmd.as_str() {
                        "ready" => {
                            ui_ready = true;
                            refresh_ui = true;
                        }
                        "height" => {
                            let h = msg.value.clamp(200.0, 900.0);
                            if (h - height).abs() >= 1.0 {
                                height = h;
                                window.set_inner_size(LogicalSize::new(WIDTH, height));
                                if let Some(r) = anchor {
                                    if window.is_visible() {
                                        place(&window, &r, height);
                                    }
                                }
                            }
                        }
                        "toggle" => {
                            cfg.lighting_enabled = msg.on;
                            light_i.set_checked(msg.on);
                            cfg.save();
                            apply = true;
                            refresh_ui = true;
                        }
                        "profile" => {
                            cfg.active_profile = msg.name;
                            cfg.lighting_enabled = true;
                            light_i.set_checked(true);
                            cfg.save();
                            apply = true;
                            refresh_ui = true;
                        }
                        "brightness" => {
                            cfg.brightness = (msg.value as u8).clamp(10, 100);
                            cfg.save();
                            apply = true;
                        }
                        "autostart" => {
                            platform::set_autostart(msg.on);
                            auto_i.set_checked(msg.on);
                        }
                        "clicks" => {
                            cfg.count_clicks = msg.on;
                            clicks::set_enabled(msg.on);
                            count_i.set_checked(msg.on);
                            cfg.save();
                            refresh_ui = true;
                            refresh_stats = true;
                        }
                        "mouse" => {
                            window.set_visible(false);
                            hidden_at = Instant::now();
                            if mouse_win.is_none() {
                                mouse_win = open_mouse_window(target, mouse_proxy.clone());
                                mouse_ready = false;
                            } else if let Some((w, _)) = &mouse_win {
                                w.set_focus();
                            }
                        }
                        "stats" => {
                            window.set_visible(false);
                            hidden_at = Instant::now();
                            if stats_win.is_none() {
                                stats_win = open_stats_window(target, stats_proxy.clone());
                                stats_ready = false;
                            } else if let Some((w, _)) = &stats_win {
                                w.set_focus();
                            }
                        }
                        "open_file" => {
                            window.set_visible(false);
                            hidden_at = Instant::now();
                            platform::open_in_editor(&Config::path());
                        }
                        "edit" => {
                            // Live edit of the active profile from the Customize section.
                            let i = cfg.active_index();
                            let p = &mut cfg.profiles[i];
                            if !msg.effect.is_empty() {
                                if let Ok(fx) = serde_json::from_value(serde_json::Value::String(msg.effect.clone())) {
                                    p.effect = fx;
                                }
                            }
                            if msg.color.len() == 7 && msg.color.starts_with('#') {
                                p.color = msg.color.to_uppercase();
                            }
                            if msg.period > 0.0 {
                                p.period_ms = (msg.period as u16).clamp(500, 10000);
                            }
                            cfg.lighting_enabled = true;
                            light_i.set_checked(true);
                            apply = true;
                            if msg.commit {
                                cfg.save();
                                refresh_ui = true;
                            }
                        }
                        "new_profile" => {
                            let mut p = cfg.active();
                            p.name = cfg.unique_name(&msg.name);
                            cfg.active_profile = p.name.clone();
                            cfg.profiles.push(p);
                            cfg.save();
                            refresh_ui = true;
                        }
                        "delete_profile" => {
                            if cfg.profiles.len() > 1 {
                                let i = cfg.active_index();
                                cfg.profiles.remove(i);
                                cfg.active_profile = cfg.profiles[i.min(cfg.profiles.len() - 1)].name.clone();
                                cfg.save();
                                apply = true;
                                refresh_ui = true;
                            }
                        }
                        "off_below" => {
                            cfg.lights_off_below = (msg.value as u8).min(90);
                            cfg.save();
                            apply = true;
                            refresh_ui = true;
                        }
                        "low_alert" => {
                            cfg.low_battery_alert = (msg.value as u8).min(90);
                            low_alerted = false;
                            cfg.save();
                        }
                        "pin" => {
                            cfg.always_show_in_tray = msg.on;
                            cfg.save();
                            if msg.on {
                                pin_tries = PIN_ATTEMPTS;
                            } else {
                                pin_tries = 0;
                                platform::set_tray_promoted(false);
                            }
                        }
                        "tray_icon" => {
                            cfg.tray_icon = if msg.name == "app" { config::TrayIcon::App } else { config::TrayIcon::Battery };
                            cfg.save();
                            refresh_icon = true;
                        }
                        "auto_update" => {
                            cfg.auto_update = msg.on;
                            cfg.save();
                        }
                        "check_update" => {
                            if upd.status != "checking" && upd.status != "installing" {
                                upd.status = "checking";
                                spawn_check(upd_proxy.clone(), true);
                                refresh_ui = true;
                            }
                        }
                        "install_update" => {
                            if let (Some(a), false) = (upd.latest.clone(), upd.status == "installing") {
                                upd.status = "installing";
                                spawn_install(upd_proxy.clone(), a);
                                refresh_ui = true;
                            }
                        }
                        "hide" => {
                            window.set_visible(false);
                            hidden_at = Instant::now();
                        }
                        "quit" => quit = true,
                        _ => {}
                    }
                    cfg_mtime = config_mtime();
                }
            }

            Event::UserEvent(UserEvent::Stats(body)) => {
                if let Ok(msg) = serde_json::from_str::<UiMsg>(&body) {
                    match msg.cmd.as_str() {
                        "ready" => {
                            stats_ready = true;
                            refresh_stats = true;
                        }
                        "height" => {
                            // Grow the window to fit the content, up to ~92% of the screen.
                            if let Some((w, _)) = &stats_win {
                                let scale = w.scale_factor();
                                let screen = w
                                    .current_monitor()
                                    .map(|m| m.size().height as f64 / scale)
                                    .unwrap_or(1000.0);
                                let want = (msg.value + 2.0).min(screen * 0.92);
                                let cur = w.inner_size().to_logical::<f64>(scale);
                                if want > cur.height + 1.0 {
                                    w.set_inner_size(LogicalSize::new(cur.width, want));
                                }
                            }
                        }
                        "reset" => {
                            let _ = clicks::drain();
                            stats = clicks::Stats::default();
                            stats.save();
                            stats_dirty = false;
                            refresh_stats = true;
                            refresh_ui = true;
                        }
                        _ => {}
                    }
                }
            }

            Event::WindowEvent { window_id, event, .. } => {
                if window_id == window.id() {
                    match event {
                        WindowEvent::Focused(true) => got_focus = true,
                        WindowEvent::Focused(false) if window.is_visible() => {
                            if got_focus && shown_at.elapsed() > Duration::from_millis(500) {
                                window.set_visible(false);
                                hidden_at = Instant::now();
                            } else {
                                // Focus blip while opening: take it back.
                                window.set_focus();
                            }
                        }
                        _ => {}
                    }
                } else if mouse_win.as_ref().is_some_and(|(w, _)| w.id() == window_id) {
                    if let WindowEvent::CloseRequested = event {
                        mouse_win = None;
                        mouse_ready = false;
                    }
                } else if stats_win.as_ref().is_some_and(|(w, _)| w.id() == window_id) {
                    if let WindowEvent::CloseRequested = event {
                        stats_win = None; // drop the webview; reopened on demand
                    }
                }
            }

            // Window/session teardown (e.g. Windows signing out): save what we have.
            Event::LoopDestroyed => {
                stats.add(clicks::drain());
                stats.save();
            }

            _ => {}
        }

        if quit {
            let _ = tx.send(Cmd::Quit);
            stats.add(clicks::drain());
            stats.save();
            tray = None;
            *control_flow = ControlFlow::Exit;
            return;
        }

        if apply {
            let _ = tx.send(Cmd::Apply(light_of(&cfg)));
        }
        let want_dpi = window.is_visible() || (mouse_win.is_some() && mouse_ready);
        if want_dpi != watching_dpi {
            watching_dpi = want_dpi;
            let _ = tx.send(Cmd::WatchDpi(want_dpi));
            if !want_dpi {
                live = None;
            }
        }
        if refresh_icon {
            if let Some(t) = &tray {
                let _ = t.set_icon(tray_image(&cfg, status.as_ref(), light_taskbar));
                let _ = t.set_tooltip(Some(tooltip(status.as_ref())));
            }
        }
        if refresh_ui && ui_ready {
            let js = format!("render({})", ui_state(&cfg, status.as_ref(), lamparray, &stats, &upd, live));
            let _ = webview.evaluate_script(&js);
        }
        if refresh_stats && stats_ready {
            if let Some((_, wv)) = &stats_win {
                let _ = wv.evaluate_script(&format!("render({})", stats_state(&cfg, &stats)));
            }
        }
    });
}
