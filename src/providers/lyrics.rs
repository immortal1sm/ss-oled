//! Lyrics provider — synchronized lyrics for the currently playing track.
//!
//! Reads track metadata from MPRIS (via `apex-mpris2`), fetches LRC lyrics, and
//! renders the line active at the current playback position.
//!
//! ```toml
//! [providers.lyrics]
//! enabled = true
//! priority = 5
//! interval = 4               # seconds on screen per rotation
//! source = "auto"            # auto | local | lrclib
//! font = "auto"              # auto | S | M | L | XL
//! align = "L"                # L | C | R
//! bold = false
//! show_title = false         # draw the track title above the lyric
//! ```
//!
//! Lyrics are sourced in the same cascade as lyrics-on-panel, minus the
//! player-specific localhost APIs:
//!   1. local `.lrc` sidecar next to a `file://` track
//!   2. lrclib.net — `/api/get` (exact, needs duration), then `/api/search`,
//!      then a title-only fuzzy search
//!
//! Results are cached to `~/.cache/apex-tux/lyrics/` so repeats and restarts
//! don't re-hit lrclib, and so lyrics still show when offline.

use crate::providers::lrc::{current_lyric, parse_lrc, LyricLine};
use crate::render::display::ContentProvider;
use anyhow::Result;
use apex_hardware::FrameBuffer;
use apex_music::{AsyncPlayer, Metadata as MetadataTrait};
use async_stream::try_stream;
use config::Config;
use embedded_graphics::{
    mono_font::{iso_8859_15, MonoTextStyle},
    pixelcolor::BinaryColor,
    prelude::Point,
    text::{Baseline, Text},
    Drawable,
};
use futures::{pin_mut, Stream};
use log::{info, warn};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::{interval, MissedTickBehavior};

const PANEL_W: i32 = 128;
const PANEL_H: i32 = 40;
const TICK_MS: u64 = 250;
/// Matches lyrics-on-panel: short-lived negative cache so a track with no
/// lyrics (or a failed fetch) is retried instead of staying blank.
const NEGATIVE_TTL: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);
const USER_AGENT: &str = concat!("apex-tux/", env!("CARGO_PKG_VERSION"));

/// Marker appended to a line that was cut off.
///
/// Must stay ASCII. The ISO-8859-15 fonts have no glyph for U+2026 (…), so an
/// ellipsis renders as blank space — the overflow becomes invisible, which is
/// exactly the case this marker exists to reveal.
///
/// `>` rather than `?`: lyric text legitimately contains question marks, so a
/// `?` marker is ambiguous with the content. `>` cannot appear here (the parser
/// strips it as a separator) and unambiguously reads as "continues".
const TRUNC_MARKER: char = '>';

/// Longest lyric (in chars) that `auto` will render at XLarge.
///
/// XL's NOMINAL capacity is 16 chars × 3 lines = 48, but wrapping prefers word
/// boundaries, and every early break wastes the remainder of that line. Measured
/// over realistic word text, truncation starts well below the nominal figure:
/// ~0% at 32 chars, ~1% at 36, ~4% at 38, ~13% at 40, 100% at 48. Above this
/// threshold `auto` steps down to Large instead of clipping a big-font line.
const XL_MAX_CHARS: usize = 34;

/// Font size classes. `line_h` follows the custom provider's rules: XLarge gets
/// extra leading because FONT_8X13's descender needs it (char_w + 6, not +2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Size {
    Small,
    Medium,
    Large,
    XLarge,
}

impl Size {
    fn char_w(self) -> i32 {
        match self {
            Size::Small => 4,
            Size::Medium => 5,
            Size::Large => 6,
            Size::XLarge => 8,
        }
    }

    fn line_h(self) -> i32 {
        match self {
            // XL uses the true font height (13px) rather than the custom
            // provider's char_w + 6 = 14 rule. That extra leading exists for
            // dense mixed-row layouts; in a single column it costs a whole
            // line — at 13px three lines fit in 39px of a 40px panel, at 14px
            // only two do.
            Size::XLarge => 13,
            _ => self.char_w() + 2,
        }
    }

