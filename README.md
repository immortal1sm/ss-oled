# ss-oled

**A living HUD for the SteelSeries Apex Pro OLED — on Linux, without SteelSeries GG.**

Forked from [not-jan/apex-tux](https://github.com/not-jan/apex-tux) (which itself
revives the original apex-tux project). Upstream gets credit for the USB HID
protocol, the provider framework, and the rendering foundation. This fork turns
that foundation into something GG never offered: an **event-driven, glanceable
heads-up display** that reacts to your system in real time.

---

## What's on screen

Eight providers rotate automatically (dwell times configurable per provider):

| Provider | Dwell | Content |
|---|---|---|
| **MPRIS2** (music) | 30s + event jumps | Title, artist, elapsed/total timer, media source label, progress bar. Jumps to front on any play/pause/track-change/media-key press |
| **Sysinfo** | 30s | CPU/RAM/network/temperature bars |
| **Image** | 5s | Your own GIF/logo with Floyd–Steinberg dithering so multi-tone images keep their shades on the 1-bit panel |
| **Weather** | 10s | Big °C temp, condition label, precipitation %, animated icon (spinning sun rays, rain, lightning, snow, fog, drifting clouds) |
| **Forecast** | 30s | Next 5 days with slide transition between pages, hi/lo temps, weekday labels |
| **Lyrics** | 5s | Synchronized lyrics for the current track, line by line |
| **Clock** | 5s | 12h/24h configurable |
| **Custom** (HTTP-JSON) | per-provider | User-defined HTTP endpoints rendered with configurable fields |

## Lyrics

Shows the current lyric line for whatever MPRIS player is active, advancing in
sync with playback. Independent of the MPRIS2 provider — you can disable or
reorder either one freely.

```toml
[providers.lyrics]
enabled = true
priority = 6
source = "auto"      # auto | local | lrclib
font = "auto"        # auto | S | M | L | XL
align = "L"          # L | C | R
bold = false
show_title = false   # track title above the lyric
```

Dwell time comes from `[interval]` — `interval.lyrics = 5`, or the global
`interval.refresh`. It is *not* read from inside `[providers.lyrics]`.

