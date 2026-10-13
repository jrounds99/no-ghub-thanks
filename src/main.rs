#![cfg_attr(windows, windows_subsystem = "windows")]
//! No G HUB Mouse Tray: battery % in the tray, lighting control and a click
//! counter for Logitech HID++ 2.0 mice, without G HUB.

mod clicks;
mod config;
mod device;
mod hidpp;
mod icon;
mod platform;

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
    Status(Option<Status>),
    Tray(TrayIconEvent),
    Menu(MenuEvent),
    Ui(String),
    Stats(String),
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
}

const WIDTH: f64 = 340.0;
const TICK: Duration = Duration::from_secs(5);
const STATS_SAVE: Duration = Duration::from_secs(60);
const LAMPARRAY_CHECK: Duration = Duration::from_secs(60);

fn light_of(cfg: &Config) -> Light {
    Light { enabled: cfg.lighting_enabled, profile: cfg.active(), brightness: cfg.brightness }
}

fn config_mtime() -> Option<SystemTime> {
    std::fs::metadata(Config::path()).and_then(|m| m.modified()).ok()
}

fn tooltip(status: Option<&Status>) -> String {
    match status {
        None => format!("{APP_NAME}\nNo Logitech mouse found"),
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

fn ui_state(cfg: &Config, status: Option<&Status>, lamparray: bool, stats: &clicks::Stats) -> String {
    let (today, total) = click_totals(stats);
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

fn main() {
    if !platform::single_instance() {
        return;
    }

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
        device::run(rx, initial, move |s| {
            let _ = proxy.send_event(UserEvent::Status(s));
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
    let auto_i = CheckMenuItem::new("Start with Windows", true, platform::autostart_enabled(), None);
    let edit_i = MenuItem::new("Edit profiles…", true, None);
    let quit_i = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    let _ = menu.append_items(&[
        &open_i,
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
    let mut stats_win: Option<(Window, WebView)> = None;
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
                let rgba = icon::render(None, light_taskbar);
                tray = TrayIconBuilder::new()
                    .with_menu(Box::new(menu.take().unwrap()))
                    .with_menu_on_left_click(false)
                    .with_tooltip(tooltip(None))
                    .with_icon(Icon::from_rgba(rgba, icon::ICON_SIZE, icon::ICON_SIZE).unwrap())
                    .build()
                    .ok();
                if let Some(e) = &cfg_error {
                    platform::toast("config.json has an error — using defaults", e);
                }
            }

            Event::UserEvent(UserEvent::Status(s)) => {
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
                    refresh_ui = true;
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
                        "edit" => {
                            window.set_visible(false);
                            hidden_at = Instant::now();
                            platform::open_in_editor(&Config::path());
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
                    if let WindowEvent::Focused(false) = event {
                        window.set_visible(false);
                        hidden_at = Instant::now();
                    }
                } else if stats_win.as_ref().is_some_and(|(w, _)| w.id() == window_id) {
                    if let WindowEvent::CloseRequested = event {
                        stats_win = None; // drop the webview; reopened on demand
                    }
                }
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
        if refresh_icon {
            if let Some(t) = &tray {
                let rgba = icon::render(status.as_ref(), light_taskbar);
                let _ = t.set_icon(Icon::from_rgba(rgba, icon::ICON_SIZE, icon::ICON_SIZE).ok());
                let _ = t.set_tooltip(Some(tooltip(status.as_ref())));
            }
        }
        if refresh_ui && ui_ready {
            let js = format!("render({})", ui_state(&cfg, status.as_ref(), lamparray, &stats));
            let _ = webview.evaluate_script(&js);
        }
        if refresh_stats && stats_ready {
            if let Some((_, wv)) = &stats_win {
                let _ = wv.evaluate_script(&format!("render({})", stats_state(&cfg, &stats)));
            }
        }
    });
}
