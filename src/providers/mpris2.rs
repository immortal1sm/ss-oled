//! MPRIS2 media provider: now-playing metadata, progress and playback state.
//!
//! Named `mpris2` because that is the name it registers under and the
//! config table it reads (`mpris2.*`). It was called `music.rs`, which made
//! the knowledge graph read it as dead code -- searching the graph for
//! `mpris2` landed on the `apex-mpris2` crate instead of on this file.
//!
//! The MPRIS protocol layer itself lives in the `apex-mpris2` crate; the
//! Windows implementation in `apex-windows`.
use crate::render::{
    display::ContentProvider,
    scheduler::{ContentWrapper, FocusChannel, CONTENT_PROVIDERS},
    text::{ScrollableBuilder, StatefulScrollable},
};
use anyhow::Result;
use async_stream::try_stream;
#[cfg(not(target_os = "windows"))]
use embedded_graphics::prelude::Primitive;
#[cfg(not(target_os = "windows"))]
use embedded_graphics::primitives::{Line, PrimitiveStyle};
use embedded_graphics::{
    geometry::Size, image::Image, pixelcolor::BinaryColor, prelude::Point, Drawable,
};
use futures_core::stream::Stream;
use linkme::distributed_slice;
use log::info;
use tinybmp::Bmp;
use tokio::time;

use crate::mpris_shared;
use crate::mpris_shared::NowPlaying;
use tokio::sync::broadcast;
use apex_music::{AsyncPlayer, Metadata, PlaybackStatus, Progress};
use config::Config;
use embedded_graphics::{
    mono_font::{iso_8859_15, MonoTextStyle},
    text::{renderer::TextRenderer, Baseline, Text},
};
use futures::StreamExt;
use std::{
    convert::TryInto,
    sync::{Arc, LazyLock},
};
use tokio::time::{Duration, MissedTickBehavior};

use apex_hardware::FrameBuffer;
use futures::pin_mut;

static NOTE_ICON: &[u8] = include_bytes!("./../../assets/note.bmp");
static PAUSE_ICON: &[u8] = include_bytes!("./../../assets/pause.bmp");

static PAUSE_BMP: LazyLock<Bmp<'static, BinaryColor>> = LazyLock::new(|| {
    Bmp::<BinaryColor>::from_slice(PAUSE_ICON).expect("Failed to parse BMP for pause icon!")
});

static NOTE_BMP: LazyLock<Bmp<'static, BinaryColor>> = LazyLock::new(|| {
    Bmp::<BinaryColor>::from_slice(NOTE_ICON).expect("Failed to parse BMP for note icon!")
});

#[cfg(target_os = "windows")]
lazy_static! {
// Windows doesn't expose the current progress within the song so we don't draw
// it here TODO: Spice this up?
static ref PLAYER_TEMPLATE: FrameBuffer = FrameBuffer::new();
}

#[cfg(not(target_os = "windows"))]
static PLAYER_TEMPLATE: LazyLock<FrameBuffer> = LazyLock::new(|| {
    let mut base = FrameBuffer::new();
    let style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    let points = [
        (Point::new(0, 39), Point::new(127, 39)),
        (Point::new(0, 39), Point::new(0, 39 - 5)),
        (Point::new(127, 39), Point::new(127, 39 - 5)),
    ];

    // Draw a border for the progress bar
    points
        .iter()
        .try_for_each(|(first, second)| {
            Line::new(*first, *second)
                .into_styled(style)
                .draw(&mut base)
        })
        .expect("Failed to prepare template image for music player!");

    base
});

static PLAY_TEMPLATE: LazyLock<FrameBuffer> = LazyLock::new(|| {
    let mut base = *PLAYER_TEMPLATE;
    Image::new(&*NOTE_BMP, Point::new(5, 5))
        .draw(&mut base)
        .expect("Failed to prepare 'play' template for music player");
    base
});

static PAUSE_TEMPLATE: LazyLock<FrameBuffer> = LazyLock::new(|| {
    let mut base = *PLAYER_TEMPLATE;
    Image::new(&*PAUSE_BMP, Point::new(5, 5))
        .draw(&mut base)
        .expect("Failed to prepare 'pause' template for music player");
    base
});