**Where lyrics come from**, in order: a `.lrc` file sitting next to a local track,
then [lrclib.net](https://lrclib.net/) — an exact match on title/artist/album plus
track length, falling back to a metadata search and then a title-only search.
Results are cached under `~/.cache/apex-tux/lyrics/`, so repeats, restarts and
offline playback all work without another request.

**Sizing.** `font = "auto"` picks the largest size the line fits: XLarge (up to
3 lines) for short lyrics, Large (up to 5) for longer ones. XLarge's nominal
capacity is 48 characters, but wrapping breaks on word boundaries and wastes the
rest of each line, so it steps down well before that — which is why almost
nothing renders clipped. Anything that still exceeds its size ends with `>`.

Set a specific size (`S`/`M`/`L`/`XL`) to pin it; note that pinning bypasses the
auto-fit, so a long line will be clipped.

`show_title = true` draws the track title at XLarge above the lyric, wrapping to a
second line rather than cutting it off. On a 40px panel a wrapped title leaves
room for about one lyric line.

## Hotkeys

Default combos use **Ctrl+Shift** + numpad keys and can be changed in the GUI's
Hotkeys tab:

| Keys | Action |
|---|---|
| `Ctrl+Shift+Numpad /` | Next provider |
| `Ctrl+Shift+Numpad *` | Previous provider |
| `Ctrl+Shift+Numpad -` | Toggle lock/unlock — pins or releases the current screen |

Moving between providers while locked keeps the lock — you choose what stays.
Use the GUI Hotkeys tab to record a new combo or clear a shortcut entirely.

> **Recording numpad keys:** the GUI's Record button cannot tell a numpad key from
> its top-row twin, so tick the **Numpad** checkbox next to a hotkey before
> recording. Without it the binding is written as a top-row key, which on some
> keyboards never reaches the panel. You can also type the binding by hand —
> `Numpad1` and `Numpad 1` both parse.

## Weather data

Powered by [Open-Meteo](https://open-meteo.com/) — free, no API key.
Configure your location with the GUI\'s city search or directly in
`settings.toml`:

```toml
[weather]
enabled = true
latitude = 15.71611
longitude = 120.90306
timezone = ""
units = "metric"          # or "imperial" for °F
label = ""

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
declare which fields to show, preview the 128×40 layout in the GUI, and it renders them. No code changes required.

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

## Build & install

### 1. System dependencies

Install Rust nightly plus the libusb and DBus dev headers:

```bash
# Arch / CachyOS
sudo pacman -S rustup libusb dbus
rustup install nightly
rustup default nightly

# Debian / Ubuntu
sudo apt install cargo rustc libusb-1.0-0-dev libdbus-1-dev
```

### 2. Build the binaries

```bash
git clone https://github.com/immortal1sm/ss-oled.git
cd ss-oled
cargo build --release --features sysinfo,image,weather,hotkeys,custom,lyrics
```

> **Build all providers.** The feature list above is not additive by default —
> `--features sysinfo` alone builds *only* sysinfo. If you omit a provider's
> feature it is silently absent at runtime (its section in `settings.toml` is
> ignored), which looks like a config problem rather than a build one. Omitting
> flags also shrinks the binary and drops features you didn't intend to lose.

This produces three binaries in `target/release/`:
- `apex-tux` — the daemon (talks to the keyboard)
- `apex-tray` — the system-tray controller
- `apex-gui` — the settings editor (launched on demand by the tray)

### 3. Install the udev rule

The Apex Pro needs the user-level permission to access the USB device:

```bash
sudo cp 97-steelseries.rules /etc/udev/rules.d/
sudo udevadm control --reload
sudo udevadm trigger
# Unplug + replug the keyboard (or `sudo udevadm trigger --action=add`).
```

### 4. Install the systemd unit

```bash
mkdir -p ~/.config/systemd/user
cp systemd/apex-tux.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now apex-tux
```

The unit inherits `DBUS_SESSION_BUS_ADDRESS` from the graphical session so
MPRIS works under Wayland.

### 5. Install the suite launcher (optional but recommended)

```bash
ln -s "$(pwd)/scripts/ss-oled" ~/.local/bin/ss-oled
```

Then use the convenience verbs:

```bash
ss-oled start    # daemon + tray
ss-oled stop     # all three
ss-oled status   # what\'s running
ss-oled restart  # apply config changes
```

### Verifying the install

- `systemctl --user status apex-tux` — daemon is active
- An icon appears in the system tray
- `journalctl --user -u apex-tux -f` — log output
- The OLED on the keyboard lights up with the first provider (usually sysinfo)

If the OLED stays dark, check `journalctl` for permission errors — usually
the udev rule didn\'t pick up (replug, or `sudo udevadm trigger`).

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
  Custom HTTP-JSON provider ✅, synchronized lyrics ✅ (lrclib + local .lrc,
  on-disk cache)
- [ ] More providers — Games?
- [ ] Switch the USB crate to something async instead *(upstream tracks hidapi-rs#51; `nusb` is the likely successor)*
- [x] ~~Add documentation on how to add custom providers~~ — [docs/PROVIDERS.md](docs/PROVIDERS.md)
- [ ] Switch from GATs to async traits once they\'re stable
- [ ] Add support for more notifications

**ss-oled roadmap:**
- [x] **GUI + Tray suite** ✅ — config editor, drag-rearrange providers,
  live API Test button, embedded city geocoding search, Hotkeys tab,
  spawn-on-demand lifecycle, IPC-over-Unix-socket daemon control
- [x] **Custom JSON-API provider engine** ✅ — generic HTTP poll, JSON-path
  resolution, per-field layout (alignment, size, row, bold), word-wrap,
  live 128×40 GUI preview, NO DATA placeholders
- [ ] GPU telemetry provider (amdgpu hwmon: busy %, temps, power, VRAM)
- [ ] Idle blanking / dimming — real OLED burn-in mitigation
- [x] **Rebindable hotkeys** — GUI Hotkeys tab records settings-backed mappings
  for next, previous, and lock/unlock toggle; shortcuts can also be cleared
- [ ] Package for Arch (AUR) / Flatpak
- [ ] Demote diagnostic INFO logs in the focus path to DEBUG
