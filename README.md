# ss-oled

**A living HUD for the SteelSeries Apex Pro OLED — on Linux, without SteelSeries GG.**

Forked from [not-jan/apex-tux](https://github.com/not-jan/apex-tux) (which itself
revives the original apex-tux project). Upstream gets credit for the USB HID
protocol, the provider framework, and the rendering foundation. This fork turns
that foundation into something GG never offered: an **event-driven, glanceable
heads-up display** that reacts to your system in real time.

---

## What\'s on screen

Seven providers rotate automatically (dwell times configurable per provider):

| Provider | Dwell | Content |
|---|---|---|
| **MPRIS2** (music) | 30s + event jumps | Title, artist, elapsed/total timer, media source label, progress bar. Jumps to front on any play/pause/track-change/media-key press |
| **Sysinfo** | 30s | CPU/RAM/network/temperature bars |
| **Image** | 5s | Your own GIF/logo with Floyd–Steinberg dithering so multi-tone images keep their shades on the 1-bit panel |
| **Weather** | 10s | Big °C temp, condition label, precipitation %, animated icon (spinning sun rays, rain, lightning, snow, fog, drifting clouds) |
| **Forecast** | 30s | Next 5 days with slide transition between pages, hi/lo temps, weekday labels |
| **Clock** | 5s | 12h/24h configurable |
| **Custom** (HTTP-JSON) | per-provider | User-defined HTTP endpoints rendered with configurable fields |

## Hotkeys

All combos use **Ctrl+Shift** + numpad keys:

| Keys | Action |
|---|---|
| `Ctrl+Shift+Numpad /` | Next provider |
| `Ctrl+Shift+Numpad *` | Previous provider |
| `Ctrl+Shift+Numpad -` | Lock — pins the current screen absolutely: no rotation, no media-event jumps |
| `Ctrl+Shift+Numpad +` | Unlock — resumes auto-rotation and event reactions |

Moving between providers while locked keeps the lock — you choose what stays.

## Weather data

