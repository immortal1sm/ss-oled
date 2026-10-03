//! Custom JSON-API providers.
//!
//! Users define screens fed by any HTTP JSON endpoint directly in
//! settings.toml — no recompile. Each `[providers.custom.<name>]` section
//! becomes a rotation slot:
//!
//! ```toml
//! [providers.custom.youtube]
//! enabled = true
//! priority = 6
//! interval = 30              # seconds on screen per rotation cycle
//! poll = 300                 # seconds between API fetches
//! source = "https://api.example.com/stats"
//! header = "Authorization: Bearer ${YT_KEY}"   # optional; ${ENV} expanded
//! fields = ["items[0].statistics.subscriberCount: Subs",
//!           "items[0].snippet.title: Name"]
//! ```
//!
//! `fields` entries are `<json-path>: <label>`. Paths use dot notation with
//! optional `[index]` segments. Rendering shows up to 4 label/value rows per
//! page under the provider name; more fields cycle pages silently.

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::render::{display::ContentProvider, scheduler::ContentWrapper};
use anyhow::{anyhow, Result};
use apex_hardware::FrameBuffer;
use async_stream::try_stream;
use config::Config;
use embedded_graphics::{
    geometry::Size,
    mono_font::{iso_8859_15, MonoTextStyle},
    pixelcolor::BinaryColor,
    prelude::{Point, Primitive},
    primitives::{PrimitiveStyle, Rectangle},
    text::{renderer::TextRenderer, Baseline, Text},
    Drawable,
};
use futures::Stream;
use log::{info, warn};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::time::{interval, MissedTickBehavior};

/// One configured field: JSON path + display label.
#[derive(Clone)]
enum FieldAlign {
    Left,
    Center,
    Right,
}

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
enum FieldSize {
    Small,
    Medium,
    Large,
    XLarge,
    /// Largest class whose wrapped text fits the field's width AND the
    /// vertical space left below it. Resolved once per render, then carried
    /// through both the layout planner and the renderer so they cannot
    /// disagree about the size.
    Auto,
}

impl FieldSize {
    pub const LADDER: [FieldSize; 4] = [
        FieldSize::XLarge,
        FieldSize::Large,
        FieldSize::Medium,
        FieldSize::Small,
    ];

    pub fn char_w(self) -> i32 {
        match self {
            FieldSize::Small => 4,
            FieldSize::XLarge => 8,
            // Auto must never reach here (resolve() returns a concrete class),
            // but fall back to Medium rather than panic mid-render.
            FieldSize::Medium | FieldSize::Auto => 5,
            FieldSize::Large => 6,
        }
    }

    pub fn line_h(self) -> i32 {
        match self {
            FieldSize::XLarge => self.char_w() + 6,
            _ => self.char_w() + 2,
        }
    }

    /// Resolve `Auto` against the room actually left on the panel.
    ///
    /// Falls back to Small when nothing fits: the text overruns either way,
    /// and Small overruns least.
    pub fn resolve(self, text: &str, avail_w: i32, avail_h: i32) -> FieldSize {
        if self != FieldSize::Auto {
            return self;
        }
        let chars = text.chars().count() as i32;
        let avail_w = avail_w.max(8);
        let avail_h = avail_h.max(0);
        for c in Self::LADDER {
            if c.line_h() > avail_h {
                continue;
            }
            let per_line = (avail_w / c.char_w()).max(1);
            // One line of headroom: word wrapping never packs a line completely
            // full, so text that exactly fits can still spill.
            let lines = ((chars + per_line - 1) / per_line).max(1);
            if lines * c.line_h() <= avail_h {
                return c;
            }
        }
        FieldSize::Small
    }
}

#[derive(Clone)]
struct Field {
    path: String,
    label: String,
    /// Draw the label text before the value on the OLED.
    show_label: bool,
    /// Draw the value after the label on the OLED.
    show_value: bool,
    /// Horizontal alignment. Default: Left.
    align: FieldAlign,
    /// Font size class. Default: Medium.
    size: FieldSize,
    /// Explicit y-row slot (0-5). None = auto-pack in array order.
    row: Option<usize>,
    /// Render the text with a faux-bold double-strike at +1px X.
    bold: bool,
    /// Nudge this field up (negative) or down (positive), in pixels. Applied
    /// after the row is resolved. Mirrors `notifications.lines.*.dy`, so a tall
    /// glyph that starts a couple of rows below its cell top can be lined up
    /// with a smaller neighbour.
    dy: i32,
}

/// Registry of live item cursors, keyed by provider name.
///
/// A hotkey arrives as a `Command` on the scheduler's channel, but the stream
/// that owns the cursor is buried inside `Box<dyn ContentProvider>` with no
/// back-channel to reach it. Rather than widen the trait for one provider, each
/// custom provider publishes its cursor here and the hotkey steps it by name.
static ITEM_CURSORS: LazyLock<Mutex<HashMap<String, ItemHandle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Which field set a custom provider is showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    /// The headline list — one screen per array element.
    Highlights,
    /// The body of the selected element, scrolled line by line.
    Article,
}

#[derive(Clone)]
struct ItemHandle {
    index: Arc<Mutex<usize>>,
    view: Arc<Mutex<View>>,
    /// Line offset applied to the article view.
    scroll: Arc<Mutex<usize>>,
    /// Bumped whenever a hotkey steps the item, so the stream knows to reset
    /// its dwell timer and hold the newly chosen item for a full interval
    /// instead of advancing away from it after a few frames.
    generation: Arc<Mutex<u64>>,
    detail_fields: Vec<Field>,
}

fn register_item_handle(name: &str, handle: ItemHandle) {
    let mut map = ITEM_CURSORS.lock().unwrap();
    map.insert(name.to_string(), handle);
    log::info!("item hotkeys registered for '{name}' (have: {:?})", map.keys().collect::<Vec<_>>());
}

/// Scroll the article view by `delta` lines. False when the provider has no
/// items or no article field set.
pub fn scroll_item(name: &str, delta: isize) -> bool {
    let map = ITEM_CURSORS.lock().unwrap();
    let Some(handle) = map.get(name) else {
        log::warn!("step_item: '{name}' not registered (have: {:?})", map.keys().collect::<Vec<_>>());
        return false;
    };
    // Same single-guard rule as step_item.
    {
        let mut s = handle.scroll.lock().unwrap();
        *s = s.saturating_add_signed(delta);
    }
    true
}

