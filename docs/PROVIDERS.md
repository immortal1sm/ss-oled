# Writing Custom Providers

A provider is a screen on your Apex keyboard's OLED. This guide shows how to
write one — from a minimal working example to the full conventions used by
ss-oled's built-in providers (clock, sysinfo, image, music, weather, forecast).

## The mental model

A provider is a struct that:

1. Reads its settings from `settings.toml`
2. Yields 128×40 monochrome frames as a `Stream`
3. Optionally asks the scheduler for focus when something happens

The scheduler rotates between enabled providers (per-provider dwell times),
and event-driven providers can jump to the front of the queue.

## Minimal working example

A provider that shows "hello" plus the system's uptime. Complete file —
drop it in as `src/providers/uptime.rs`:

```rust
use crate::render::{
    display::ContentProvider,
    scheduler::{ContentWrapper, FocusChannel, CONTENT_PROVIDERS},
};
use anyhow::Result;
use apex_hardware::FrameBuffer;
use async_stream::try_stream;
use config::Config;
use embedded_graphics::{
    mono_font::{iso_8859_15, MonoTextStyle},
    pixelcolor::BinaryColor,
    text::{renderer::TextRenderer, Baseline, Text},
    prelude::Point,
    Drawable,
};
use futures::Stream;
use linkme::distributed_slice;
use log::info;

// 1. Self-registration: this static hooks into the scheduler's provider list.
#[distributed_slice(CONTENT_PROVIDERS)]
static PROVIDER_INIT: fn(&Config, FocusChannel) -> Result<Box<dyn ContentWrapper>> =
    register_callback;

struct Uptime;

impl Uptime {
    fn render(&self) -> Result<FrameBuffer> {
        let mut buffer = FrameBuffer::new();

        // Read kernel uptime (blocking but instant — fine inline).
        let uptime = std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
            .unwrap_or(0.0);

        let days = (uptime / 86400.0).floor() as i64;
        let hours = ((uptime % 86400.0) / 3600.0).floor() as i64;
        let text = format!("Up {days}d {hours}h");

        let style = MonoTextStyle::new(&iso_8859_15::FONT_10X20, BinaryColor::On);
        Text::with_baseline(&text, Point::new(4, 12), style, Baseline::Top)
            .draw(&mut buffer)?;

        Ok(buffer)
    }
}

impl ContentProvider for Uptime {
    type ContentStream<'a> = impl Stream<Item = Result<FrameBuffer>> + 'a;

    fn stream(&mut self) -> Result<Self::ContentStream<'_>> {
        Ok(try_stream! {
            use tokio::time::{interval, Duration, MissedTickBehavior};
            let mut tick = interval(Duration::from_secs(60));
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                yield self.render()?;
                tick.tick().await;   // re-render every minute
            }
        })
    }

    fn name(&self) -> &'static str {
        "uptime"
    }
}

// 2. Registration callback: check `enabled`, build the provider.
fn register_callback(config: &Config, _focus_tx: FocusChannel) -> Result<Box<dyn ContentWrapper>> {
    info!("Registering Uptime display source.");

    if !config.get_bool("uptime.enabled").unwrap_or(false) {
        anyhow::bail!("uptime provider disabled");
    }

    Ok(Box::new(Uptime))
}
```

Wire it into `src/providers/mod.rs`:

```rust
pub(crate) mod uptime;
```

Then enable it in `~/.config/apex-tux/settings.toml`:

```toml
[uptime]
enabled = true
priority = 6
interval = 30   # dwell time on screen, seconds
```

That's it — rebuild and the rotation includes it.

## The three pieces, explained

### 1. Registration (`distributed_slice`)

The scheduler discovers providers at startup through a link-time collected
array (`CONTENT_PROVIDERS`). You never edit a central list — adding the
`#[distributed_slice]` static is enough. The registered function receives
the parsed config and returns either a boxed provider or `Err`, which means
"skip me." Returning `Err` when disabled is the convention (it logs at INFO).

### 2. `ContentProvider`

Two required items:

- `type ContentStream<'a>` + `stream()` — an async stream of frames.
- `fn name()` — the provider's settings/identity key ("weather", "mpris2").
  Must match the `[section]` name in settings.toml.

### 3. Frame rendering

A `FrameBuffer` is a 128×40 monochrome canvas. Anything from
[embedded-graphics](https://docs.rs/embedded-graphics) draws onto it:
`Text`, `Line`, `Circle`, `Rectangle`, raw `ImageRaw` bitmaps.

**Hard bounds:** y must stay within 0..=39, x within 0..=127. Content drawn
past those rows is silently clipped — this has bitten us before (see git log,
"fit layouts in 40px panel"). Compute your layout against DISPLAY_HEIGHT.

## Conventions

### Settings keys

Every provider owns a `[<name>]` section with at minimum:

| Key | Meaning |
|---|---|
| `enabled` | checked in `register_callback`; `false` → bail |
| `priority` | position in rotation order |
| (dwell) | set under the global `[interval]` section as `interval.<name>` |

Add custom keys freely (`weather.latitude`, `image.path`) — but read them
in `register_callback` or the provider constructor, never per-frame.

### Timing inside `stream()`

Yield frames on a schedule, not in a tight loop:

```rust
let mut tick = tokio::time::interval(Duration::from_millis(300));
tick.set_missed_tick_behavior(MissedTickBehavior::Skip);  // don't burst after stalls
loop {
    yield self.render()?;
    tick.tick().await;
}
```

Animation cadence used by built-ins: ~300ms per frame is plenty for this
panel — faster is wasted effort.

### Blocking work (HTTP, file I/O beyond a stat)

Never block the async runtime directly. Spawn it:

```rust
let result = tokio::task::spawn_blocking(move || fetch_stuff()).await;
```

See `weather_data.rs` for the full pattern including cache-then-refresh.

### Requesting focus (event-driven jumps)

If your provider should grab the screen when something happens (like MPRIS2
does on play/pause), you were given a `FocusChannel` in `register_callback`.
Keep it, clone it into your stream, and send on the event:

```rust
focus_tx.send(crate::render::scheduler::ProviderWantsFocus)?;
```

Note: while the user has LOCKED the display (tray/hotkey), focus requests
are ignored by design.

### Config schema declaration (GUI integration)

To make your provider configurable from the upcoming GUI/tray suite, declare
its schema (convention being introduced alongside docs/DESIGN.md):

```rust
fn config_schema() -> Vec<crate::config_schema::ConfigField> {
    vec![
        ConfigField::bool("uptime.enabled", "Enabled"),
        ConfigField::int("uptime.priority", "Rotation priority"),
    ]
}
```

The GUI renders forms generically from these declarations — no GUI-side code
needed for third-party providers. (See docs/DESIGN.md.)

## Testing without hardware

Build with the simulator feature to preview frames in a window instead of
pushing to the keyboard:

```bash
cargo build --release --features simulator --no-default-features
```

Or verify layout math by rendering to an offscreen buffer and dumping it —
the weather icons were developed entirely this way before touching hardware.

## Checklist before merging a new provider

- [ ] `register_callback` bails cleanly when disabled
- [ ] All draw calls stay within 128×40
- [ ] `stream()` yields on an interval (never tight-loops)
- [ ] Blocking I/O wrapped in `spawn_blocking`
- [ ] Settings documented here and in the README example config
- [ ] Works when its `[section]` is missing entirely (sane defaults)