Powered by [Open-Meteo](https://open-meteo.com/) — free, no API key.
Configure your location with the GUI\'s city search or directly in
`settings.toml`:

```toml
[weather]
enabled = true
latitude = 15.71611
longitude = 120.90306
timezone = "Asia/Manila"
units = "metric"          # or "imperial" for °F
label = "Muñoz"

[forecast]
enabled = true
priority = 5

[interval]
refresh = 30               # global dwell; per-provider overrides below
weather = 10               # weather flashes by; forecast lingers
```

Data refreshes every 15 minutes and survives network drops (last good data
stays on screen).

## Custom JSON providers

A general-purpose HTTP+JSON provider engine — point it at any endpoint,
declare which fields to show, and it renders them. No code changes required.

```toml
[providers.custom.joke]
enabled = true
priority = 4
source = "https://official-joke-api.appspot.com/jokes/programming/random"
poll = 120
show_header = true
header = "JOKE"
fields = [
    "[0].setup: setup | a=L s=M",
    "[0].punchline: punchline | a=R s=L b=1",
]
```

| Field-spec token | Meaning |
|---|---|
| `path` | JSON path (dot notation + `[index]`) |
| `: label` | Optional display label; use `-` to hide label but keep value |
| `!` suffix | Hide value (label-only display) |
| `| a=L/C/R` | Horizontal alignment |
| `| s=S/M/L/X` | Font size (4×6 / 5×7 / 6×10 / 8×13) |
| `| r=0..5` | Explicit y-slot on the 40px panel |
| `| b=1` | Faux-bold double-strike |

The daemon handles fetching on a configurable interval, JSON-path resolution,
word-wrap onto multiple lines, and a "NO DATA" placeholder while waiting
for the first fetch. The GUI adds a live **Test** endpoint button and an
auto-fill suggestion pass over the response.

## Configuration GUI + system tray

The companion `apex-gui` is an `egui`-based editor for every settings.toml
key. **Spawn-on-demand:** tray menu **Open settings…** launches it; closing
the window frees the memory while the daemon and tray keep running.

```bash
ss-oled start    # launches daemon + tray
ss-oled stop     # shuts down all three
ss-oled status   # what\'s running
```

The tray (`apex-tray`, `ksni`-based) lets you:

- **Open settings…** — launch `apex-gui`
- **Provider switching** — jump to any enabled provider
- **Lock toggle** — same behavior as `Ctrl+Shift+Numpad -`
- **Restart service** — apply config changes without killing the GUI
- **Quit suite** — shuts down daemon + tray + GUI cleanly

The GUI and tray talk to the daemon over a **Unix-socket IPC**
(`/run/user/1000/apex-tux.sock`), keeping the daemon small and focused.

## Why this architecture

The old approach for DIY OLED dashboards — the one used by my own
[arduino-pc-monitor](https://github.com/immortal1sm/arduino-pc-monitor), an
Arduino Nano + SH1106 dashboard — was: PC → Python loop → serial UART →
Arduino → SPI → panel. Five pipeline hops, ~55ms of wire time per frame,
continuous polling CPU, and a second device to power and maintain.

ss-oled talks to the keyboard\'s panel **directly over native USB HID**:

- One interrupt transfer per frame (<1ms) instead of a 115200-baud crawl
- Fully event-driven — the daemon sleeps until DBus signals, hotkeys, or
  dwell timers actually fire (~0.4% idle CPU, ~12 MB RSS)
- GUI and tray are **separate processes** spawned on demand; the daemon
  itself stays lean
- No middleman hardware — the keyboard\'s own MCU drives its panel

Smaller pipeline, faster frames, fewer devices. That speed is what makes the
animated weather icons, slide transitions, and instant focus jumps possible.

> ss-oled was born from arduino-pc-monitor: same dashboard philosophy (sysinfo,
> media, weather), rebuilt for hardware that already sits on your desk. The
> Arduino project remains the reference for cross-vendor sensor work and
> standalone displays; ss-oled is where that experience lands when a
> SteelSeries Apex keyboard is available.

## Building

Requires Rust nightly, libusb and dbus dev headers:

```bash
cargo build --release --features sysinfo,image,weather,hotkeys,custom
```

Install the udev rule (`97-steelseries.rules`, vendor `1038` product `1610`
with `uaccess` tag), then run as a user systemd unit with
`DBUS_SESSION_BUS_ADDRESS` inherited so MPRIS works under Wayland.

### Writing your own providers

See [docs/PROVIDERS.md](docs/PROVIDERS.md) — a guide to writing custom
providers with a minimal working example, plus the JSON-path syntax and
field-spec options.

### Architecture deep-dive

See [docs/DESIGN.md](docs/DESIGN.md) — fork architecture, IPC layout, GUI +
tray lifecycle, suite launcher script, and the rationale behind the
spawn-on-demand design.

### Configuration

See `settings.toml` — every provider has `enabled` and `priority` keys;
dwell times via `[interval]`; weather location via `[weather]`; custom
providers under `[providers.custom.<name>]`.

## Acknowledgments

- [not-jan/apex-tux](https://github.com/not-jan/apex-tux) — the foundation
- The original apex-tux authors — Linux support in the first place
- [Open-Meteo](https://open-meteo.com/) — keyless weather API
- [embedded-graphics](https://github.com/embedded-graphics/embedded-graphics) — rendering stack

---

# TODO

Carried over from upstream, plus this fork\'s own roadmap:

**Upstream TODOs (status noted where applicable):**
- [ ] Windows support *(upstream goal; ss-oled is Linux-first for now)*
- [x] ~~Test on more than one Desktop Environment on X11~~ — **closed with reasoning:**
  ss-oled is display-server independent *by design*. It talks only to DBus,
  kernel HID, and the network — the codebase contains no X11/Wayland calls, and
  the release binary doesn\'t even link libX11.
- [x] More providers — GIFs ✅ (image provider + FS dithering), Weather/Forecast ✅,
  Custom HTTP-JSON provider ✅
- [ ] More providers — Games?
- [ ] Switch the USB crate to something async instead *(upstream tracks hidapi-rs#51; `nusb` is the likely successor)*
- [x] ~~Add documentation on how to add custom providers~~ — [docs/PROVIDERS.md](docs/PROVIDERS.md)
- [ ] Switch from GATs to async traits once they\'re stable
- [ ] Add support for more notifications

**ss-oled roadmap:**
- [x] **GUI + Tray suite** ✅ — schema-driven config editor, drag-rearrange
  providers, live API Test button, embedded city geocoding search,
  spawn-on-demand lifecycle, IPC-over-Unix-socket daemon control
- [x] **Custom JSON-API provider engine** ✅ — generic HTTP poll, JSON-path
  resolution, per-field layout (alignment, size, row, bold), word-wrap,
  NO DATA placeholders
- [ ] GPU telemetry provider (amdgpu hwmon: busy %, temps, power, VRAM)
- [ ] Idle blanking / dimming — real OLED burn-in mitigation
- [ ] **Rebindable hotkeys** — current Ctrl+Shift+Numpad combos are hardcoded;
  a Hotkeys tab in the GUI would let users remap (next/prev/lock) to any combo
- [ ] Package for Arch (AUR) / Flatpak
- [ ] Demote diagnostic INFO logs in the focus path to DEBUG