    fn style(self) -> MonoTextStyle<'static, BinaryColor> {
        let f = match self {
            Size::Small => &iso_8859_15::FONT_4X6,
            Size::Medium => &iso_8859_15::FONT_5X7,
            Size::Large => &iso_8859_15::FONT_6X10,
            Size::XLarge => &iso_8859_15::FONT_8X13,
        };
        MonoTextStyle::new(f, BinaryColor::On)
    }

    /// Descending ladder used by `font = "auto"`, biggest first.
    ///
    /// Floors at Large. When nothing on the ladder can hold the lyric, we
    /// render at Large and truncate with an ellipsis rather than dropping to
    /// Medium or Small — on a 128x40 OLED the smaller sizes are hard to read,
    /// and a truncated line at a legible size beats a cramped full one.
    fn ladder() -> [Size; 2] {
        [Size::XLarge, Size::Large]
    }

    /// Lines that fit on a panel with `reserved_top` px already used.
    fn max_lines(self, reserved_top: i32) -> usize {
        let avail = PANEL_H - reserved_top;
        if avail <= 0 {
            0
        } else {
            (avail / self.line_h()).max(0) as usize
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Source {
    /// local sidecar first, then lrclib
    Auto,
    /// only a `.lrc` next to the track file
    Local,
    /// only lrclib
    Lrclib,
}

pub struct LyricsProvider {
    source: Source,
    size: Option<Size>,
    align: Align,
    bold: bool,
    show_title: bool,
    /// Parsed lyrics for the current track, shared with the fetch task.
    lyrics: Arc<Mutex<LyricsState>>,
}

#[derive(Default)]
struct LyricsState {
    lines: Vec<LyricLine>,
    /// Track identity the cached `lines` belong to.
    key: String,
    title: String,
    /// Set when a fetch completed with no lyrics; blocks refetch for NEGATIVE_TTL.
    missing_since: Option<std::time::Instant>,
}

/// Render a possibly-wrapped text block, honouring alignment and the panel
/// bottom edge. `y` is the first line's top; `line_budget` caps how many lines
/// this block may use (callers reserve vertical space for other rows).
#[allow(clippy::too_many_arguments)]
fn draw_block(
    buffer: &mut FrameBuffer,
    text: &str,
    y: i32,
    size: Size,
    align: Align,
    bold: bool,
    line_budget: usize,
) {
    let style = size.style();
    let char_w = size.char_w();
    let line_h = size.line_h();
    let max_chars = (PANEL_W / char_w).max(1) as usize;
    // Never exceed either the caller's budget or the physical panel height.
    let max_lines = line_budget.min(Size::max_lines(size, y)).max(1);

    let chars: Vec<char> = text.chars().collect();
    let mut offset = 0usize;

    for line_idx in 0..max_lines {
        if offset >= chars.len() {
            break;
        }
        let y_pos = y + (line_idx as i32) * line_h;
        if y_pos + line_h > PANEL_H + size.char_w() {
            break;
        }

        // Take the next chunk, preferring a word boundary.
        let remaining = chars.len() - offset;
        // `broke_on_space` records that `take` landed ON a delimiter. That
        // space has already been used as the split point, so the next line
        // must start AFTER it — otherwise the continuation begins with a
        // leading space and sits one column right of the line above it.
        let mut broke_on_space = false;
        let take = if remaining <= max_chars {
            remaining
        } else {
            let window: String = chars[offset..offset + max_chars].iter().collect();
            match window.rfind(' ') {
                Some(pos) if pos > 0 => {
                    broke_on_space = true;
                    pos
                }
                // No space: hard split so we never loop forever.
                _ => max_chars,
            }
        };
        let mut chunk: String = chars[offset..offset + take].iter().collect();
        offset += take;
        if broke_on_space {
            // Consume exactly one delimiter, and never run past the end.
            while offset < chars.len() && chars[offset] == ' ' {
                offset += 1;
            }
        }
        let had_more = offset < chars.len();

        if had_more && line_idx + 1 == max_lines {
            // Mark truncation with '?' — the ISO-8859-15 fonts have no glyph for
            // U+2026 (…), so an ellipsis renders blank. '?' is ASCII and always
            // present, so the overflow marker is actually visible on the panel.
            chunk = format!(
                "{}{TRUNC_MARKER}",
                chunk.chars().take(max_chars.saturating_sub(1)).collect::<String>()
            );
            draw_line(buffer, &chunk, y_pos, size, align, bold, style);
            break;
        }

        if !chunk.is_empty() {
            draw_line(buffer, &chunk, y_pos, size, align, bold, style);
        }
    }
}

/// Draw a single line, clipping to the panel and marking any overflow with
/// `TRUNC_MARKER`.
///
/// `draw_line` itself does not clip: text wider than the panel simply runs off
/// the right edge with no indication. Every caller must size its text first, so
/// the clipping lives here where it cannot be forgotten.
fn draw_line(
    buffer: &mut FrameBuffer,
    text: &str,
    y: i32,
    size: Size,
    align: Align,
    bold: bool,
    style: MonoTextStyle<'static, BinaryColor>,
) {
    let char_w = size.char_w();
    let max_chars = (PANEL_W / char_w).max(1) as usize;

    let mut owned;
    let text = if text.chars().count() > max_chars {
        // Reserve the last column for the marker so the overflow is visible.
        owned = format!(
            "{}{TRUNC_MARKER}",
            text.chars().take(max_chars.saturating_sub(1)).collect::<String>()
        );
        owned.as_str()
    } else {
        text
    };

    let width = text.chars().count() as i32 * char_w;
    let x = match align {
        Align::Left => 0,
        Align::Center => (PANEL_W - width) / 2,
        Align::Right => PANEL_W - width,
    };
    let pos = Point::new(x.max(0), y);
    // Drawing into a FrameBuffer is infallible in practice; ignore per-glyph
    // results so the render helpers stay plain `fn` instead of `Result`.
    let _ = Text::with_baseline(text, pos, style, Baseline::Top).draw(buffer);
    if bold {
        let _ = Text::with_baseline(
            text,
            Point::new(pos.x + 1, pos.y),
            style,
            Baseline::Top,
        )
        .draw(buffer);
    }
}

/// Largest size at which `text` fits within `allow_lines` lines.
///
/// Falls back to the smallest ladder entry when nothing fits; `draw_block`
/// then wraps and truncates. Never returns `Small` — the ladder stops at
/// Medium on purpose.
fn pick_size(text: &str, reserved_top: i32, allow_lines: usize) -> Size {
    let allow = allow_lines.max(1);
    let text_len = text.chars().count();
    let ladder = Size::ladder();

    for size in ladder {
        // `allow` is a budget, but a size can never exceed the lines the panel
        // actually affords it. Capping here is what makes a 60-char lyric step
        // down to Large instead of being truncated at XLarge: XL only gets
        // 40/13 = 3 lines regardless of the budget.
        let usable = allow.min(Size::max_lines(size, reserved_top));
        if usable == 0 {
            continue;
        }

        // XLarge is special: its nominal capacity overstates what word-wrap
        // can actually fit, because each early break wastes the rest of the
        // line. Use the measured threshold instead of chars × lines.
        let capacity = if size == Size::XLarge {
            XL_MAX_CHARS
        } else {
            (PANEL_W / size.char_w()) as usize * usable
        };

        if text_len <= capacity {
            return size;
        }
    }
    ladder[ladder.len() - 1]
}

fn cache_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".cache")
        });
    base.join("apex-tux").join("lyrics")
}