static IDLE_TEMPLATE: LazyLock<FrameBuffer> = LazyLock::new(|| {
    let mut base = *PAUSE_TEMPLATE;
    let style = MonoTextStyle::new(&iso_8859_15::FONT_6X10, BinaryColor::On);
    Text::with_baseline(
        "No player found",
        Point::new(5 + 3 + 24, 3),
        style,
        Baseline::Top,
    )
    .draw(&mut base)
    .expect("Failed to prepare 'idle' template for music player");
    base
});

static UNKNOWN_TITLE: &str = "Unknown title";
static UNKNOWN_ARTIST: &str = "Unknown artist";


/// Format a microsecond count (as returned by MPRIS `Position` and
/// `mpris:length`) as `M:SS`. Returns `"0:00"` for zero or negative values.
fn format_mmss(us: u64) -> String {
    let total_secs = us / 1_000_000;
    let minutes = total_secs / 60;
    let seconds = total_secs % 60;
    format!("{minutes}:{seconds:02}")
}

#[distributed_slice(CONTENT_PROVIDERS)]
static PROVIDER_INIT: fn(&Config, FocusChannel) -> Result<Box<dyn ContentWrapper>> =
    register_callback;

#[allow(clippy::unnecessary_wraps)]
fn register_callback(config: &Config, focus_tx: FocusChannel) -> Result<Box<dyn ContentWrapper>> {
    // From the process-wide owner, not a fresh connection: this is the whole
    // point of sharing. The registry signature is fixed for all providers, so
    // the handle is fetched here rather than passed in.
    let mpris = mpris_shared::shared().clone();

    info!("Registering MPRIS2 display source.");

    let event_focus = config.get_bool("mpris2.event_focus").unwrap_or(true);
    let show_timer = config.get_bool("mpris2.show_timer").unwrap_or(true);
    let show_source_label = config.get_bool("mpris2.show_source_label").unwrap_or(true);

    let player = match config.get_str("mpris2.preferred_player") {
        Ok(name) => MediaPlayerBuilder::new(mpris.clone())
            .with_player_name(name)
            .with_event_focus(event_focus)
            .with_display_options(show_timer, show_source_label)
            .with_focus_tx(focus_tx),
        Err(_) => MediaPlayerBuilder::new(mpris)
            .with_event_focus(event_focus)
            .with_display_options(show_timer, show_source_label)
            .with_focus_tx(focus_tx),
    };

    info!(
        "MPRIS2 options: event_focus={} show_timer={} show_source_label={}",
        event_focus, show_timer, show_source_label
    );

    Ok(Box::new(player))
}

// No `Default`: `MprisShared` deliberately has none, because a handle built by
// hand would never receive a publish. The owner is the only constructor, which
// is what guarantees every consumer sees the same connection.
#[derive(Clone)]
pub struct MediaPlayerBuilder {
    /// If a preference for the player is wanted specify this field
    name: Option<Arc<String>>,
    /// Channel used to request the scheduler switch focus to this provider
    /// when a track changes or playback resumes.
    focus_tx: Option<FocusChannel>,
    /// Whether MPRIS events pull the screen to this provider. Set from
    /// `mpris2.event_focus` in settings.toml.
    event_focus: bool,
    /// Show elapsed/total timer row (mpris2.show_timer).
    show_timer: bool,
    /// Show media source label (mpris2.show_source_label).
    show_source_label: bool,
    /// Process-wide shared MPRIS state. This provider does not open its own
    /// DBus connection; it reads snapshots published by the owner in
    /// `mpris_shared`, so a player restart is recovered from exactly once for
    /// both MPRIS consumers.
    mpris: mpris_shared::MprisShared,
}

// Ok so the plan for the MPRIS2 module is to wait for two DBUS events
// - PropertiesChanged to see if the song changed
// - Seeked to see if the progress was changed manually
// There's an existing mpris2 crate but it doesn't support async operation which
// is kind of painful to use in this architecture.
// When we received these events they should be mapped and put into another
// queue. Upon receiving the event our code should pull the metadata from the
// player.

