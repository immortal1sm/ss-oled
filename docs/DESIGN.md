# ss-oled Design — GUI, Tray, Custom Providers

This document is the blueprint for ss-oled's divergence from upstream:
a schema-driven configuration GUI, a system tray controller, rebindable
hotkeys, and a user-facing custom provider engine.

## Principles

1. settings.toml stays the single source of truth. The GUI is a friendly
   writer for it; the terminal remains fully functional.
2. Daemon stays pure: no GUI/display-server dependencies in apex-tuxd.
   GUI and tray are separate opt-in binaries.
3. Providers declare their own config schema — built-in and third-party
   providers get GUI forms for free via generic rendering.
4. Memory budget: daemon ~4MB + tray ~10MB idle (<20MB combined).
   Config window is transient and bounded.

## System tray (ksni, separate binary)

Menu structure:

    ss-oled
    |- Current: weather (10s)        <- tooltip/icon reflects state
    |--------------------------------
    |- [x] Locked                    <- single toggle key
    |- Provider > MPRIS2 / Sysinfo / Image / Weather / Forecast
    |--------------------------------
    |- Edit settings...
    |- Restart daemon
    +- Quit tray

- Lock is ONE toggle, not two hotkeys (kglobalaccel fires the same
  signal regardless of state - flipping the bool is trivial).
- IPC to daemon over a unix socket (~50-line listener in daemon).
- "Edit settings..." opens $EDITOR or the GUI window if installed;
  daemon watches settings.toml via inotify and auto-applies changes.

## GUI window (egui, separate binary)

Layout: sidebar of providers + General tab. Bottom buttons:
[Revert] [Save] [Apply]
  - Apply = write file + restart daemon (live)
  - Save  = write file only (next restart)
  - Revert = discard unsaved edits

### General tab
Drag-to-reorder provider table - position IS the priority ranking
(rewrites `priority` ints on save). Checkbox per row = enabled/disabled.
Shared [location] section (see below) also lives here.

### Per-provider tabs

Every tab includes "Duration on screen" spinner (maps to interval.<name>;
0 = manual-only). Existing per-provider settings render from each
provider's declared schema.

- MPRIS2: event-focus toggle ("jump to music on play/pause/track change"
  -> new event_focus key), source-label toggle, timer-row toggle,
  preferred_player dropdown populated from live DBus names.
- Sysinfo: already complete - interface/sensor pickers only.
- Image: file picker dialog filtered to supported formats, LIVE PREVIEW
  rendered through the real read_image() pipeline (what you see is what
  ships, dithering included), Floyd-Steinberg on/off switch.
- Weather + Forecast: COMBINED into one provider whose dwell cycles
  today-view -> day2..6 using the existing push-slide machinery.
  Location by CITY NAME search (Open-Meteo geocoding API returns
  coordinates + timezone + canonical name in one call) - no raw
  coordinate entry needed. Units metric/imperial dropdown.
  The geocoder's timezone feeds the shared [location] section, which
  the clock also consumes (clock depends on [location], not on weather).
- Clock: 12h/24h toggle; timezone comes from shared [location].

### Hotkeys tab
Key-capture fields per action; parsed into Modifiers+Code at startup.
Defaults keep current bindings. Lock uses a single combo (toggle).

## Custom provider engine ("create provider")

Users cannot compile Rust at runtime - so instead of code generation,
ss-oled ships a GENERIC provider driven entirely by declaration:
poll a data source, map JSON fields to labels, lay them out in rows.
Same capability class as waybar/eww modules.

### Data source form
- API endpoint URL
- Headers incl. AUTH SECRETS: entered in the GUI as password fields
  (masked ****). Copy/paste is the primary input mode so reveal is
  unnecessary; secrets are stored out-of-band (env var or 0600 secrets
  file), never in shareable settings.toml.
- Poll interval (rate-limit friendly)
- [Test request] button: fetches once, fills live values into the
  mapping table for instant feedback

### Field mapping table

    JSON path                              -> Label    Value now
    items[0].snippet.title                 -> Username PewDiePie
    ..statistics.subscriberCount           -> Subs     111M
    custom.donation_goal                   -> Goal%    73

### Layout editor: row/grid model (not free-pixel)

Rationale: panel has ~6 fixed font sizes; monochrome 128x40 punishes
overlap with unreadable output; WYSIWYG pixel editors are huge work.
Each mapped field gets: row number, alignment, size class (S/M/L),
optional icon. GUI shows a LIVE PREVIEW canvas rendered through the
real engine beside an add/remove/reorder row list. Rows cover ~all
realistic screens (stats/counts/goals are 1-3 text rows plus a bar).

Example rendered screen from the mapping above:

    ----------------------------
    | PewDiePie                |
    | 111M subs                |
    | ########----  73%        |
    ----------------------------

## Platform limits (permanent, worth respecting)

1. 128x40 monochrome: ~4 readable text lines max; dithering fakes shades,
   never adds resolution.
2. No touch input on the panel itself - interaction via hotkeys/tray/GUI.
3. Render vocabulary: text lines, progress bars, packed bitmaps/GIFs.
4. Animation above ~10 FPS wastes effort on this panel.

## Implementation sequence

1. PROVIDERS.md doc (incl. config_schema() convention)
2. image.dither daemon key (small, becomes doc example)
3. Daemon: IPC unix-socket listener + schema introspection
4. Tray binary: menus first (lock/switch/restart/edit-settings)
5. Window binary: generic schema-driven forms
6. Custom provider engine: poll/template/rows
7. Layout editor UI on top of 6