/// Filesystem-safe cache filename for a track.
fn cache_path(key: &str) -> PathBuf {
    let safe: String = key
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let mut s: String = safe.chars().take(120).collect();
    if s.is_empty() {
        s = "unknown".to_string();
    }
    cache_dir().join(format!("{s}.lrc"))
}

/// Identity of a track for cache/fetch purposes.
fn track_key(title: &str, artist: &str, album: &str) -> String {
    format!("{}|{}|{}", title.trim(), artist.trim(), album.trim())
}

/// Try a `.lrc` sidecar next to a local track file.
fn fetch_local(url: &str) -> Option<String> {
    let path = url.strip_prefix("file://")?;
    let decoded = percent_decode(path);
    let mut p = PathBuf::from(decoded);
    p.set_extension("lrc");
    std::fs::read_to_string(p).ok()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn http_get(url: &str) -> Option<String> {
    ureq::get(url)
        .timeout(HTTP_TIMEOUT)
        .set("User-Agent", USER_AGENT)
        .call()
        .ok()?
        .into_string()
        .ok()
}

/// Query lrclib. Mirrors lyrics-on-panel: exact match with duration first,
/// then a metadata search, then a title-only fuzzy search.
fn fetch_lrclib(title: &str, artist: &str, album: &str, duration_us: i64) -> Option<String> {
    let enc = |s: &str| {
        s.chars()
            .map(|c| match c {
                'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
                ' ' => "+".to_string(),
                _ => format!("%{:02X}", c as u32),
            })
            .collect::<String>()
    };

    let dur_secs = if duration_us > 0 {
        Some(duration_us / 1_000_000)
    } else {
        None
    };

    if let Some(dur) = dur_secs {
        let url = format!(
            "https://lrclib.net/api/get?track_name={}&artist_name={}&album_name={}&duration={}",
            enc(title),
            enc(artist),
            enc(album),
            dur
        );
        if let Some(body) = http_get(&url) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                if let Some(s) = v.get("syncedLyrics").and_then(|x| x.as_str()) {
                    if !s.trim().is_empty() {
                        return Some(s.to_string());
                    }
                }
            }
        }
    }

    let search_url = format!(
        "https://lrclib.net/api/search?track_name={}&artist_name={}&album_name={}",
        enc(title),
        enc(artist),
        enc(album)
    );
    if let Some(body) = http_get(&search_url) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
            if let Some(arr) = v.as_array() {
                for item in arr {
                    if let Some(s) = item.get("syncedLyrics").and_then(|x| x.as_str()) {
                        if !s.trim().is_empty() {
                            return Some(s.to_string());
                        }
                    }
                }
            }
        }
    }

    // Last resort: title only.
    let fuzzy_url = format!("https://lrclib.net/api/search?q={}", enc(title));
    if let Some(body) = http_get(&fuzzy_url) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
            if let Some(arr) = v.as_array() {
                for item in arr {
                    if let Some(s) = item.get("syncedLyrics").and_then(|x| x.as_str()) {
                        if !s.trim().is_empty() {
                            return Some(s.to_string());
                        }
                    }
                }
            }
        }
    }

    None
}