#[derive(Debug, Clone)]
pub struct MediaPlayerRenderer {
    artist: StatefulScrollable,
    title: StatefulScrollable,
    /// Human-readable media source label (e.g. "Firefox", "Spotify"),
    /// drawn bottom-right of the timer row.
    source: Option<String>,
    /// Show elapsed/total timer row (mpris2.show_timer).
    show_timer: bool,
    /// Show media source label (mpris2.show_source_label).
    show_source_label: bool,
    /// Position (microseconds) at the moment status last transitioned
    /// out of Playing. Firefox's MPRIS plugin keeps reporting a
    /// monotonically-increasing position even when paused/stopped, which
    /// would otherwise let the elapsed timer tick up while the player is
    /// idle. We snapshot position on each Playing read and freeze it
    /// when status changes to Paused/Stopped.
    last_playing_position: i64,
}

/// Map a raw MPRIS bus name (e.g.
/// "org.mpris.MediaPlayer2.firefox.instance_1_367") to a short friendly label
/// for the OLED.
fn friendly_source(bus_name: &str) -> String {
    let stripped = bus_name
        .trim_start_matches("org.mpris.MediaPlayer2.")
        .split('.')
        .next()
        .unwrap_or(bus_name)
        .to_lowercase();

    // Title-case common players so they read nicely on the tiny screen.
    let label = match stripped.as_str() {
        "firefox" => "Firefox",
        "chromium" => "Chromium",
        "chrome" | "googlechrome" => "Chrome",
        "spotify" => "Spotify",
        "vlc" => "VLC",
        "mpv" => "mpv",
        "plasma-browser-integration" | "plasma_browser_integration" => "Browser",
        "kdeconnect" => "Phone",
        "lollypop" => "Lollypop",
        "rhythmbox" => "Rhythmbox",
        "clementine" => "Clementine",
        "strawberry" => "Strawberry",
        "audacious" => "Audacious",
        "cmus" => "cmus",
        other => other,
    };
    label.to_string()
}

impl MediaPlayerRenderer {
    fn new() -> Result<Self> {
        // Layout (y coordinates, top-down):
        //   y=3  ..13   title (FONT_6X10, 10px tall)
        //   y=13 ..16   3px gap — clearly separates title from artist
        //   y=16 ..26   artist (FONT_6X10, 10px tall)
        //   y=26 ..27   1px gap
        //   y=27 ..33   timer row (FONT_4X6, 6px tall, drawn by update())
        //   y=34 ..39   progress bar template (5px tall, with 1px line at y=39)
        // The icon is at x=5..13, so text columns start at x=5+3+24=32.
        let artist = ScrollableBuilder::new()
            .with_text(UNKNOWN_ARTIST)
            .with_custom_spacing(10)
            .with_position(Point::new(5 + 3 + 24, 16))
            .with_projection(Size::new(16 * 6, 10));
        let title = ScrollableBuilder::new()
            .with_text(UNKNOWN_TITLE)
            .with_custom_spacing(10)
            .with_position(Point::new(5 + 3 + 24, 3))
            .with_projection(Size::new(16 * 6, 10));

        Ok(Self {
            artist: artist.try_into()?,
            title: title.try_into()?,
            source: None,
            show_timer: true,
            show_source_label: true,
            last_playing_position: 0,
        })
    }

    /// Toggle visibility options from config.
    pub fn set_display_options(&mut self, show_timer: bool, show_source_label: bool) {
        self.show_timer = show_timer;
        self.show_source_label = show_source_label;
    }

    /// Set the media source label shown bottom-right of the timer row.
    /// Pass an empty string to hide it.
    pub fn set_source(&mut self, bus_name: &str) {
        let label = friendly_source(bus_name);
        if label.is_empty() {
            self.source = None;
        } else {
            self.source = Some(label);
        }
    }

