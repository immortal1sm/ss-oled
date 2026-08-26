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
struct Field {
    path: String,
    label: String,
    /// Draw the label text before the value on the OLED.
    show_label: bool,
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
        .map(|f| {
            let val = get_path(&json, &f.path)
                .map(value_to_string)
                .unwrap_or_else(|| "—".into());
            (f.label.clone(), val)
        })
        .collect())
}

impl CustomProvider {
    fn render_rows(
        name: &str,
        values: &[(String, String)],
        page: u64,
        show_header: bool,
        labels_enabled: &[bool],
    ) -> Result<FrameBuffer> {
        let mut buffer = FrameBuffer::new();
        let header_style = MonoTextStyle::new(&iso_8859_15::FONT_6X10, BinaryColor::On);
        let row_style = MonoTextStyle::new(&iso_8859_15::FONT_5X7, BinaryColor::On);

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

        // Page dots always top-right.
        let pages = pages_needed(values.len());
        for i in 0..pages as i32 {
            let x = 126 - pages as i32 * 4 + i * 4;
            Rectangle::new(Point::new(x, 2), Size::new(2, 2))
                .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 1))
                .draw(&mut buffer)?;
        }
        if !show_header && y == 0 {
            y = 4;
        }

        let start = (page as usize % pages) * PER_PAGE;
        for (row, ((label, value), show_label)) in values
            .iter()
            .zip(labels_enabled.iter().chain(std::iter::repeat(&true)))
            .skip(start)
            .take(PER_PAGE)
            .enumerate()
        {
            let show_label = *show_label;
            if show_label {
                Text::with_baseline(
                    format!("{label}:").as_str(),
                    Point::new(0, y),
                    row_style,
                    embedded_graphics::text::Baseline::Top,
                )
                .draw(&mut buffer)?;
            }
            let vx = if show_label { 64 } else { 0 };
            Text::with_baseline(
                value.as_str(),
                Point::new(vx, y),
                row_style,
                embedded_graphics::text::Baseline::Top,
            )
            .draw(&mut buffer)?;
            y += 8;
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
        let labels_enabled: Vec<bool> = self.fields.iter().map(|f| f.show_label).collect();

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
            let labels_enabled: Vec<bool> =
                self.fields.iter().map(|f| f.show_label).collect();

            loop {
                yield Self::render_rows(
                    &name,
                    &self.values.lock().unwrap(),
                    frames / page_frames,
                    show_header,
                    &labels_enabled,
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
        // "<json-path>: <label>" — label optional. An empty label means the
        // row shows the bare value with no label text.
        let (path, label) = match spec.split_once(':') {
            Some((p, l)) => {
                let l = l.trim().to_string();
                // "-" sentinel: user chose to hide this row's label.
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
            label,
            show_label: true,
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