/// Flip between the highlight list and the article body.
///
/// Returns the new view, or None when the provider has no article fields --
/// in which case the hotkey must not silently toggle into an empty screen.
pub fn toggle_view(name: &str) -> Option<View> {
    let map = ITEM_CURSORS.lock().unwrap();
    let handle = map.get(name)?;
    if handle.detail_fields.is_empty() {
        return None;
    }
    let mut view = handle.view.lock().unwrap();
    *view = if *view == View::Highlights {
        View::Article
    } else {
        View::Highlights
    };
    // Entering the article starts at the top; leaving it resets the scroll so
    // the next item does not open mid-way through.
    *handle.scroll.lock().unwrap() = 0;
    Some(*view)
}

/// Step the item of the named custom provider. `delta` is usually +/-1.
///
/// Returns true when a provider by that name is registered. Non-custom and
/// item-less providers simply are not in the registry, so a hotkey pressed
/// while one is showing is a harmless no-op rather than an error.
pub fn step_item(name: &str, delta: isize) -> bool {
    let map = ITEM_CURSORS.lock().unwrap();
    let Some(handle) = map.get(name) else {
        log::warn!("step_item: '{name}' not registered (have: {:?})", map.keys().collect::<Vec<_>>());
        return false;
    };
    // Take the guard ONCE. `*m.lock() = m.lock() + delta` deadlocks: the
    // right-hand side holds the guard to the end of the statement, so the
    // left-hand side blocks forever on a non-reentrant std::sync::Mutex.
    // That hung a tokio worker, so the hotkey appeared to do nothing AND the
    // whole IPC server stopped answering.
    {
        let mut idx = handle.index.lock().unwrap();
        *idx = idx.wrapping_add_signed(delta);
    }
    *handle.generation.lock().unwrap() += 1;
    true
}

/// True if this custom provider has array items to step through.
pub fn has_items(name: &str) -> bool {
    ITEM_CURSORS.lock().unwrap().contains_key(name)
}

/// One array element's resolved values, for both views.
///
/// The highlight and article views resolve DIFFERENT field sets from the same
/// element. Keeping them in one struct is what makes flipping between them a
/// matter of picking a side rather than re-fetching.
#[derive(Clone, Default)]
struct ItemValues {
    /// Values for the `fields` spec — the headline list.
    main: Vec<(String, String)>,
    /// Values for the `detail_fields` spec — the article body.
    detail: Vec<(String, String)>,
}

/// A single custom provider instance.
pub struct CustomProvider {
    name: String,
    source: String,
    header: Option<String>,
    fields: Vec<Field>,
    /// Seconds between API refreshes.
    poll_secs: u64,
    /// Whether to draw the provider-name header row.
    show_header: bool,
    /// Shared latest values, updated by fetch threads.
    ///
    /// One entry per ITEM when `items` is configured (the common case for an
    /// API that returns a list), otherwise a single entry. Each entry holds
    /// both the highlight and the article field set.
    values: Arc<Mutex<Vec<ItemValues>>>,
    /// JSON path to the array to iterate. `None` = the response root is a
    /// single object (the original behaviour).
    items: Option<String>,
    /// Which array element is currently shown. Stepped by the item hotkeys.
    item_index: Arc<Mutex<usize>>,
    /// Cap on how many array elements to keep.
    max_items: usize,
    /// Bumped by the item hotkeys; resets the dwell when it changes.
    generation: Arc<Mutex<u64>>,
    /// Field set for the article view. Empty means this provider has no
    /// article, and the toggle hotkey is a no-op for it.
    detail_fields: Vec<Field>,
    /// Shared with the item registry so the hotkeys can flip the view while
    /// the stream is running.
    view: Arc<Mutex<View>>,
    scroll: Arc<Mutex<usize>>,
}

const PER_PAGE: usize = 4;

fn pages_needed(n_fields: usize) -> usize {
    if n_fields == 0 {
        1
    } else {
        n_fields.div_ceil(PER_PAGE)
    }
}

fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Simple JSON path resolver: `a.b[0].c`.
fn get_path<'a>(json: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = json;
    for seg in path.split('.') {
        if seg.is_empty() {
            continue;
        }
        if let Some((key, idx)) = seg.split_once('[') {
            // Segment like "[0]" (no key, just an array index) or "key[0]"
            // (key + array index). Only do the key lookup if key is non-empty.
            if !key.is_empty() {
                cur = cur.get(key.trim_end_matches('['))?;
            }
            let idx: usize = idx.trim_end_matches(']').parse().ok()?;
            cur = cur.get(idx)?;
        } else {
            cur = cur.get(seg)?;
        }
    }
    Some(cur)
}

