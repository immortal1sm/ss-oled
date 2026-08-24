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

const RECONNECT_DELAY: u64 = 5;

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
    info!("Registering MPRIS2 display source.");

    let player = match config.get_str("mpris2.preferred_player") {
        Ok(name) => MediaPlayerBuilder::new()
            .with_player_name(name)
            .with_focus_tx(focus_tx),
        Err(_) => MediaPlayerBuilder::new().with_focus_tx(focus_tx),
    };

    Ok(Box::new(player))
}

#[derive(Debug, Clone, Default)]
pub struct MediaPlayerBuilder {
    /// If a preference for the player is wanted specify this field
    name: Option<Arc<String>>,
    /// Channel used to request the scheduler switch focus to this provider
    /// when a track changes or playback resumes.
    focus_tx: Option<FocusChannel>,
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
        })
    }

    pub fn update<T: Metadata>(&mut self, progress: &Progress<T>) -> Result<FrameBuffer> {
        let mut display = match progress.status {
            PlaybackStatus::Playing => *PLAY_TEMPLATE,
            PlaybackStatus::Paused | PlaybackStatus::Stopped => *PAUSE_TEMPLATE,
        };

        let metadata = &progress.metadata;

        #[cfg(not(target_os = "windows"))]
        {
            // ----- progress bar (unchanged position) -----
            let length = metadata.length().unwrap_or(0) as f64;

            let current = progress.position.max(0) as f64;

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
            let elapsed_us = match progress.status {
                PlaybackStatus::Stopped => 0,
                _ => progress.position.max(0) as u64,
            };
            let total_us = metadata.length().unwrap_or(0);
            let timer_text = if total_us > 0 {
                format!("{} / {}", format_mmss(elapsed_us), format_mmss(total_us))
            } else {
                // Some players (Telegram, certain browser tabs) don't publish
                // a track length. Show just the elapsed time.
                format_mmss(elapsed_us)
            };
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

        let artists = metadata.artists()?;
        let title = metadata.title()?;

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

impl MediaPlayerBuilder {
    pub fn with_player_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(Arc::new(name.into()));
        self
    }

    pub fn with_focus_tx(mut self, tx: FocusChannel) -> Self {
        self.focus_tx = Some(tx);
        self
    }

    pub fn new() -> Self {
        Self::default()
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
        let focus_tx = self
            .focus_tx
            .clone()
            .expect("focus_tx must be set in register_callback");

        Ok(try_stream! {
            #[cfg(target_os = "windows")]
            let mpris = apex_windows::Player::new()?;
            #[cfg(target_os = "linux")]
            let mpris = apex_mpris2::MPRIS2::new().await?;
            pin_mut!(mpris);

            let mut interval = time::interval(Duration::from_secs(RECONNECT_DELAY));
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            'outer: loop {
                info!(
                    "Trying to connect to DBUS with player preference: {:?}",
                    self.name
                );
                yield *IDLE_TEMPLATE;
                #[cfg(target_os = "windows")]
                let player = &mpris;
                #[cfg(target_os = "linux")]
                let player = mpris.wait_for_player(self.name.clone()).await?;

                info!("Connected to music player: {:?}", player.name().await);


                let tracker = mpris.stream().await?;
                pin_mut!(tracker);

                while let Some(event) = tracker.next().await {
                    log::debug!("MPRIS event: {:?}", event);
                    // React to MPRIS events. Only fire focus on transitions INTO
                    // Playing — pause and stop do NOT pull focus (the scheduler
                    // keeps whatever provider was active and rotates normally).
                    if matches!(event, apex_music::PlayerEvent::Properties) {
                        log::info!("MPRIS PropertiesChanged received");
                        // PropertiesChanged covers track-change AND playback-status
                        // changes. Check the current status and only fire if we're
                        // transitioning into Playing.
                        match player.playback_status().await {
                            Ok(PlaybackStatus::Playing) => {
                                log::info!("Status is Playing — sending focus request");
                                let send_result = focus_tx.send(
                                    crate::render::scheduler::ProviderWantsFocus
                                );
                                log::info!("focus_tx.send result: {:?}", send_result);
                            }
                            Ok(other) => {
                                log::info!("Status is {:?} — skipping focus", other);
                            }
                            Err(e) => {
                                log::warn!("Could not get playback_status: {}", e);
                            }
                        }
                    }

                    if let Ok(progress) = player.progress().await {
                        if let Ok(image) = renderer.update(&progress) {
                            yield image;
                        }
                    } else {
                        continue 'outer;
                    }
                }
            }
        })
    }

    fn name(&self) -> &'static str {
        "mpris2"
    }
}
