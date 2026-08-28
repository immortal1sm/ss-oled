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
//! page under the provider name; more fields cycle pages (page dots top-right).

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

#[derive(Clone)]
enum FieldSize {
    Small,
    Medium,
    Large,
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
    values: Arc<Mutex<Vec<(String, String)>>>,
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
            cur = cur.get(key.trim_end_matches('['))?;
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

/// Fetch + resolve all fields (runs inside spawn_blocking).
fn fetch_values(
    source: &str,
    header: &Option<String>,
    fields: &[Field],
) -> Result<Vec<(String, String)>> {
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
    Ok(fields
        .iter()
        .filter(|f| f.show_label || f.show_value)
        .map(|f| {
            let val = get_path(&json, &f.path)
                .map(value_to_string)
                .unwrap_or_else(|| "—".into());
            (f.label.clone(), val)
        })
        .collect())
}

/// Wrap `text` into up to `max_lines` lines, each fitting within
/// `max_px` pixels at `char_w` pixels per character. Splits at the
/// last word boundary within each line; falls back to character-level
/// split when no space fits. Returns fewer lines if the text wraps to
/// fewer than `max_lines`. Returns one entry per line.
fn wrap_text(text: &str, max_px: i32, char_w: i32, max_lines: usize) -> Vec<String> {
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

impl CustomProvider {
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

        // No data retrieved yet — explicit placeholder.
        if values.is_empty() {
            let placeholder = MonoTextStyle::new(&iso_8859_15::FONT_9X15, BinaryColor::On);
            let text = "NO DATA";
            let m = placeholder.measure_string(
                text,
                Point::zero(),
                embedded_graphics::text::Baseline::Top,
            );
            let x = (128 - m.bounding_box.size.width as i32) / 2;
            Text::with_baseline(
                text,
                Point::new(x, y + 4),
                placeholder,
                embedded_graphics::text::Baseline::Top,
            )
            .draw(&mut buffer)?;
            return Ok(buffer);
        }

        // Page dots top-right - only when there are actually multiple
        // pages to indicate between. Single-page panels stay clean.
        let pages = pages_needed(values.len());
        if pages > 1 {
            // Highlight the currently-visible page with a filled square;
            // remaining pages are hollow outlines. Falls back to all
            // hollow if the active page index is out of range (defensive).
            let active = (page as usize) % pages;
            for i in 0..pages as i32 {
                let x = 126 - pages as i32 * 4 + i * 4;
                let style = if i as usize == active {
                    PrimitiveStyle::with_fill(BinaryColor::On)
                } else {
                    PrimitiveStyle::with_stroke(BinaryColor::On, 1)
                };
                Rectangle::new(Point::new(x, 2), Size::new(2, 2))
                    .into_styled(style)
                    .draw(&mut buffer)?;
            }
        }
        if !show_header && y == 0 {
            y = 4;
        }

        // Render plan: one entry per visible field. Explicit `row` slots
        // are reserved first; auto-packed fields fill remaining vertical space.
        let mut plan: Vec<(usize, i32)> = Vec::new();
        let mut taken_rows: [bool; 6] = [false; 6];
        let mut next_auto_y = y;
        for (idx, f) in fields.iter().enumerate() {
            if idx >= values.len() {
                break;
            }
            if !f.show_label && !f.show_value {
                continue;
            }
            let row_y = match f.row {
                Some(r) if r < taken_rows.len() && !taken_rows[r] => {
                    taken_rows[r] = true;
                    let target = match f.size {
                        FieldSize::Large => r as i32 * 14,
                        FieldSize::Medium => r as i32 * 8,
                        FieldSize::Small => r as i32 * 6,
                    };
                    target.max(y)
                }
                _ => {
                    let h = match f.size {
                        FieldSize::Large => 14,
                        FieldSize::Medium => 8,
                        FieldSize::Small => 6,
                    };
                    let t = next_auto_y;
                    next_auto_y += h;
                    t
                }
            };
            plan.push((idx, row_y));
        }

        for (row_idx, row_y) in plan {
            let (label, value) = &values[row_idx];
            let f = &fields[row_idx];
            let style = match f.size {
                FieldSize::Small => MonoTextStyle::new(&iso_8859_15::FONT_4X6, BinaryColor::On),
                FieldSize::Medium => MonoTextStyle::new(&iso_8859_15::FONT_5X7, BinaryColor::On),
                FieldSize::Large => MonoTextStyle::new(&iso_8859_15::FONT_6X10, BinaryColor::On),
            };
            let char_w = match f.size {
                FieldSize::Small => 4,
                FieldSize::Medium => 5,
                FieldSize::Large => 6,
            };
            let line_h = char_w + 2;

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
            // vertical space below row_y. Clamped to 0 so wrap_text is a
            // no-op if there's no room.
            let max_lines = if row_y < 40 {
                ((40 - row_y) / line_h).max(0) as usize
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
                Text::with_baseline(
                    &label_text,
                    Point::new(x_offset, row_y),
                    style,
                    embedded_graphics::text::Baseline::Top,
                )
                .draw(&mut buffer)?;
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
                Text::with_baseline(
                    line,
                    Point::new(value_x, y_pos),
                    style,
                    embedded_graphics::text::Baseline::Top,
                )
                .draw(&mut buffer)?;
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

        Ok(try_stream! {
            // Initial fetch off-thread; placeholder rows until first success.
            {
                let values = Arc::clone(&values);
                let init_name = name.clone();
                let source = source.clone();
                let header = header.clone();
                let fields = fields.clone();
                tokio::task::spawn_blocking(move || {
                    match fetch_values(&source, &header, &fields) {
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
                yield Self::render_rows(
                    &name,
                    &self.values.lock().unwrap(),
                    frames / page_frames,
                    show_header,
                    &self.fields,
                )?;

                frames += 1;
                if last_fetch.elapsed() >= poll_every {
                    last_fetch = Instant::now();
                    let values = Arc::clone(&values);
                    let source = source.clone();
                    let header = header.clone();
                    let fields = fields.clone();
                    tokio::task::spawn_blocking(move || {
                        match fetch_values(&source, &header, &fields) {
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

    let field_specs: Vec<String> = config
        .get_array(&format!("{prefix}.fields"))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.into_str().ok())
        .collect();

    let mut fields = Vec::new();
    for spec in field_specs {
        // "<json-path>: <label>[!]" + optional " | a=L s=M r=N" layout suffix.
        // '!' marks value hidden; " -" in the label slot marks label hidden.
        let mut align = FieldAlign::Left;
        let mut size = FieldSize::Medium;
        let mut row: Option<usize> = None;
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
                        size = match v {
                            "S" | "s" => FieldSize::Small,
                            "M" | "m" => FieldSize::Medium,
                            "L" | "l" => FieldSize::Large,
                            _ => FieldSize::Medium,
                        }
                    }
                    "r" => row = v.parse::<usize>().ok(),
                    _ => {}
                }
            }
        }
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
        });
    }

    if fields.is_empty() {
        return Err(anyhow!("{prefix}.fields is empty — nothing to show"));
    }

    Ok(Some(CustomProvider {
        name: name.to_string(),
        source,
        header,
        fields,
        poll_secs,
        show_header,
        values: Arc::new(Mutex::new(Vec::new())),
    }))
}
