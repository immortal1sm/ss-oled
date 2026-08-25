# ss-oled

**A living HUD for the SteelSeries Apex Pro OLED — on Linux, without SteelSeries GG.**

Forked from [not-jan/apex-tux](https://github.com/not-jan/apex-tux) (which itself
revives the original apex-tux project). Upstream gets credit for the USB HID
protocol, the provider framework, and the rendering foundation. This fork turns
that foundation into something GG never offered: an **event-driven, glanceable
heads-up display** that reacts to your system in real time.

---

## What's on screen

Six providers rotate automatically (dwell times configurable per provider):

| Provider | Dwell | Content |
|---|---|---|
| **MPRIS2** (music) | 30s + event jumps | Title, artist, elapsed/total timer, media source label ("Firefox", "Spotify", "Phone"...), progress bar. Jumps to front on any play/pause/track-change/media-key press |
| **Sysinfo** | 30s | CPU/RAM/network/temperature bars |
| **Image** | 5s | Your own GIF/logo — rendered with Floyd–Steinberg dithering so multi-tone images keep their shades on the 1-bit panel (same trick SteelSeries GG uses) |
| **Weather** | 10s | Today: big °C temp, condition label, precipitation %, animated icon — spinning sun rays, falling rain, lightning flashes, snow, fog wisps, drifting clouds |
| **Forecast** | 30s | Next 5 days with a push-slide transition between pages, hi/lo temps, weekday labels (M/T/W/TH/F/ST/S), page dots |
| **Clock** | 5s | 12h/24h configurable |

## Hotkeys

All combos use **Ctrl+Shift** + numpad keys:

| Keys | Action |
|---|---|
| `Ctrl+Shift+Numpad /` | Next provider |
| `Ctrl+Shift+Numpad *` | Previous provider |
| `Ctrl+Shift+Numpad -` | **Lock** — pins the current screen absolutely: no rotation, no media-event jumps |
| `Ctrl+Shift+Numpad +` | **Unlock** — resumes auto-rotation and event reactions |

Moving between providers while locked keeps the lock — you choose what stays.

## Weather data

Powered by [Open-Meteo](https://open-meteo.com/) — free, no API key.
Configure your location in `settings.toml`:

```toml
[weather]
enabled = true
latitude = 15.71611        # your coordinates
longitude = 120.90306
timezone = "Asia/Manila"
units = "metric"
label = "Manila"

[forecast]
enabled = true
priority = 5

[interval]
refresh = 30               # global dwell; per-provider overrides below
weather = 10               # weather flashes by; forecast lingers
```

Data refreshes every 15 minutes and survives network drops (last good data
stays on screen).

## Why this architecture

The old approach for DIY OLED dashboards — the one used by my own
[arduino-pc-monitor](https://github.com/immortal1sm/arduino-pc-monitor), an
Arduino Nano + SH1106 dashboard — was: PC → Python loop → serial UART →
Arduino → SPI → panel. Five pipeline hops, ~55ms of wire time per frame,
continuous polling CPU, and a second device to power and maintain.

ss-oled talks to the keyboard's panel **directly over native USB HID**:

- One interrupt transfer per frame (<1ms) instead of a 115200-baud crawl
- Fully event-driven — the daemon sleeps until DBus signals, hotkeys, or
  dwell timers actually fire (~0% idle CPU, ~3MB RSS)
- No middleman hardware — the keyboard's own MCU drives its panel

Smaller pipeline, faster frames, fewer devices. That speed is what makes the
animated weather icons, slide transitions, and instant focus jumps possible.

> ss-oled was born from arduino-pc-monitor: same dashboard philosophy (sysinfo,
> media, weather), rebuilt for hardware that already sits on your desk. The
> Arduino project remains the reference for cross-vendor sensor work and
> standalone displays; ss-oled is where that experience lands when a SteelSeries
> Apex keyboard is available.

## Building

Requires Rust nightly, libusb and dbus dev headers:

```bash
cargo build --release --features sysinfo,image,weather,hotkeys
```

Install the udev rule (`97-steelseries.rules`, vendor `1038` product `1610`
with `uaccess` tag), then run as a user systemd unit with
`DBUS_SESSION_BUS_ADDRESS` inherited so MPRIS works under Wayland.

### Writing your own screens

See [docs/PROVIDERS.md](docs/PROVIDERS.md) —
a guide to writing custom providers with a minimal working example.

### Configuration

See `settings.toml` — every provider has `enabled` and `priority` keys;
dwell times via `[interval]`; weather location via `[weather]`.

## Acknowledgments

- [not-jan/apex-tux](https://github.com/not-jan/apex-tux) — the foundation
- The original apex-tux authors — Linux support in the first place
- [Open-Meteo](https://open-meteo.com/) — keyless weather API
- [embedded-graphics](https://github.com/embedded-graphics/embedded-graphics) — rendering stack

---

# TODO

Carried over from upstream, plus this fork's own roadmap:

**Upstream TODOs (status noted where applicable):**
- [ ] Windows support *(upstream goal; ss-oled is Linux-first for now)*
- [x] ~~Test on more than one Desktop Environment on X11~~ — **closed with reasoning:**
  ss-oled is display-server independent *by design*. It talks only to DBus,
  kernel HID, and the network — the codebase contains no X11/Wayland calls, and
  the release binary doesn't even link libX11. 
- [x] More providers — GIFs ✅ (image provider + FS dithering), Weather/Forecast ✅
- [ ] More providers — Games? 
- [ ] Switch the USB crate to something async instead *(upstream tracks hidapi-rs#51; `nusb` is the likely successor)*
- [x] ~~Add documentation on how to add custom providers~~ — [docs/PROVIDERS.md](docs/PROVIDERS.md)
- [ ] Switch from GATs to async traits once they're stable
- [ ] Add support for more notifications

**ss-oled roadmap:**
- [ ] GPU telemetry provider (amdgpu hwmon: busy %, temps, power, VRAM)
- [ ] Idle blanking / dimming — real OLED burn-in mitigation
- [ ] **GUI + Tray suite** — full design spec in [docs/DESIGN.md](docs/DESIGN.md):
      system tray controller (separate opt-in binary), schema-driven config
      window (drag-order rotation, live image preview, city-search weather),
      rebindable hotkeys with single lock toggle, and the custom provider
      engine (JSON API -> field mapping -> row layout) that lets users build
      screens without writing Rust. 
- [ ] Package for Arch (AUR) / Flatpak
- [ ] Demote diagnostic INFO logs in the focus path to DEBUG