    /// Render from the shared MPRIS snapshot.
    ///
    /// Takes `&NowPlaying` rather than `&Progress<T>`: the renderer has no
    /// business knowing the DBus shape. It reads five fields (title, artists,
    /// length, position, status) and the shared module already flattened them,
    /// so this drops the last MPRIS type out of the render path entirely.
    pub fn update(&mut self, np: &NowPlaying) -> Result<FrameBuffer> {
        let mut display = match np.status {
            PlaybackStatus::Playing => *PLAY_TEMPLATE,
            PlaybackStatus::Paused | PlaybackStatus::Stopped => *PAUSE_TEMPLATE,
        };

        #[cfg(not(target_os = "windows"))]
        {
            // ----- progress bar -----
            // Snapshot position when actively Playing. When Paused/Stopped,
            // freeze the bar at the last Playing position so Firefox's
            // stale monotonic position doesn't drift the elapsed display.
            match np.status {
                PlaybackStatus::Playing => self.last_playing_position = np.position_us,
                PlaybackStatus::Paused => {}
                PlaybackStatus::Stopped => self.last_playing_position = 0,
            }
            let length = np.length_us as f64;
            let current = self.last_playing_position.max(0) as f64;

            let completion = if length > 0.0 {
                (current / length).clamp(0_f64, 1_f64)
            } else {
                0_f64
            };

            let pixels = (128_f64 - 2_f64 * 3_f64) * completion;
            let style = PrimitiveStyle::with_stroke(BinaryColor::On, 3);
            Line::new(Point::new(3, 35), Point::new(pixels as i32 + 3, 35))
                .into_styled(style)
                .draw(&mut display)?;

            // ----- timer row -----
            // Positioned at y=27..33 — 1px below artist (y=16..26) and 1px
            // above progress bar bracket (y=34..39).
            //
            // When playback status is Stopped, force elapsed to 0 so the
            // timer doesn't keep ticking up using stale position data. Some
            // players (Firefox in particular) keep reporting the last-known
            // position even when the player is fully stopped, which would
            // otherwise show the timer advancing on a stopped track.
            let elapsed_us = match np.status {
                PlaybackStatus::Stopped => 0,
                _ => self.last_playing_position.max(0) as u64,
            };
            let total_us = np.length_us.max(0) as u64;
            let timer_text = if total_us > 0 {
                format!("{} / {}", format_mmss(elapsed_us), format_mmss(total_us))
            } else {
                // Some players (Telegram, certain browser tabs) don't publish
                // a track length. Show just the elapsed time.
                format_mmss(elapsed_us)
            };
            if self.show_timer {
                let timer_style = MonoTextStyle::new(&iso_8859_15::FONT_4X6, BinaryColor::On);
                let metrics = timer_style.measure_string(&timer_text, Point::zero(), Baseline::Top);
                let text_width = metrics.bounding_box.size.width as i32;
                let timer_x = (128 - text_width) / 2;
                Text::with_baseline(
                    &timer_text,
                    Point::new(timer_x, 27),
                    timer_style,
                    Baseline::Top,
                )
                .draw(&mut display)?;
            }
        }

        // ----- media source label (top-left corner) -----
        // FONT_4X6 (4px tall) at y=0 sits in the 3px gap above the title
        // row, so it doesn't collide with the timer at y=27 or the title
        // at y=3..13. Labels up to ~16 chars fit; longer ones are truncated
        // at the right edge of the panel.
        if self.show_source_label {
            if let Some(src) = &self.source {
                if !src.is_empty() {
                    let src_style = MonoTextStyle::new(&iso_8859_15::FONT_4X6, BinaryColor::On);
                    Text::with_baseline(src, Point::new(0, 0), src_style, Baseline::Top)
                        .draw(&mut display)?;
                }
            }
        }

        let artists = np.artist.clone();
        let title = np.title.clone();

        // Some sources (YouTube via plasma-browser-integration, monochrome.tf,
        // and many other web players) publish the full "title - artist" or
        // "title • artist" string in xesam:title and leave xesam:artist
        // empty. When that happens, split the combined string so the two
        // OLED rows each get a sensible value instead of an empty artist row.
        let (title, artists) = Self::split_combined_metadata(&title, &artists);

        if let Ok(false) = self.artist.update(&artists) {
            if artists.len() > 16 {
                self.artist.text.scroll();
            }
        }

        if let Ok(false) = self.title.update(&title) {
            if title.len() > 16 {
                self.title.text.scroll();
            }
        }

        self.title.text.draw(&mut display)?;
        self.artist.text.draw(&mut display)?;

        Ok(display)
    }
}

