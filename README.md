# No G HUB Mouse Tray

`no-ghub-thanks` — a tiny Windows tray app that replaces Logitech G HUB for the two things most people actually need:

- **Battery % in the system tray**, with a low-battery toast.
- **Lighting control** — a master on/off switch that sticks across reboots, sleep/wake and cable swaps, plus lighting profiles (static, pulse, color cycle, battery-level color).
- **Click counter** (optional, off by default) with a stats page: all-time odometer, daily / weekly / monthly charts, averages, best day, streaks, per-button split, time-of-day and some deeply unnecessary facts.

It talks to the mouse directly over Logitech's HID++ 2.0 protocol. No services, no updater, no account, no telemetry. One ~2 MB exe.

Built and tested against a **G502 X PLUS** over both the LIGHTSPEED receiver and the USB cable. Other HID++ 2.0 mice with `UNIFIED_BATTERY (0x1004)` and `RGB_EFFECTS (0x8071)` should work; mice without RGB still get the battery icon.

## Install

1. Download `no-ghub-thanks.exe` from [Releases](../../releases) (`latest` is built from `main` on every push).
2. Uninstall G HUB.
3. Disable the **Logitech LampArray Service** (ships with the device driver, survives the G HUB uninstall, and fights over the LEDs). Admin PowerShell:
   ```powershell
   Start-Process powershell -Verb RunAs -ArgumentList '-NoProfile -Command "Get-Service logi_lamparray* | Stop-Service -Force -PassThru | Set-Service -StartupType Disabled"'
   ```
   The app warns you if it ever comes back.
4. Run the exe. Left-click the tray icon for the panel, right-click for the menu. Tick **Start with Windows**.

## Using it

- **Tray icon**: battery % with a level bar. Blue bar = charging. `--` = mouse asleep or not found. Hover for details.
- **Panel** (left-click): battery, lighting switch, profile chips, brightness, Start with Windows. Follows your Windows light/dark mode and accent color.
- **Lighting off** means off: it's re-applied every time the mouse connects or wakes, because the mouse falls back to its onboard lighting on every power cycle.

## Click counter

Turn on **Click counter** in the panel (or **Count clicks** in the tray menu). Clicks are read from Windows Raw Input — the same events every app receives — so there is no extra traffic to the mouse and nothing is added to the input path. Counted: left, right, middle, back, forward, wheel notches. The mouse's DPI and G-shift buttons are handled inside the mouse and never reach Windows, so they aren't counted.

Stats live in `%APPDATA%\no-ghub-thanks\clicks.json` (one row per day). They never leave the PC. **Reset stats** is at the bottom of the stats page.

## Custom name

Task Manager shows the name baked into the exe — **No G HUB Mouse Tray** by default. To build one with your own name:

- **GitHub:** Actions → **build** → **Run workflow** → type a *Display name* → download the exe from the run's artifacts.
- **Locally:** `$env:APP_DISPLAY_NAME = "Mouse Buddy"; cargo build --release`

## Profiles

Edit via **Edit profiles** (opens `%APPDATA%\no-ghub-thanks\config.json` in Notepad). Changes apply within a few seconds of saving.

```json
{
  "lighting_enabled": true,
  "active_profile": "Blue pulse",
  "brightness": 80,
  "low_battery_alert": 15,
  "profiles": [
    { "name": "Blue pulse", "effect": "breathe", "color": "#0040FF", "period_ms": 3000 },
    { "name": "Rainbow",    "effect": "cycle",   "period_ms": 6000 },
    { "name": "Amber",      "effect": "static",  "color": "#FFB000" },
    { "name": "Battery",    "effect": "battery" }
  ]
}
```

| effect    | uses                | notes                                              |
|-----------|---------------------|----------------------------------------------------|
| `off`     | —                   | LEDs dark                                          |
| `static`  | `color`             | solid color                                        |
| `breathe` | `color`, `period_ms`| pulse                                              |
| `cycle`   | `period_ms`         | hue rotation                                       |
| `battery` | —                   | green ≥50%, amber ≥20%, red below; pulses green while charging |

`low_battery_alert`: toast at this % while discharging (`0` disables).

## How it works

Windows splits the receiver's vendor interface into short (`0x10`) and long (`0x11`) report collections; the app sends HID++ 2.0 requests on the long one and listens for receiver link notifications (`0x41`) on the short one.

On connect or wake it:

1. Resolves feature indices (`0x1004` battery, `0x8071` RGB effects, `0x8100` onboard profiles).
2. Puts the mouse in onboard mode so DPI and buttons come from the mouse's own profile.
3. Claims the LED pipeline from firmware (`RGB_EFFECTS` SetSWControl).
4. Sets the cluster effect, looked up by effect id from the mouse's own effect table.

Battery comes from `UNIFIED_BATTERY` events plus a 60 s poll.

Protocol details cross-checked against [Solaar](https://github.com/pwr-Solaar/Solaar)'s documentation of the G502 X PLUS. No Solaar code is included.

## Build

```
cargo build --release
```

Windows, Rust stable, MSVC toolchain. The UI is a small HTML page rendered by WebView2 (preinstalled on Windows 11).

## Roadmap

- Onboard profiles: DPI levels and button remapping, written to the mouse's flash (with an automatic backup of all profiles before the first write).

## License

MIT
