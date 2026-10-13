#![cfg_attr(windows, windows_subsystem = "windows")]
//! no-ghub-thanks: battery % in the tray and lighting control for Logitech
//! HID++ 2.0 mice, without G HUB.

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
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, Rect, TrayIcon, TrayIconBuilder, TrayIconEvent};

enum UserEvent {
    Status(Option<Status>),
    Tray(TrayIconEvent),
    Menu(MenuEvent),
    Ui(String),
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
const LAMPARRAY_CHECK: Duration = Duration::from_secs(60);

fn light_of(cfg: &Config) -> Light {
    Light { enabled: cfg.lighting_enabled, profile: cfg.active(), brightness: cfg.brightness }
}

fn config_mtime() -> Option<SystemTime> {
    std::fs::metadata(Config::path()).and_then(|m| m.modified()).ok()
}

fn tooltip(status: Option<&Status>) -> String {
    match status {
        None => "No Logitech mouse found".into(),
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

fn ui_state(cfg: &Config, status: Option<&Status>, lamparray: bool) -> String {
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
    })
    .to_string()
}

fn main() {
    if !platform::single_instance() {
        return;
    }

    let (mut cfg, cfg_error) = match Config::load() {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(e)),
    };

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
        .with_title("No G HUB, thanks")
        .with_decorations(false)
        .with_resizable(false)
        .with_visible(false)
        .with_always_on_top(true)
        .with_inner_size(LogicalSize::new(WIDTH, 380.0));
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
    let bg = if platform::apps_light() { (243, 243, 243, 255) } else { (32, 32, 32, 255) };
    let webview = wry::WebViewBuilder::new()
        .with_html(include_str!("ui.html"))
        .with_background_color(bg)
        .with_ipc_handler(move |req| {
            let _ = proxy.send_event(UserEvent::Ui(req.body().clone()));
        })
        .build(&window)
        .expect("webview");

    // Tray menu (right click).
    let open_i = MenuItem::new("Open", true, None);
    let light_i = CheckMenuItem::new("Lighting", true, cfg.lighting_enabled, None);
    let auto_i = CheckMenuItem::new("Start with Windows", true, platform::autostart_enabled(), None);
    let edit_i = MenuItem::new("Edit profiles…", true, None);
    let quit_i = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    let _ = menu.append_items(&[
        &open_i,
        &PredefinedMenuItem::separator(),
        &light_i,
        &auto_i,
        &edit_i,
        &PredefinedMenuItem::separator(),
        &quit_i,
    ]);

    let mut tray: Option<TrayIcon> = None;
    let mut menu = Some(menu);
    let mut status: Option<Status> = None;
    let mut ui_ready = false;
    let mut low_alerted = false;
    let mut lamparray = platform::lamparray_running();
    let mut last_lamp_check = Instant::now();
    let mut light_taskbar = platform::taskbar_light();
    let mut cfg_mtime = config_mtime();
    let mut height = 380.0;
    let mut anchor: Option<Rect> = None;
    let mut hidden_at = Instant::now() - Duration::from_secs(1);

    let place = |window: &tao::window::Window, rect: &Rect, height: f64| {
        let scale = window.scale_factor();
        let (w, h) = (WIDTH * scale, height * scale);
        let (ix, iy) = (rect.position.x, rect.position.y);
        let (iw, ih) = (rect.size.width as f64, rect.size.height as f64);
        let mon = window
            .available_monitors()
            .find(|m| {
                let (p, s) = (m.position(), m.size());
                ix >= p.x as f64 && ix < (p.x + s.width as i32) as f64 && iy >= p.y as f64 && iy < (p.y + s.height as i32) as f64
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

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + TICK);

        let mut refresh_icon = false;
        let mut refresh_ui = false;
        let mut apply = false;

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

            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                // Theme change, config edited in notepad, LampArray service back.
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
                } else if e.id == *light_i.id() {
                    cfg.lighting_enabled = light_i.is_checked();
                    cfg.save();
                    apply = true;
                    refresh_ui = true;
                } else if e.id == *auto_i.id() {
                    platform::set_autostart(auto_i.is_checked());
                    refresh_ui = true;
                } else if e.id == *edit_i.id() {
                    platform::open_in_editor(&Config::path());
                } else if e.id == *quit_i.id() {
                    let _ = tx.send(Cmd::Quit);
                    tray = None;
                    *control_flow = ControlFlow::Exit;
                    return;
                }
            }

            Event::UserEvent(UserEvent::Ui(body)) => {
                let Ok(msg) = serde_json::from_str::<UiMsg>(&body) else { return };
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
                    "edit" => {
                        window.set_visible(false);
                        hidden_at = Instant::now();
                        platform::open_in_editor(&Config::path());
                    }
                    "hide" => {
                        window.set_visible(false);
                        hidden_at = Instant::now();
                    }
                    "quit" => {
                        let _ = tx.send(Cmd::Quit);
                        tray = None;
                        *control_flow = ControlFlow::Exit;
                        return;
                    }
                    _ => {}
                }
                cfg_mtime = config_mtime();
            }

            Event::WindowEvent { event: WindowEvent::Focused(false), .. } => {
                window.set_visible(false);
                hidden_at = Instant::now();
            }

            _ => {}
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
            let js = format!("render({})", ui_state(&cfg, status.as_ref(), lamparray));
            let _ = webview.evaluate_script(&js);
        }
    });
}