impl MediaPlayerRenderer {
    /// If the artist field is blank but the title embeds the artist after a
    /// separator, split them apart. Handles the common web-player patterns:
    ///   "Song Title - Artist Name"
    ///   "Song Title – Artist Name"  (en dash)
    ///   "Song Title • Artist Name"
    ///   "Artist Name - Song Title"  (reversed: artist first)
    ///
    /// Only splits when xesam:artist is empty/whitespace — real metadata is
    /// trusted as-is. Returns (title_for_row1, artist_for_row2).
    fn split_combined_metadata(title: &str, artist: &str) -> (String, String) {
        if !artist.trim().is_empty() {
            // Real artist data exists; trust it.
            return (title.to_string(), artist.to_string());
        }

        let t = title.trim();
        if t.is_empty() {
            return (t.to_string(), String::new());
        }

        // Separator candidates, in priority order. The bullet is checked
        // first because it's unambiguous; dashes are ambiguous with song
        // titles containing dashes ("Smells Like Teen Spirit - ..."), so we
        // take the LAST dash occurrence for those, which matches how
        // "Title - Artist" is conventionally written.
        const BULLETS: [&str; 3] = ["•", "·", "‧"];
        for sep in BULLETS {
            if let Some(pos) = t.find(sep) {
                let title_part = t[..pos].trim();
                let artist_part = t[pos + sep.len()..].trim();
                if !title_part.is_empty() && !artist_part.is_empty() {
                    return (title_part.to_string(), artist_part.to_string());
                }
            }
        }

        for sep in [" - ", " – "] {
            if let Some(pos) = t.rfind(sep) {
                let title_part = t[..pos].trim();
                let artist_part = t[pos + sep.len()..].trim();
                if !title_part.is_empty() && !artist_part.is_empty() {
                    return (title_part.to_string(), artist_part.to_string());
                }
            }
        }

        // Nothing separable — show the title as-is on row 1.
        (t.to_string(), String::new())
    }
}

impl MediaPlayerBuilder {
    pub fn with_player_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(Arc::new(name.into()));
        self
    }

    pub fn with_focus_tx(mut self, tx: FocusChannel) -> Self {
        self.focus_tx = Some(tx);
        self
    }

    pub fn with_event_focus(mut self, enabled: bool) -> Self {
        self.event_focus = enabled;
        self
    }

    pub fn with_display_options(mut self, show_timer: bool, show_source_label: bool) -> Self {
        self.show_timer = show_timer;
        self.show_source_label = show_source_label;
        self
    }

    pub fn new(mpris: mpris_shared::MprisShared) -> Self {
        Self {
            name: None,
            focus_tx: None,
            event_focus: true,
            show_timer: true,
            show_source_label: true,
            mpris,
        }
    }
}

impl ContentProvider for MediaPlayerBuilder {
    type ContentStream<'a> = impl Stream<Item = Result<FrameBuffer>> + 'a;

    fn stream(&mut self) -> Result<<Self as ContentProvider>::ContentStream<'_>> {
        info!(
            "Trying to connect to DBUS with player preference: {:?}",
            self.name
        );

        let mut renderer = MediaPlayerRenderer::new()?;
        renderer.set_display_options(self.show_timer, self.show_source_label);
        let event_focus = self.event_focus;
        let focus_tx = self
            .focus_tx
            .clone()
            .expect("focus_tx must be set in register_callback");