/// Expand `${VAR}` references from the process environment.
fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                out.push_str(std::env::var(&after[..end]).unwrap_or_default().as_str());
                rest = &after[end + 1..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Resolve one object's field specs into label/value pairs.
fn resolve_fields(json: &serde_json::Value, fields: &[Field]) -> Vec<(String, String)> {
    fields
        .iter()
        .filter(|f| f.show_label || f.show_value)
        .map(|f| {
            let val = get_path(json, &f.path)
                .map(value_to_string)
                .unwrap_or_else(|| "—".into());
            (f.label.clone(), val)
        })
        .collect()
}

/// Fetch + resolve all fields (runs inside spawn_blocking).
///
/// With `items` set, the response is expected to contain an array at that
/// path and each element becomes its own screen. Elements whose FIRST field
/// resolves to empty are dropped -- list APIs routinely include nulls,
/// deleted posts and placeholders, and rendering "—" for each is noise.
fn fetch_values(
    source: &str,
    header: &Option<String>,
    fields: &[Field],
    detail_fields: &[Field],
    items: Option<&str>,
    max_items: usize,
) -> Result<Vec<ItemValues>> {
    let mut req = ureq::get(source).timeout(Duration::from_secs(8));
    if let Some(h) = header {
        let expanded = expand_env(h);
        let mut parts = expanded.splitn(2, ':');
        let key = parts.next().unwrap_or("").trim();
        let val = parts.next().unwrap_or("").trim();
        if !key.is_empty() {
            req = req.set(key, val);
        }
    }
    let body = req.call()?.into_string()?;
    let json: serde_json::Value = serde_json::from_str(&body)?;

    // Resolve both field sets against the same object. Doing it here is what
    // lets the article view show the ARTICLE rather than re-showing the
    // highlight values under the detail labels.
    let both = |el: &serde_json::Value| ItemValues {
        main: resolve_fields(el, fields),
        detail: resolve_fields(el, detail_fields),
    };

    let Some(items_path) = items else {
        return Ok(vec![both(&json)]);
    };

    let array = match get_path(&json, items_path) {
        Some(serde_json::Value::Array(a)) => a,
        Some(_) => return Err(anyhow::anyhow!("`{items_path}` is not an array")),
        None => return Ok(Vec::new()),
    };

    let first_visible = fields
        .iter()
        .find(|f| f.show_label || f.show_value)
        .map(|f| f.path.clone())
        .unwrap_or_default();

    let kept: Vec<&serde_json::Value> = array
        .iter()
        .take(max_items.max(1))
        .filter(|el| {
            // Filter on the first visible field: an item with nothing to show
            // is a null / deleted / placeholder entry.
            if first_visible.is_empty() {
                return true;
            }
            match get_path(el, &first_visible) {
                Some(serde_json::Value::String(s)) => !s.trim().is_empty(),
                Some(serde_json::Value::Null) | None => false,
                Some(_) => true,
            }
        })
        .collect();
    // One line per poll, not per item: confirms the array was found and how
    // many entries survived the junk filter.
    log::info!(
        "custom: items '{}' -> {} of {} elements kept",
        items_path,
        kept.len(),
        array.len()
    );
    Ok(kept.into_iter().map(both).collect())
}

/// Wrap `text` into up to `max_lines` lines, each fitting within
/// `max_px` pixels at `char_w` pixels per character. Splits at the
/// last word boundary within each line; falls back to character-level
/// split when no space fits. Returns fewer lines if the text wraps to
/// fewer than `max_lines`. Returns one entry per line.
///
/// Public so the notification renderer can wrap its body the same way —
/// two independent wrap implementations drift apart on edge cases.
pub fn wrap_text(text: &str, max_px: i32, char_w: i32, max_lines: usize) -> Vec<String> {
    if max_lines == 0 {
        return vec![];
    }
    if text.is_empty() {
        return vec![String::new()];
    }
    let max_chars = (max_px / char_w).max(1) as usize;
    let mut lines: Vec<String> = Vec::new();
    // Convert to owned String once so the loop can reassign remaining
    // without lifetime gymnastics. We shadow `text` to keep the loop body
    // reading naturally.
    let mut remaining = text.to_string();
    while lines.len() < max_lines {
        if remaining.chars().count() <= max_chars {
            lines.push(remaining.to_string());
            return lines;
        }
        // Find the rightmost space within the first max_chars chars.
        let mut prefix_end_byte = remaining.len();
        for (i, (byte_idx, _ch)) in remaining.char_indices().enumerate() {
            if i == max_chars {
                prefix_end_byte = byte_idx;
                break;
            }
        }
        let prefix = &remaining[..prefix_end_byte];
        let split_chars = match prefix.rfind(' ') {
            Some(byte_idx) if byte_idx > 0 => prefix[..byte_idx].chars().count(),
            _ => max_chars,
        };
        let first: String = remaining.chars().take(split_chars).collect();
        remaining = remaining
            .chars()
            .skip(split_chars)
            .collect::<String>()
            .trim_start()
            .to_string();
        if first.is_empty() {
            // Safety: avoid infinite loop if split produced nothing.
            break;
        }
        lines.push(first);
    }
    // If we hit max_lines with content still unrendered, truncate
    // remaining to fit on the last line.
    if !remaining.is_empty() {
        let take = max_chars.saturating_sub(1); // leave 1 char for ellipsis
        let truncated: String = remaining.chars().take(take).collect::<String>();
        lines.push(format!("{truncated}…"));
    }
    lines
}

/// Draw `text` at `pos`. When `bold` is true, draw it twice with a 1px
/// horizontal offset to produce a faux-bold appearance (no real bold
/// variant exists for the embedded-graphics mono fonts we use).
fn draw_text(
    buffer: &mut FrameBuffer,
    text: &str,
    pos: Point,
    style: MonoTextStyle<BinaryColor>,
    bold: bool,
) -> Result<()> {
    Text::with_baseline(text, pos, style, embedded_graphics::text::Baseline::Top).draw(buffer)?;
    if bold {
        Text::with_baseline(
            text,
            Point::new(pos.x + 1, pos.y),
            style,
            embedded_graphics::text::Baseline::Top,
        )
        .draw(buffer)?;
    }
    Ok(())
}

impl CustomProvider {
    /// Scrolling article view: wraps the detail field's value to the panel
    /// width and shows a window of lines starting at `scroll`.
    ///
    /// Deliberately simpler than `render_rows`: scrolling wants a continuous
    /// line offset, while render_rows packs discrete label/value rows into
    /// slots. Reusing it would fight the `row` planner for no benefit.
    fn render_article(
        name: &str,
        values: &[(String, String)],
        detail_fields: &[Field],
        scroll: usize,
        show_header: bool,
    ) -> Result<FrameBuffer> {
        let mut buffer = FrameBuffer::new();
        let header_style = MonoTextStyle::new(&iso_8859_15::FONT_6X10, BinaryColor::On);
        let mut y = 0i32;

        if show_header {
            Text::with_baseline(
                name.to_uppercase().as_str(),
                Point::new(0, y),
                header_style,
                embedded_graphics::text::Baseline::Top,
            )
            .draw(&mut buffer)?;
            y += 12;
        }

        // The first detail field is treated as the body; later ones are
        // appended so `detail_fields` can carry a title line plus a body.
        let mut body = String::new();
        for (i, f) in detail_fields.iter().enumerate() {
            let Some((_, v)) = values.get(i) else { break };
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(v);
        }

        if body.trim().is_empty() {
            let style = MonoTextStyle::new(&iso_8859_15::FONT_6X10, BinaryColor::On);
            let msg: &str = if detail_fields.is_empty() {
                "NO ARTICLE"
            } else {
                "EMPTY"
            };
            draw_text(&mut buffer, msg, Point::new(2, y + 6), style, false)?;
            return Ok(buffer);
        }

        let size = detail_fields[0].size;
        // Resolve once here so the style, the wrapping and the line pitch all
        // agree on the size.
        let size = size.resolve(&body, 128, 40 - y.max(0));
        let char_w = size.char_w();
        let line_h = size.line_h();
        let style = match size {
            FieldSize::Small => MonoTextStyle::new(&iso_8859_15::FONT_4X6, BinaryColor::On),
            FieldSize::Large => MonoTextStyle::new(&iso_8859_15::FONT_6X10, BinaryColor::On),
            FieldSize::XLarge => MonoTextStyle::new(&iso_8859_15::FONT_8X13, BinaryColor::On),
            FieldSize::Medium | FieldSize::Auto => {
                MonoTextStyle::new(&iso_8859_15::FONT_5X7, BinaryColor::On)
            }
        };
        // Wrap generously, then scroll by line.
        let max_lines = 200;
        let all = wrap_text(&body, 128, char_w, max_lines);

        for (n, line) in all.iter().enumerate().skip(scroll) {
            let ly = y + (n - scroll) as i32 * line_h;
            if ly >= 40 || ly + line_h <= y {
                continue;
            }
            draw_text(&mut buffer, line, Point::new(0, ly), style, detail_fields[0].bold)?;
        }
        Ok(buffer)
    }

    fn render_rows(
        name: &str,
        values: &[(String, String)],
        page: u64,
        show_header: bool,
        fields: &[Field],
    ) -> Result<FrameBuffer> {
        let mut buffer = FrameBuffer::new();
        let header_style = MonoTextStyle::new(&iso_8859_15::FONT_6X10, BinaryColor::On);

        let mut y = 0;

        if show_header {
            Text::with_baseline(
                name.to_uppercase().as_str(),
                Point::new(0, y),
                header_style,
                embedded_graphics::text::Baseline::Top,
            )
            .draw(&mut buffer)?;
            y += 12;
        }

        // No data retrieved yet — explicit placeholder centered in the
        // available vertical space. FONT_9X15 is 15px tall, so a 40px
        // panel centers at top_y = 12 (no header) or 14 (with header).
        if values.is_empty() {
            let placeholder = MonoTextStyle::new(&iso_8859_15::FONT_9X15, BinaryColor::On);
            let text = "NO DATA";
            let m = placeholder.measure_string(
                text,
                Point::zero(),
                embedded_graphics::text::Baseline::Top,
            );
            let x = (128 - m.bounding_box.size.width as i32) / 2;
            let top_y = if show_header { 14 } else { 12 };
            Text::with_baseline(
                text,
                Point::new(x, top_y),
                placeholder,
                embedded_graphics::text::Baseline::Top,
            )
            .draw(&mut buffer)?;
            return Ok(buffer);
        }

        if !show_header && y == 0 {
            y = 2;
        }

        // Render plan: one entry per visible field. Explicit `row` slots
        // are reserved first; auto-packed fields fill remaining vertical
        // space and consume exactly as much height as wrap_text() reports
        // for their value (so wrapped fields don't overlap the next one).
        // (row index, y, RESOLVED size) -- the renderer must use the same
        // size the planner laid out with, or text lands at the wrong pitch.
        let mut plan: Vec<(usize, i32, FieldSize)> = Vec::new();
        let mut taken_rows: [bool; 6] = [false; 6];
        let mut next_auto_y = y;
        for (idx, f) in fields.iter().enumerate() {
            if idx >= values.len() {
                break;
            }
            if !f.show_label && !f.show_value {
                continue;
            }
            // Per-field metrics used by both planning and rendering.
            // Auto resolves against the room BELOW this field, so a long value
            // steps down instead of pushing the fields under it off-panel.
            let value_text = values.get(idx).map(|(_, v)| v.as_str()).unwrap_or("");
            let label_len = if f.show_label && !f.label.is_empty() {
                f.label.chars().count() as i32 + 1
            } else {
                0
            };
            let fsize = f.size.resolve(
                value_text,
                128 - (label_len + 1) * 5,
                40 - next_auto_y.max(0),
            );
            let char_w = fsize.char_w();
            let line_h = fsize.line_h();

            let row_y = match f.row {
                Some(r) if r < taken_rows.len() && !taken_rows[r] => {
                    taken_rows[r] = true;
                    // Explicit slot: 1-line height only (truncate, no wrap).
                    let target = match fsize {
                        FieldSize::XLarge => r as i32 * 18,
                        FieldSize::Large => r as i32 * 14,
                        FieldSize::Small => r as i32 * 6,
                        FieldSize::Medium | FieldSize::Auto => r as i32 * 8,
                    };
                    target.max(y)
                }
                _ => {
                    // Auto-pack: simulate wrap to learn the actual height
                    // this field will consume, then advance the cursor by
                    // exactly that many lines so the next field starts
                    // immediately below the wrapped output.
                    let t = next_auto_y;
                    if idx < values.len() {
                        let (_label, value) = &values[idx];
                        let label_text = if f.show_label && !_label.is_empty() {
                            format!("{_label}:")
                        } else {
                            String::new()
                        };
                        let label_w = if !label_text.is_empty() {
                            char_w * label_text.chars().count() as i32
                        } else {
                            0
                        };
                        let reserved_left = if label_text.is_empty() {
                            0
                        } else {
                            label_w + 4
                        };
                        let avail_w = (128 - reserved_left).max(8);
                        let max_lines = if t < 40 {
                            (((40 - t) / line_h) as usize).min(3)
                        } else {
                            0
                        };
                        let lines = if value.is_empty() || max_lines == 0 {
                            1
                        } else {
                            wrap_text(value, avail_w, char_w, max_lines).len()
                        };
                        next_auto_y += (lines as i32) * line_h;
                    } else {
                        next_auto_y += line_h;
                    }
                    t
                }
            };
            // Apply the per-field vertical nudge LAST, after the row/auto-pack
            // has resolved, so it shifts the rendered text without disturbing
            // the packing cursor (which must keep advancing by real heights or
            // later fields collide).
            plan.push((idx, row_y + f.dy, fsize));
        }

        for (row_idx, row_y, fsize) in plan {
            let (label, value) = &values[row_idx];
            let f = &fields[row_idx];
            // `fsize` comes from the plan, NOT `f.size`: when a field is Auto
            // the planner resolved it against the space available, and the
            // renderer has to draw at exactly that class or the glyph pitch
            // will not match the rows below.
            let style = match fsize {
                FieldSize::Small => MonoTextStyle::new(&iso_8859_15::FONT_4X6, BinaryColor::On),
                FieldSize::Large => MonoTextStyle::new(&iso_8859_15::FONT_6X10, BinaryColor::On),
                FieldSize::XLarge => MonoTextStyle::new(&iso_8859_15::FONT_8X13, BinaryColor::On),
                FieldSize::Medium | FieldSize::Auto => {
                    MonoTextStyle::new(&iso_8859_15::FONT_5X7, BinaryColor::On)
                }
            };
            let char_w = fsize.char_w();
            // XLarge uses 2-char ascender + base so it gets extra leading
            // room vs the simple char_w + 2 heuristic used for other sizes.
            let line_h = fsize.line_h();

            let label_text = if f.show_label && !label.is_empty() {
                format!("{label}:")
            } else {
                String::new()
            };
            let text = if f.show_value {
                value.clone()
            } else {
                String::new()
            };

            let label_w = if !label_text.is_empty() {
                char_w * label_text.chars().count() as i32
            } else {
                0
            };
            let reserved_left = if label_text.is_empty() {
                0
            } else {
                label_w + 4
            };
            let avail_w = (128 - reserved_left).max(8);

            // Wrap only in auto-pack mode; explicit slots reserve vertical
            // space and a wrapped 2nd line would collide with the next slot.
            // max_lines = how many additional lines fit in the remaining
            // vertical space below row_y, capped at 3 to keep the panel
            // usable (most API values fit in 3 lines). Clamped to 0 so
            // wrap_text is a no-op if there's no room.
            let max_lines = if row_y < 40 {
                (((40 - row_y) / line_h) as usize).min(3)
            } else {
                0
            };
            let can_wrap = f.row.is_none();
            let wrapped: Vec<String> = if text.is_empty() {
                vec![String::new()]
            } else if can_wrap && max_lines > 0 {
                wrap_text(&text, avail_w, char_w, max_lines)
            } else {
                // Explicit row slot OR no room for any wrap: one truncated
                // line that fits the available width.
                let take = (avail_w / char_w).max(0) as usize;
                vec![text.chars().take(take).collect()]
            };

            // Use FIRST line width for horizontal alignment; subsequent
            // wrapped lines hang at y + n*line_h.
            let first_w = wrapped
                .first()
                .map(|l| char_w * l.chars().count() as i32)
                .unwrap_or(0);
            let total_w = reserved_left + first_w;
            let x_offset = match f.align {
                FieldAlign::Left => 0,
                FieldAlign::Center => (128 - total_w).max(0) / 2,
                FieldAlign::Right => 128 - total_w,
            };

            if !label_text.is_empty() {
                draw_text(
                    &mut buffer,
                    &label_text,
                    Point::new(x_offset, row_y),
                    style,
                    f.bold,
                );
            }
            let value_x = if label_text.is_empty() {
                x_offset
            } else {
                x_offset + label_w + 4
            };

            for (line_idx, line) in wrapped.iter().enumerate() {
                if line.is_empty() {
                    continue;
                }
                let y_pos = row_y + (line_idx as i32) * line_h;
                if y_pos + line_h > 40 {
                    break; // off-panel
                }
                draw_text(&mut buffer, line, Point::new(value_x, y_pos), style, f.bold);
            }
        }

        Ok(buffer)
    }
}

impl ContentProvider for CustomProvider {
    type ContentStream<'a>
    where
        Self: 'a,
    = impl Stream<Item = Result<FrameBuffer>> + 'a;

    fn stream(&mut self) -> Result<Self::ContentStream<'_>> {
        info!("Registering custom display source '{}'.", self.name);

        let values = Arc::clone(&self.values);
        let source = self.source.clone();
        let header = self.header.clone();
        let fields = self.fields.clone();
        let name = self.name.clone();
        // Visibility is read from self.fields directly in render_rows.
        let items = self.items.clone();
        let max_items = self.max_items;
        let item_index = Arc::clone(&self.item_index);
        let generation = Arc::clone(&self.generation);
        let mut seen_generation = 0u64;
        // Read the shared view/scroll each frame rather than snapshotting:
        // the hotkeys mutate them while the stream runs.
        let view = Arc::clone(&self.view);
        let scroll = Arc::clone(&self.scroll);
        let detail_fields = self.detail_fields.clone();

        Ok(try_stream! {
            // Initial fetch off-thread; placeholder rows until first success.
            {
                let values = Arc::clone(&values);
                let init_name = name.clone();
                let source = source.clone();
                let header = header.clone();
                let fields = fields.clone();
                let items = items.clone();
                let detail_fields = detail_fields.clone();
                tokio::task::spawn_blocking(move || {
                    match fetch_values(&source, &header, &fields, &detail_fields, items.as_deref(), max_items) {
                        Ok(v) => *values.lock().unwrap() = v,
                        Err(e) => warn!("custom '{init_name}' initial fetch failed: {e}"),
                    }
                });
            }

            let mut tick = interval(Duration::from_millis(300));
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let poll_every = Duration::from_secs(self.poll_secs.max(5));
            let page_frames = 13; // ~4s per page at 300ms
            let mut frames: u64 = 0;
            let mut last_fetch = Instant::now();

            let show_header = self.show_header;


            loop {
                // `item_index` selects which array element is on screen. With
                // no `items` configured there is exactly one entry, so it stays
                // at 0 and paging across fields behaves exactly as before.
                {
                    let guard = values.lock().unwrap();
                    let n_items = guard.len().max(1);
                    let idx = (*item_index.lock().unwrap()) % n_items;
                    let empty = ItemValues::default();
                    let item: &ItemValues = guard.get(idx).unwrap_or(&empty);
                    if *view.lock().unwrap() == View::Article {
                        // Article view is a continuous scroller, so it must
                        // NOT auto-advance: that would scroll away from
                        // whatever the user is reading.
                        yield Self::render_article(
                            &name,
                            &item.detail,
                            &detail_fields,
                            *scroll.lock().unwrap(),
                            show_header,
                        )?;
                    } else {
                        // Field paging applies WITHIN the selected item.
                        let page = (frames / page_frames)
                            % pages_needed(item.main.len()).max(1) as u64;
                        yield Self::render_rows(
                            &name,
                            &item.main,
                            page,
                            show_header,
                            &self.fields,
                        )?;
                    }
                }

                // A hotkey step resets the dwell, so the item the user just
                // chose stays on screen for a full interval instead of being
                // replaced by the next one a few frames later.
                let gen_now = *generation.lock().unwrap();
                if gen_now != seen_generation {
                    seen_generation = gen_now;
                    frames = 0;
                }

                // Advance to the next item at the end of each dwell, but not
                // while the article view is open -- scrolling is manual there.
                if frames % page_frames == page_frames - 1
                    && *view.lock().unwrap() == View::Highlights
                {
                    let len = values.lock().unwrap().len().max(1);
                    let mut idx = item_index.lock().unwrap();
                    *idx = (*idx + 1) % len;
                }
                frames += 1;
                if last_fetch.elapsed() >= poll_every {
                    last_fetch = Instant::now();
                    let values = Arc::clone(&values);
                    let source = source.clone();
                    let header = header.clone();
                    let fields = fields.clone();
                    let items = items.clone();
                    let detail_fields = detail_fields.clone();
                    tokio::task::spawn_blocking(move || {
                        match fetch_values(&source, &header, &fields, &detail_fields, items.as_deref(), max_items) {
                            Ok(v) => *values.lock().unwrap() = v,
                            Err(e) => warn!("custom provider refresh failed: {e}"),
                        }
                    });
                }
                tick.tick().await;
            }
        })
    }

    fn name(&self) -> &'static str {
        // Providers live for the process lifetime; leaking one short string
        // satisfies the trait's 'static requirement for dynamic names.
        Box::leak(self.name.clone().into_boxed_str())
    }
}