/// Resolve lyrics for a track: disk cache → source cascade.
fn resolve_lyrics(
    source: Source,
    url: &str,
    title: &str,
    artist: &str,
    album: &str,
    duration_us: i64,
) -> Option<String> {
    let path = cache_path(&track_key(title, artist, album));
    if let Ok(text) = std::fs::read_to_string(&path) {
        return Some(text);
    }

    let fetched = match source {
        Source::Local => fetch_local(url),
        Source::Lrclib => fetch_lrclib(title, artist, album, duration_us),
        Source::Auto => fetch_local(url)
            .or_else(|| fetch_lrclib(title, artist, album, duration_us)),
    }?;

    // Best-effort cache write; failure is not fatal.
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_ok() {
            let _ = std::fs::write(&path, &fetched);
        }
    }

    Some(fetched)
}

/// Lines the track title occupies when `show_title` is on.
///
/// The title renders at XLarge so it reads clearly as a heading. Long titles
/// wrap onto a second line rather than being clipped — but at XL a second line
/// costs 26px of a 40px panel, leaving only 14px (a single Large line) for the
/// lyric. So we reserve exactly what the title actually uses, measured from the
/// title text, and let the lyric size to the remainder.
fn title_lines(title: &str) -> usize {
    let max_chars = (PANEL_W / Size::XLarge.char_w()).max(1) as usize;
    let len = title.chars().count();
    if len <= max_chars {
        1
    } else {
        // Wrap to a second line. Titles longer than 2 lines at XL are clipped
        // by draw_block's own overflow marker.
        2.min(Size::max_lines(Size::XLarge, 0))
    }
}

impl LyricsProvider {
    fn render(
        &self,
        state: &LyricsState,
        position_us: i64,
        _page: u64,
    ) -> Result<FrameBuffer> {
        let mut buffer = FrameBuffer::new();

        let title = state.title.clone();
        let current = current_lyric(&state.lines, position_us);

        // Nothing fetched yet, or genuinely no lyrics for this track.
        let Some(current) = current else {
            let label = if title.is_empty() {
                "LYRICS".to_string()
            } else {
                // Fall back to the track title so the panel isn't blank.
                title
            };
            // Use draw_block, NOT draw_line: draw_line clips a single line and
            // marks the overflow, so a long track title lost everything past
            // the first line ("He Got Banned From C..."). draw_block wraps at
            // word boundaries and paints the following lines, which is what a
            // long title needs.
            const FALLBACK_LINES: usize = 3;
            // Same ladder as the lyric path: take the largest size the whole
            // title fits in within the available lines. A short title lands on
            // XLarge, a long one steps down to Large, rather than being pinned
            // to Large regardless of how well it would fit.
            let size = pick_size(&label, 0, FALLBACK_LINES);
            // Centre on the lines the text actually needs, not the budget,
            // so a one-line title doesn't sit high in a three-line block.
            let max_chars = (PANEL_W / size.char_w()).max(1) as usize;
            let lines = label
                .chars()
                .count()
                .div_ceil(max_chars)
                .clamp(1, FALLBACK_LINES);
            let block_h = size.line_h() * lines as i32;
            let y = ((PANEL_H - block_h) / 2).max(0);
            draw_block(
                &mut buffer,
                &label,
                y,
                size,
                self.align,
                self.bold,
                lines,
            );
            return Ok(buffer);
        };

        // Track title renders at XLarge and wraps to a second line when it does not
        // fit, instead of being clipped. Only the space it actually occupies is
        // reserved, so short titles leave the lyric more room.
        let reserved_top = if self.show_title && !title.is_empty() {
            let lines = title_lines(&title);
            draw_block(
                &mut buffer,
                &title,
                0,
                Size::XLarge,
                self.align,
                self.bold,
                lines,
            );
            (lines as i32) * Size::XLarge.line_h()
        } else {
            0
        };

        // `auto` picks the largest size the lyric fits in, within whatever
        // vertical space is left after the optional title row. No "next
        // line" preview is drawn, so the full remaining height is available.
        let size = match self.size {
            Some(s) => s,
            None => pick_size(&current, reserved_top, Size::max_lines(Size::Small, reserved_top)),
        };

        // The lyric gets whatever vertical space the title left, capped by its own
        // size's line height.
        let lyric_budget = Size::max_lines(size, reserved_top).max(1);
        draw_block(
            &mut buffer,
            &current,
            reserved_top,
            size,
            self.align,
            self.bold,
            lyric_budget,
        );

        Ok(buffer)
    }
}