        Ok(try_stream! {
            // Shared MPRIS state, one connection for the whole process. The
            // Linux path no longer opens its own DBus handle, so this provider
            // cannot hold a second, independently-dropping view of the same
            // player.
            #[cfg(all(feature = "dbus-support", target_os = "linux"))]
            let mpris = self.mpris.clone();

            // Focus on a track change needs MPRIS *events*, which a `watch`
            // cannot carry: they are not a value, and a late subscriber must
            // not replay a stale one. Those ride the same owner as a broadcast.
            #[cfg(all(feature = "dbus-support", target_os = "linux"))]
            let mut events = mpris.subscribe_events();

            let mut renderer = MediaPlayerRenderer::new()?;
            renderer.set_display_options(self.show_timer, self.show_source_label);
            let event_focus = self.event_focus;
            let focus_tx = self
                .focus_tx
                .clone()
                .expect("focus_tx must be set in register_callback");

            info!("Trying to connect to DBUS with player preference: {:?}", self.name);
            yield *IDLE_TEMPLATE;

            // Tick: re-read position periodically so the timer advances while
            // Playing. Events from the shared owner still drive immediate
            // refreshes (PropertiesChanged / Seeked). Tick rate: 1s when actively
            // Playing, 5s otherwise (no need to poll a stopped player more often
            // than reconnects).
            let mut poll = time::interval(Duration::from_millis(1000));
            poll.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                // Wait for either a real MPRIS event or the polling tick.
                #[cfg(all(feature = "dbus-support", target_os = "linux"))]
                let event = match events.as_mut() {
                    Some(rx) => tokio::select! {
                        e = rx.recv() => Some(e),
                        _ = poll.tick() => None,
                    },
                    // Owner not up yet: poll alone rather than dropping the
                    // provider.
                    None => {
                        poll.tick().await;
                        None
                    }
                };
                #[cfg(not(all(feature = "dbus-support", target_os = "linux")))]
                {
                    poll.tick().await;
                }
                #[cfg(not(all(feature = "dbus-support", target_os = "linux")))]
                let event: Option<Result<apex_music::PlayerEvent, tokio::sync::broadcast::error::RecvError>> = None;

                let event = match event {
                    Some(Ok(e)) => Some(e),
                    // `Lagged`: we fell behind. Not fatal -- the snapshot is
                    // authoritative, so resync instead of replaying.
                    Some(Err(broadcast::error::RecvError::Lagged(_))) => None,
                    // `Closed`: the owner is gone. Keep drawing the idle frame.
                    Some(Err(broadcast::error::RecvError::Closed)) => {
                        yield *IDLE_TEMPLATE;
                        continue;
                    }
                    None => None,
                };
                // Read before `event` is consumed below.
                let got_event = event.is_some();

                // One read of "what is playing", from whichever source this
                // platform uses. Linux shares it; Windows still owns its own.
                #[cfg(all(feature = "dbus-support", target_os = "linux"))]
                let np = mpris.now();
                #[cfg(not(all(feature = "dbus-support", target_os = "linux")))]
                let np = {
                    let player = apex_windows::Player::new()?;
                    let np = flatten_windows_player(&player).await;
                    Some(np)
                };

                let Some(np) = np else {
                    // Shared connection down, or nothing playing. Draw idle and
                    // wait; one reconnect restores this along with every other
                    // MPRIS consumer.
                    yield *IDLE_TEMPLATE;
                    continue;
                };

                // Capture the media source so update() can render it
                // bottom-right of the timer row.
                if !np.player.is_empty() {
                    renderer.set_source(&np.player);
                }

                if let Some(event) = event {
                    log::debug!("MPRIS event: {:?}", event);
                    // React to MPRIS events. We fire focus on PropertiesChanged
                    // AND Seeked. The latter catches cases where Firefox
                    // publishes Seeked without a corresponding PropertiesChanged
                    // (rare but happens). Timer events don't fire focus.
                    if matches!(
                        event,
                        apex_music::PlayerEvent::Properties | apex_music::PlayerEvent::Seeked
                    ) {
                        // Honor mpris2.event_focus: when disabled, media state
                        // changes still re-render this screen if it's shown, but
                        // do NOT steal focus from another provider.
                        if event_focus {
                            log::info!("MPRIS event fired: {:?}", event);
                            let send_result =
                                focus_tx.send(crate::render::scheduler::ProviderWantsFocus);
                            log::info!("focus_tx.send result: {:?}", send_result);
                        } else {
                            log::debug!("MPRIS event (event_focus off): {:?}", event);
                        }
                    }
                }

                // On the polling path only repaint while actively playing, so a
                // paused/stopped player stays frozen instead of churning pixels.
                if got_event || matches!(np.status, PlaybackStatus::Playing) {
                    if let Ok(image) = renderer.update(&np) {
                        yield image;
                    }
                }
            }
        })
    }

    fn name(&self) -> &'static str {
        "mpris2"
    }
}