/// Enumerate `[providers.custom.<name>]` section names.
pub fn list_custom_sections(config: &Config) -> Vec<String> {
    let custom = match config.get_table("providers") {
        Ok(t) => match t.get("custom") {
            Some(v) => match v.clone().into_table() {
                Ok(t2) => t2,
                Err(_) => return Vec::new(),
            },
            None => return Vec::new(),
        },
        Err(_) => return Vec::new(),
    };
    let mut names: Vec<String> = custom.keys().map(|k| k.to_string()).collect();
    names.sort();
    names
}

/// Build one CustomProvider from a `[providers.custom.<name>]` config table.
/// Returns `Ok(None)` when the section is disabled.
/// Parse a `fields` array from config into `Field`s.
///
/// Shared by `fields` and `detail_fields` so both accept the identical spec
/// syntax: `"<json-path>: <label>[!]" + " | a=L s=M r=N b=1 d=N"`.
/// Parse a `fields` array into `Field`s.
///
/// Shared by `fields` and `detail_fields` so both accept the identical spec
/// syntax. `prefix` is the config prefix, e.g. `providers.custom.advice`.
/// `key` names the array to read, relative to `prefix` — `fields` for the
/// highlight list, `article_fields` for the secondary screen.
fn parse_field_specs(config: &Config, prefix: &str, key: &str) -> Vec<Field> {
    let field_specs: Vec<String> = config
        .get_array(&format!("{prefix}.{key}"))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.into_str().ok())
        .collect();

    let mut fields = Vec::new();
    for spec in field_specs {
        // "<json-path>: <label>[!]" + optional " | a=L s=M r=N b=1" layout suffix.
        // '!' marks value hidden; " -" in the label slot marks label hidden.
        let mut align = FieldAlign::Left;
        let mut size = FieldSize::Medium;
        let mut row: Option<usize> = None;
        let mut bold = false;
        let mut dy: i32 = 0;
        let (base, layout) = match spec.split_once('|') {
            Some((b, l)) => (b, l),
            None => (spec.as_str(), ""),
        };
        for kv in layout.split_whitespace() {
            if let Some((k, v)) = kv.split_once('=') {
                match k {
                    "a" => {
                        align = match v {
                            "L" | "l" => FieldAlign::Left,
                            "C" | "c" => FieldAlign::Center,
                            "R" | "r" => FieldAlign::Right,
                            _ => FieldAlign::Left,
                        }
                    }
                    "s" => {
                        // Accept both the single letters the GUI writes and
                        // the word forms the notification layout uses, so the
                        // two spell the same size the same way.
                        size = match v.to_ascii_uppercase().as_str() {
                            "S" | "SMALL" => FieldSize::Small,
                            "M" | "MEDIUM" => FieldSize::Medium,
                            "L" | "LARGE" => FieldSize::Large,
                            "X" | "XL" | "XLARGE" => FieldSize::XLarge,
                            // Auto: largest class the field's text fits in.
                            "A" | "AUTO" => FieldSize::Auto,
                            _ => FieldSize::Medium,
                        }
                    }
                    "r" => row = v.parse::<usize>().ok(),
                    "b" => bold = matches!(v, "1" | "true" | "yes" | "on"),
                    // Signed: `d=-2` nudges up, `d=2` nudges down.
                    "d" => dy = v.parse::<i32>().unwrap_or(0),
                    _ => {}
                }
            }
        }
        // TRIM first: splitting on `|` leaves `"path: Label "` with a trailing
        // space, so `strip_suffix('!')` would never match and a hidden value
        // would silently reappear on any field carrying layout metadata.
        let base = base.trim_end();
        let (spec, show_value) = match base.strip_suffix('!') {
            Some(p) => (p.to_string(), false),
            None => (base.to_string(), true),
        };
        let (path, label) = match spec.split_once(':') {
            Some((p, l)) => {
                let l = l.trim().to_string();
                let l = if l == "-" { String::new() } else { l };
                (p.trim().to_string(), l)
            }
            None => {
                let tail = spec.rsplit('.').next().unwrap_or(&spec).to_string();
                (spec.trim().to_string(), tail)
            }
        };
        fields.push(Field {
            path,
            show_label: !label.is_empty(),
            label,
            show_value,
            align,
            size,
            row,
            bold,
            dy,
        });
    }

    fields
}