impl ContentProvider for LyricsProvider {
    type ContentStream<'a>
        where
            Self: 'a,
    = impl Stream<Item = Result<FrameBuffer>> + 'a;

    fn stream(&mut self) -> Result<Self::ContentStream<'_>> {
        info!("Registering lyrics display source.");

        let state = Arc::clone(&self.lyrics);
        let source = self.source;

        Ok(try_stream! {
            #[cfg(target_os = "linux")]
            let mpris = apex_mpris2::MPRIS2::new().await?;
            pin_mut!(mpris);

            let mut tick = interval(Duration::from_millis(TICK_MS));
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                tick.tick().await;

                let (title, artist, album, url, duration_us, position_us) =
                    match resolve_track(&mut mpris).await {
                        Some(t) => t,
                        None => {
                            yield self.render(&state.lock().unwrap(), 0, 0)?;
                            continue;
                        }
                    };

                let key = track_key(&title, &artist, &album);
                let needs_fetch = {
                    let s = state.lock().unwrap();
                    if s.key != key {
                        true
                    } else if let Some(t) = s.missing_since {
                        t.elapsed() >= NEGATIVE_TTL
                    } else {
                        false
                    }
                };

                if needs_fetch {
                    let state = Arc::clone(&state);
                    let key = key.clone();
                    let title_c = title.clone();
                    tokio::task::spawn_blocking(move || {
                        let text = resolve_lyrics(
                            source,
                            &url,
                            &title,
                            &artist,
                            &album,
                            duration_us,
                        );
                        let mut s = state.lock().unwrap();
                        // Only commit if this track is still the current one.
                        if s.key != key {
                            s.lines = text.as_deref().map(parse_lrc).unwrap_or_default();
                            s.key = key.clone();
                            s.title = title_c.clone();
                            s.missing_since = if s.lines.is_empty() {
                                Some(std::time::Instant::now())
                            } else {
                                None
                            };
                            if s.missing_since.is_some() {
                                warn!("no synced lyrics found for '{title_c}'");
                            } else {
                                info!("loaded {} lyric lines for '{title_c}'", s.lines.len());
                            }
                        }
                    });
                }

                yield self.render(&state.lock().unwrap(), position_us, 0)?;
            }
        })
    }

    fn name(&self) -> &'static str {
        "lyrics"
    }
}

#[cfg(target_os = "linux")]
type TrackInfo = (String, String, String, String, i64, i64);

/// Pull title/artist/album/url/length/position from the active MPRIS player.
///
/// `AsyncPlayer::progress()` bundles metadata + position in one round trip.
/// Album and track URL are not exposed by `apex-music` today, so they come
/// back empty — lrclib still matches on title+artist alone, and the
/// local-sidecar tier needs the URL, so it stays inert until that metadata is
/// plumbed through.
#[cfg(target_os = "linux")]
async fn resolve_track(mpris: &mut apex_mpris2::MPRIS2) -> Option<TrackInfo> {
    let player = mpris.wait_for_player(None).await.ok()?;
    let progress = player.progress().await.ok()?;
    let title = progress.metadata.title().ok()?;
    if title.trim().is_empty() {
        return None;
    }
    let artist = progress.metadata.artists().unwrap_or_default();
    let length = progress.metadata.length().unwrap_or(0);
    Some((
        title,
        artist,
        String::new(),
        String::new(),
        length as i64,
        progress.position,
    ))
}