pub fn from_config_section(name: &str, config: &Config) -> Result<Option<CustomProvider>> {
    let prefix = format!("providers.custom.{name}");
    if !config
        .get_bool(&format!("{prefix}.enabled"))
        .unwrap_or(false)
    {
        return Ok(None);
    }

    let source: String = config
        .get_str(&format!("{prefix}.source"))
        .map_err(|_| anyhow!("{prefix}.source is required"))?;

    let header = config
        .get_str(&format!("{prefix}.header"))
        .ok()
        .filter(|h| !h.is_empty());

    let poll_secs: u64 = config
        .get_int(&format!("{prefix}.poll"))
        .unwrap_or(300)
        .max(10) as u64;

    let show_header = config
        .get_bool(&format!("{prefix}.show_header"))
        .unwrap_or(true);

    let fields = parse_field_specs(config, &prefix, "fields");

    if fields.is_empty() {
        return Err(anyhow!("{prefix}.fields is empty — nothing to show"));
    }

    let items = config
        .get_str(&format!("providers.custom.{name}.items"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let max_items = config
        .get_int(&format!("providers.custom.{name}.max_items"))
        .unwrap_or(50)
        .clamp(1, 200) as usize;

    // The article view is EXPLICIT: `article = true` plus `article_fields`.
    // It is deliberately NOT inferred from the fields merely being present, so
    // a provider can keep a spare field list around without the hotkey
    // unexpectedly opening a second screen. The old nested `detail.fields` is
    // still honoured as a fallback so existing configs keep working.
    let article_enabled = config
        .get_bool(&format!("providers.custom.{name}.article"))
        .or_else(|_| {
            config
                .get_array(&format!("providers.custom.{name}.detail.fields"))
                .map(|a| !a.is_empty())
        })
        .unwrap_or(false);
    let detail_fields: Vec<Field> = if article_enabled {
        parse_field_specs(config, &format!("providers.custom.{name}"), "article_fields")
            .into_iter()
            .filter(|f| f.show_label || f.show_value)
            .collect()
    } else {
        Vec::new()
    };

    let index = Arc::new(Mutex::new(0usize));
    let generation = Arc::new(Mutex::new(0u64));
    let view = Arc::new(Mutex::new(View::Highlights));
    let scroll = Arc::new(Mutex::new(0usize));
    if items.is_some() {
        register_item_handle(
            name,
            ItemHandle {
                index: Arc::clone(&index),
                view: Arc::clone(&view),
                scroll: Arc::clone(&scroll),
                generation: Arc::clone(&generation),
                detail_fields: detail_fields.clone(),
            },
        );
    }

    Ok(Some(CustomProvider {
        items,
        max_items,
        item_index: index,
        generation,
        detail_fields,
        view,
        scroll,
        name: name.to_string(),
        source,
        header,
        fields,
        poll_secs,
        show_header,
        values: Arc::new(Mutex::new(Vec::new())),
    }))
}


#[cfg(test)]
mod dy_tests {
    use super::*;

    fn build_with(field_spec: &str) -> CustomProvider {
        // This `config` version has no Config::builder(), so parse the TOML
        // directly and use `Config::new`.
        let toml_src = format!(
            r#"
[providers.custom.t]
enabled = true
source = "https://example.com/x.json"
fields = ["{field_spec}"]
"#
        );
        // Same pattern main.rs uses for the daemon's config load.
        let mut cfg = Config::default();
        cfg.merge(config::File::from_str(
            &toml_src,
            config::FileFormat::Toml,
        ))
        .expect("config merge");
        from_config_section("t", &cfg)
            .expect("parse")
            .expect("enabled")
    }

    /// `d=` must reach the Field through the real config parser, and default to
    /// 0 when absent so existing fields are unaffected.
    /// The headline feature: an API whose response wraps a LIST must produce
    /// one screen per element, with field paths resolved relative to each
    /// element (not the wrapper).
    #[test]
    fn items_iterates_a_wrapped_array() {
        // Shape taken from the real HackerNews Algolia response.
        let json: serde_json::Value = serde_json::from_str(
            r#"{"hits":[{"title":"First story","author":"a1","points":10},
                        {"title":"Second story","author":"b2","points":20},
                        {"title":"Third story","author":"c3","points":30}],
                "nbHits":3}"#,
        )
        .unwrap();
        let fields = vec![
            Field { path: "title".into(), label: "t".into(), show_label: true, show_value: true,
                    align: FieldAlign::Left, size: FieldSize::Medium, row: None, bold: false, dy: 0 },
            Field { path: "author".into(), label: "a".into(), show_label: true, show_value: true,
                    align: FieldAlign::Left, size: FieldSize::Medium, row: None, bold: false, dy: 0 },
        ];
        // Exercise the same resolution path fetch_values uses.
        let resolved: Vec<Vec<(String, String)>> = json["hits"]
            .as_array().unwrap()
            .iter()
            .map(|el| resolve_fields(el, &fields))
            .collect();
        assert_eq!(resolved.len(), 3, "expected one screen per array element");
        assert_eq!(resolved[0][0].1, "First story");
        assert_eq!(resolved[1][0].1, "Second story");
        assert_eq!(resolved[2][1].1, "c3");
    }

    /// JUNK FILTER: nulls / blank / deleted entries must be dropped, or the
    /// panel fills with "—" for items that were never real.
    /// Regression: the article view rendered the HIGHLIGHT values under the
    /// detail labels, because both views read the same resolved list. Each
    /// view must read its own field set from the same element.
    #[test]
    fn article_view_uses_the_detail_fields_not_the_highlights() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"title":"Headline text","url":"https://example.com","story_text":"the full article body"}"#,
        )
        .unwrap();
        let main_fields = vec![Field { path: "title".into(), label: "t".into(),
            show_label: false, show_value: true, align: FieldAlign::Left,
            size: FieldSize::Medium, row: None, bold: false, dy: 0 }];
        let detail_fields = vec![Field { path: "story_text".into(), label: "b".into(),
            show_label: false, show_value: true, align: FieldAlign::Left,
            size: FieldSize::Medium, row: None, bold: false, dy: 0 }];

        let item = ItemValues {
            main: resolve_fields(&json, &main_fields),
            detail: resolve_fields(&json, &detail_fields),
        };
        assert_eq!(item.main[0].1, "Headline text");
        assert_eq!(
            item.detail[0].1, "the full article body",
            "article view must resolve detail_fields, not main"
        );
        assert_ne!(item.detail[0].1, item.main[0].1);
    }

    /// A field that is absent must not silently become the OTHER view's text.
    #[test]
    fn missing_detail_field_does_not_fall_back_to_the_headline() {
        let json: serde_json::Value =
            serde_json::from_str(r#"{"title":"Headline text"}"#).unwrap();
        let main_fields = vec![Field { path: "title".into(), label: "t".into(),
            show_label: false, show_value: true, align: FieldAlign::Left,
            size: FieldSize::Medium, row: None, bold: false, dy: 0 }];
        let detail_fields = vec![Field { path: "story_text".into(), label: "b".into(),
            show_label: false, show_value: true, align: FieldAlign::Left,
            size: FieldSize::Medium, row: None, bold: false, dy: 0 }];
        let item = ItemValues {
            main: resolve_fields(&json, &main_fields),
            detail: resolve_fields(&json, &detail_fields),
        };
        assert_ne!(item.detail[0].1, item.main[0].1, "article fell back to the headline");
        assert_eq!(item.detail[0].1, "\u{2014}"); // the em-dash placeholder
    }

    #[test]
    fn blank_items_are_filtered_out() {
        let json: serde_json::Value = serde_json::from_str(
            r#"[{"title":"Real one"},{"title":null},{"title":""},{"title":"   "},{"nope":1}]"#,
        )
        .unwrap();
        let fields = vec![Field { path: "title".into(), label: "t".into(), show_label: true,
            show_value: true, align: FieldAlign::Left, size: FieldSize::Medium, row: None,
            bold: false, dy: 0 }];
        let first_visible = fields[0].path.clone();
        let kept: Vec<&serde_json::Value> = json.as_array().unwrap().iter()
            .filter(|el| match get_path(el, &first_visible) {
                Some(serde_json::Value::String(s)) => !s.trim().is_empty(),
                Some(serde_json::Value::Null) | None => false,
                Some(_) => true,
            })
            .collect();
        assert_eq!(kept.len(), 1, "only the real item should survive");
        assert_eq!(get_path(kept[0], "title").unwrap(), &serde_json::json!("Real one"));
    }

    #[test]
    /// Auto must scale with the room available, not be a fixed size in
    /// disguise: the same text gets a bigger class when it has space.
    #[test]
    fn field_auto_scales_with_available_room() {
        let roomy = FieldSize::Auto.resolve("OK", 128, 40);
        let tight = FieldSize::Auto.resolve("a considerably longer value that will not fit", 128, 12);
        assert!(
            roomy.line_h() > tight.line_h(),
            "auto did not scale: roomy={roomy:?} tight={tight:?}"
        );
        assert_eq!(roomy, FieldSize::XLarge, "full-height short text should reach XL");
    }

    /// An explicit size is returned untouched whatever the text or room.
    #[test]
    fn explicit_field_sizes_pass_through() {
        let huge = "x".repeat(500);
        for c in [FieldSize::Small, FieldSize::Medium, FieldSize::Large, FieldSize::XLarge] {
            assert_eq!(c.resolve("anything", 128, 40), c);
            assert_eq!(c.resolve(&huge, 10, 5), c);
        }
    }

    /// An impossible constraint must still yield a drawable class, never Auto.
    #[test]
    fn field_resolve_never_returns_auto() {
        for w in [0, 1, 7, 64, 128] {
            for h in [0, 1, 3, 11, 40] {
                let r = FieldSize::Auto.resolve("some value here", w, h);
                assert_ne!(r, FieldSize::Auto, "auto leaked out at {w}x{h}");
            }
        }
    }

    /// `s=A` in a field spec must select Auto, alongside the existing sizes.
    #[test]
    fn field_auto_parses_from_the_spec_syntax() {
        // This `config` version has no Config::builder(); use the same
        // merge-from-str pattern the daemon and build_with use.
        let mut cfg = Config::default();
        cfg.merge(config::File::from_str(
            r#"
[t]
fields = ["title: x |s=A", "author: y |s=XL"]
"#,
            config::FileFormat::Toml,
        ))
        .expect("config");
        let fields = parse_field_specs(&cfg, "t", "fields");
        assert_eq!(fields[0].size, FieldSize::Auto);
        assert_eq!(fields[1].size, FieldSize::XLarge);
    }

    fn field_dy_parses_from_spec() {
        assert_eq!(build_with("a.b: -").fields[0].dy, 0);
        assert_eq!(build_with("a.b: - | d=-3").fields[0].dy, -3);
        assert_eq!(build_with("a.b: - | d=4").fields[0].dy, 4);
        // Alongside other layout keys.
        let f = build_with("a.b: - | s=L r=2 b=1 d=-2");
        assert_eq!(f.fields[0].dy, -2);
        assert_eq!(f.fields[0].row, Some(2));
        assert!(f.fields[0].bold);
        // A malformed value must not panic the parser.
        assert_eq!(build_with("a.b: - | d=xyz").fields[0].dy, 0);
    }

    /// A hidden value must survive a layout suffix. Before the trim fix,
    /// splitting on `|` left a trailing space and `strip_suffix('!')` silently
    /// failed, so `!` was ignored on any field carrying layout metadata.
    #[test]
    fn hidden_value_survives_layout_suffix() {
        assert!(!build_with("a.b: Lbl! | d=2").fields[0].show_value);
        assert!(!build_with("a.b: Lbl! | s=L").fields[0].show_value);
        assert!(build_with("a.b: Lbl").fields[0].show_value);
    }
}