/// Build the provider from `[providers.lyrics]`.
pub fn from_config(config: &Config) -> Result<Option<LyricsProvider>> {
    if !config.get_bool("providers.lyrics.enabled").unwrap_or(false) {
        return Ok(None);
    }

    // `config::Config::get_str` returns an owned String, so unwrap_or must
    // supply a String too.
    let source = match config
        .get_str("providers.lyrics.source")
        .unwrap_or_else(|_| "auto".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "local" => Source::Local,
        "lrclib" => Source::Lrclib,
        _ => Source::Auto,
    };

    let size = match config
        .get_str("providers.lyrics.font")
        .unwrap_or_else(|_| "auto".to_string())
        .to_ascii_uppercase()
        .as_str()
    {
        "S" | "SMALL" => Some(Size::Small),
        "M" | "MEDIUM" => Some(Size::Medium),
        "L" | "LARGE" => Some(Size::Large),
        "XL" | "XLARGE" => Some(Size::XLarge),
        _ => None,
    };

    let align = match config
        .get_str("providers.lyrics.align")
        .unwrap_or_else(|_| "L".to_string())
        .to_ascii_uppercase()
        .as_str()
    {
        "C" | "CENTER" => Align::Center,
        "R" | "RIGHT" => Align::Right,
        _ => Align::Left,
    };

    Ok(Some(LyricsProvider {
        source,
        size,
        align,
        bold: config.get_bool("providers.lyrics.bold").unwrap_or(false),
        show_title: config.get_bool("providers.lyrics.show_title").unwrap_or(false),
        lyrics: Arc::new(Mutex::new(LyricsState::default())),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The no-lyrics fallback must use the same auto ladder as the lyric path:
    /// a short title should reach XLarge, a long one step down to Large.
    #[test]
    fn fallback_ladder_scales_with_title_length() {
        for (title, want) in [
            ("Never Gonna Give You Up", Size::XLarge),
            ("Bohemian Rhapsody", Size::XLarge),
            ("He Got Banned From ChatGPT... - YouTube", Size::Large),
        ] {
            assert_eq!(pick_size(title, 0, 3), want, "unexpected size for {title:?}");
        }
    }

    /// The fallback must show the WHOLE title. Regression: it used to call
    /// draw_line (a single-line clip) and rendered only "He Got Banned From C".
    #[test]
    fn fallback_wraps_rather_than_clips() {
        let title = "He Got Banned From ChatGPT... - YouTube";
        let size = pick_size(title, 0, 3);
        let max_chars = (PANEL_W / size.char_w()).max(1) as usize;
        let lines = title.chars().count().div_ceil(max_chars).clamp(1, 3);
        assert!(
            title.chars().count() <= max_chars * lines,
            "{title:?} does not fit in {lines} line(s) at {:?}",
            size
        );
        assert!(size.line_h() * lines as i32 <= PANEL_H, "block runs off the panel");
    }

    /// Wrapping at a word boundary must not leave the delimiter at the start of
    /// the continuation. Observed on the panel as:
    ///   "He Got Banned From"
    ///   " ChatGPT... - YouTube"      <- leading space, misaligned
    #[test]
    fn wrap_does_not_lead_with_a_space() {
        let text = "He Got Banned From ChatGPT... - YouTube";
        let size = pick_size(text, 0, 3);
        let max_chars = (PANEL_W / size.char_w()).max(1) as usize;

        // Mirror draw_block's chunking.
        let chars: Vec<char> = text.chars().collect();
        let mut offset = 0usize;
        let mut lines: Vec<String> = Vec::new();
        while offset < chars.len() && lines.len() < 3 {
            let remaining = chars.len() - offset;
            let mut broke_on_space = false;
            let take = if remaining <= max_chars {
                remaining
            } else {
                let window: String = chars[offset..offset + max_chars].iter().collect();
                match window.rfind(' ') {
                    Some(pos) if pos > 0 => { broke_on_space = true; pos }
                    _ => max_chars,
                }
            };
            lines.push(chars[offset..offset + take].iter().collect::<String>());
            offset += take;
            if broke_on_space {
                while offset < chars.len() && chars[offset] == ' ' {
                    offset += 1;
                }
            }
        }

        assert_eq!(lines[0], "He Got Banned From", "first line changed");
        assert!(
            !lines[1].starts_with(' '),
            "continuation leads with a space: {:?}",
            lines[1]
        );
        assert_eq!(lines[1], "ChatGPT... - YouTube");
        // Nothing lost: every break replaced exactly one space, so joining the
        // lines back with a single space must reproduce the original.
        assert_eq!(lines.join(" "), text, "characters lost or duplicated in wrapping");
    }

    #[test]
    fn truncation_marker_has_a_real_glyph() {
        // A non-ASCII marker would render blank on the panel, hiding the very
        // thing it is meant to signal.
        assert!(
            (TRUNC_MARKER as u32) < 0x80,
            "truncation marker {TRUNC_MARKER:?} must be ASCII to have a glyph"
        );
        assert_ne!(
            TRUNC_MARKER, '\u{2026}',
            "U+2026 has no glyph in these fonts"
        );
    }

    /// Regression guard: `draw_line` used to do no clipping, so an over-long
    /// title ran off the right panel edge with no overflow marker.
    ///
    /// Observability note: `FrameBuffer` is exactly panel-sized, so drawing
    /// past the edge is silently absorbed by the BitArray — the rightmost ink
    /// column is identical either way, so that CANNOT detect this regression.
    /// The real, observable difference is the number of distinct glyph columns
    /// rendered, which is asserted here by comparing against a reference string
    /// that is exactly the clipped form.
    #[test]
    fn draw_line_clips_overflow_instead_of_running_off() {
        /// Rightmost lit column in the buffer, or None if the buffer is blank.
        fn rightmost_inked_column(buf: &FrameBuffer) -> Option<u32> {
            // FrameBuffer stores bits row-major with an 8-byte header offset;
            // see apex-hardware/src/device.rs.
            let mut rightmost: Option<u32> = None;
            for y in 0..PANEL_H as u32 {
                for x in 0..PANEL_W as u32 {
                    let idx = (x + y * PANEL_W as u32) as usize + 8;
                    if *buf.framebuffer.get(idx).unwrap() {
                        rightmost = Some(match rightmost {
                            Some(prev) => prev.max(x),
                            None => x,
                        });
                    }
                }
            }
            rightmost
        }

        let render = |text: &str, size: Size, align: Align| -> FrameBuffer {
            let mut buf = FrameBuffer::new();
            draw_line(&mut buf, text, 0, size, align, false, size.style());
            buf
        };

        let long_title = "A Really Quite Long Song Title Indeed Yes Far Too Long";
        // Sanity: the fixture must actually exceed the panel width.
        assert!(
            long_title.chars().count() * Size::Small.char_w() as usize > PANEL_W as usize,
            "fixture must be wider than the panel"
        );

        for align in [Align::Left, Align::Center, Align::Right] {
            let buf = render(long_title, Size::Small, align);

            // The clipped result must be pixel-identical to drawing the
            // explicitly-clipped string. This is the assertion that actually
            // detects the regression: pre-fix, draw_line(long_title) would
            // render 32 characters starting at x=0, which differs from the
            // clipped form (which ends with the marker glyph).
            let max_chars = (PANEL_W / Size::Small.char_w()) as usize;
            let expected: String = format!(
                "{}{TRUNC_MARKER}",
                long_title.chars().take(max_chars - 1).collect::<String>()
            );
            let reference = render(&expected, Size::Small, align);

            assert_eq!(
                buf.framebuffer, reference.framebuffer,
                "clipped output must match the explicitly-clipped string ({align:?})"
            );

            // And it must stay inside the panel.
            let right = rightmost_inked_column(&buf).expect("some ink expected");
            assert!(
                right < PANEL_W as u32,
                "ink must stay inside the panel, got column {right} ({align:?})"
            );
        }

        // Short text is NOT clipped: it keeps all its characters. Compare against the
        // over-long case, where the marker replaced content. Glyphs are drawn
        // flush to their cell, so the rightmost inked column is the last glyph's
        // cell — measure how much of the panel the string covers instead.
        let short = render("Short", Size::Small, Align::Left);
        let short_right = rightmost_inked_column(&short).expect("some ink expected");
        // "Short" = 5 chars * 4px = 20px, so ink ends around column 19.
        assert!(
            short_right < 30,
            "unclipped short text should occupy only ~20px, got column {short_right}"
        );

        // A string that exactly fills the panel is neither clipped nor marked:
        // it renders whole.
        let exact = "a".repeat((PANEL_W / Size::Small.char_w()) as usize);
        let exact_buf = render(&exact, Size::Small, Align::Left);
        let exact_right = rightmost_inked_column(&exact_buf).expect("some ink expected");
        assert!(
            exact_right >= PANEL_W as u32 - 2,
            "exact-width text should fill the panel, got column {exact_right}"
        );
    }

    /// Regression guard for the XLarge wrap-waste bug.
    ///
    /// XL's nominal capacity is 16 chars x 3 lines = 48, but wrapping on word
    /// boundaries wastes the remainder of each line, so real-world lyrics of
    /// ~40 chars got clipped at XL and rendered with a truncation marker.
    /// `auto` must step down to Large before that happens.
    #[test]
    fn auto_steps_down_before_xlarge_can_truncate() {
        // The lyric the user actually saw clipped at XL (39 chars).
        let reported = "Even though we're goin' through it (ah)";
        assert!(
            pick_size(reported, 0, 5) != Size::XLarge,
            "a 39-char lyric must not render at XL: it would truncate"
        );
        assert_eq!(pick_size(reported, 0, 5), Size::Large);

        // At or below the measured threshold, XL is still chosen.
        assert_eq!(pick_size(&"a".repeat(XL_MAX_CHARS), 0, 5), Size::XLarge);
        assert_eq!(pick_size("Short line", 0, 5), Size::XLarge);

        // One char over the threshold steps down.
        assert_eq!(
            pick_size(&"a".repeat(XL_MAX_CHARS + 1), 0, 5),
            Size::Large
        );
        // The threshold must stay well under XL's nominal 48, or wrapping
        // waste brings truncation back.
        let nominal = (PANEL_W / Size::XLarge.char_w()) as usize * 3;
        assert!(
            XL_MAX_CHARS < nominal,
            "XL_MAX_CHARS ({XL_MAX_CHARS}) must be below nominal XL capacity ({nominal})"
        );
    }

    #[test]
    fn panel_geometry_matches_expected() {
        // These numbers drive wrap/truncation; a change here changes the UI.
        assert_eq!(Size::XLarge.char_w(), 8);
        // XL uses the true font height so three lines fit a 40px panel.
        assert_eq!(Size::XLarge.line_h(), 13);
        assert_eq!(PANEL_H / Size::XLarge.line_h(), 3);
        assert_eq!(Size::Large.line_h(), 8);
        assert_eq!(PANEL_W / Size::Small.char_w(), 32); // chars per line at S
    }

    #[test]
    fn auto_floors_at_large_and_truncates() {
        // Text far too long for any ladder entry: floor is Large, which means
        // the renderer truncates with an ellipsis rather than shrinking.
        let huge = "a".repeat(5_000);
        assert_eq!(pick_size(&huge, 0, 5), Size::Large);
        // Long-but-reasonable text also floors at Large rather than Medium.
        let long = "word ".repeat(40); // ~200 chars
        assert_eq!(pick_size(&long, 0, 5), Size::Large);
        // Auto must never select Medium or Small.
        for n in [1, 20, 48, 49, 105, 106, 200, 5_000] {
            let s = pick_size(&"a".repeat(n), 0, 5);
            assert!(
                matches!(s, Size::XLarge | Size::Large),
                "{n} chars picked {s:?}, expected XL or L"
            );
        }
    }

    #[test]
    fn auto_prefers_largest_that_fits() {
        assert_eq!(pick_size("Yeah", 0, 3), Size::XLarge);
        // XL's effective ceiling is XL_MAX_CHARS (34), not the nominal 48 —
        // word-wrap waste means longer lyrics would truncate.
        assert_eq!(pick_size(&"a".repeat(XL_MAX_CHARS), 0, 5), Size::XLarge);
        assert_eq!(
            pick_size(&"a".repeat(XL_MAX_CHARS + 1), 0, 5),
            Size::Large
        );
        // Large holds 21 * 5 = 105.
        assert_eq!(pick_size(&"a".repeat(105), 0, 5), Size::Large);
    }

    /// Regression guard: capacity must be capped by the lines a size actually
    /// fits AND by the wrap waste that makes XL's nominal capacity unreachable.
    ///
    /// XLarge is the special case: it uses `XL_MAX_CHARS` (34) rather than
    /// `chars_per_line * lines` (48), because wrapping on word boundaries wastes
    /// the remainder of each line. A 39-char lyric at XL truncated ~4% of the
    /// time in simulation and visibly for real lyrics.
    #[test]
    fn size_is_capped_by_panel_height_not_budget() {
        let real_lyric = "Bang bang, bang bang, that's the sound of my heart"; // 52 chars
        // 52 > XL_MAX_CHARS (34), so it must NOT choose XLarge.
        assert_ne!(pick_size(real_lyric, 0, 5), Size::XLarge);
        assert_eq!(pick_size(real_lyric, 0, 5), Size::Large);
        // A lyric inside the measured XL ceiling still renders at XL.
        assert_eq!(pick_size(&"a".repeat(XL_MAX_CHARS), 0, 5), Size::XLarge);
        // XL_MAX_CHARS must be meaningfully below the nominal 48, otherwise the
        // wrap-waste bug returns.
        assert!(
            (PANEL_W / Size::XLarge.char_w()) as usize * 3 - XL_MAX_CHARS >= 10,
            "XL_MAX_CHARS must leave headroom for wrap waste"
        );
    }

    #[test]
    fn three_xl_lines_fit_the_panel() {
        assert_eq!(Size::max_lines(Size::XLarge, 0), 3);
        // Three lines of 13px must not exceed the 40px panel.
        assert!(3 * Size::XLarge.line_h() <= PANEL_H);
    }

    #[test]
    fn cache_path_is_sanitised() {
        let p = cache_path(&track_key("Song/Name: Part 1", "Artist*", "Alb#um"));
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(!name.contains('/'));
        assert!(!name.contains('*'));
        assert!(!name.contains('#'));
        assert!(name.ends_with(".lrc"));
    }

    #[test]
    fn percent_decode_handles_escaped_paths() {
        assert_eq!(percent_decode("/tmp/My%20Song.mp3"), "/tmp/My Song.mp3");
        assert_eq!(percent_decode("/tmp/plain.mp3"), "/tmp/plain.mp3");
    }

    #[test]
    fn local_source_ignores_non_file_urls() {
        assert!(fetch_local("https://example.com/song.mp3").is_none());
    }
}
