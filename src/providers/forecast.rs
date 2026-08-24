//! 5-day forecast: pages through the next days with a horizontal slide
//! transition. Day 0 (today) is shown by the weather provider; this one
//! covers days 1..=5.

use crate::{
    providers::weather_data::WeatherCache,
    render::{
        display::ContentProvider,
        scheduler::{ContentWrapper, FocusChannel, CONTENT_PROVIDERS},
    },
};
use anyhow::Result;
use apex_hardware::FrameBuffer;
use async_stream::try_stream;
use config::Config;
use embedded_graphics::{
    mono_font::{iso_8859_15, MonoTextStyle},
    pixelcolor::BinaryColor,
    prelude::{Point, Primitive},
    primitives::{PrimitiveStyle, Rectangle},
    text::{renderer::TextRenderer, Baseline, Text},
    Drawable,
};
use futures::Stream;
use linkme::distributed_slice;
use log::info;

use super::weather_icons::draw_condition_icon;

#[distributed_slice(CONTENT_PROVIDERS)]
static PROVIDER_INIT: fn(&Config, FocusChannel) -> Result<Box<dyn ContentWrapper>> =
    register_callback;

const PAGE_MS: u64 = 3000; // per-day dwell
const SLIDE_STEPS: i32 = 6; // intermediate frames for slide transition
const SLIDE_STEP_W: i32 = 128 / SLIDE_STEPS as i32; // px per slide frame

struct Forecast {
    cache: std::sync::Arc<WeatherCache>,
}

fn day_name(days_ahead: usize) -> &'static str {
    // Stable & short — fits FONT_6X10 at a glance.
    match days_ahead {
        1 => "TOMORROW",
        _ => "",
    }
}

/// Render day `idx` of the cached data onto `buffer`. If `slide_from` is
/// Some(offset_px), the whole frame is drawn shifted right by that amount
/// (the incoming-slide animation).
fn render_day(
    buffer: &mut FrameBuffer,
    days: &[crate::providers::weather_data::DayForecast],
    idx: usize,
    offset_x: i32,
) -> Result<()> {
    let day = match days.get(idx) {
        Some(d) => d,
        None => return Ok(()),
    };

    // Layout — panel is only 40px tall, everything must end by y=39.
    //   x=0..44    condition icon (vertically centered)
    //   x=46+      right column: hi temp y=1..21, lo temp y=22..31,
    //              precip % beside lo temp
    //   y=30..39   day header centered at bottom, page dots to its right
    let small = MonoTextStyle::new(&iso_8859_15::FONT_6X10, BinaryColor::On);
    let big = MonoTextStyle::new(&iso_8859_15::FONT_10X20, BinaryColor::On);
    let mid = MonoTextStyle::new(&iso_8859_15::FONT_6X10, BinaryColor::On);

    // Condition icon in the left region (x=0..44), vertically centered.
    // Static (frame 0) — the forecast pages through days every ~3s, so
    // animating here would restart each icon's cycle constantly.
    draw_condition_icon(buffer, day.condition, 0);

    // Hi temp: right column, starts near top. FONT_10X20 renders ~20px tall.
    let tx = 46 + offset_x;
    let hi = format!("{:.0}\u{00B0}C", day.temp_max);
    Text::with_baseline(&hi, Point::new(tx, 1), big, Baseline::Top).draw(buffer)?;

    // Lo temp directly below hi (y=22..31)
    let lo = format!("{:.0}\u{00B0}C", day.temp_min);
    Text::with_baseline(&lo, Point::new(tx, 22), small, Baseline::Top).draw(buffer)?;

    // Precip chance on same band as lo, to its right ("72%" ≈ 18px wide,
    // lo is up to "-99°C" ≈ 50px wide ending at x≈96; place at x=100).
    if day.precip_prob > 0 {
        let p = format!("{}%", day.precip_prob);
        Text::with_baseline(&p, Point::new(102 + offset_x, 22), small, Baseline::Top)
            .draw(buffer)?;
    }

    // Day header bottom-left under the icon area, y=30..39 (last safe row).
    // Shows the short weekday label (M|T|W|TH|F|ST|S) for this day.
    let header = day.day_label();
    let m = mid.measure_string(header, Point::zero(), Baseline::Top);
    let hx = (128 - m.bounding_box.size.width as i32) / 2 + offset_x;
    Text::with_baseline(header, Point::new(hx, 30), mid, Baseline::Top).draw(buffer)?;

    // Page dots: right side of the header row (y=33..37)
    let total = 5usize;
    let page = idx.saturating_sub(1).min(total - 1);
    for d in 0..total {
        let cx = 104 + d as i32 * 5 + offset_x;
        let filled = d == page;
        let size: u32 = if filled { 5 } else { 3 };
        Rectangle::new(
            Point::new(cx, 33),
            embedded_graphics::geometry::Size::new(size, size),
        )
        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
        .draw(buffer)
        .ok();
    }

    Ok(())
}

impl Forecast {
    fn render_page(
        &self,
        prev: Option<(usize, i32)>,
        page: usize,
        slide_offset: Option<i32>,
    ) -> Result<FrameBuffer> {
        let _ = prev;
        let mut buffer = FrameBuffer::new();
        if let Some(data) = self.cache.get() {
            if data.days.len() > 1 {
                render_day(&mut buffer, &data.days, page + 1, slide_offset.unwrap_or(0))?;
            }
        }
        Ok(buffer)
    }

    /// Full push-transition frame: outgoing day at `out_offset` (negative =
    /// moving left), incoming at `in_offset` (positive = entering from
    /// right). Both pages' icons AND text shift together.
    fn render_transition(
        &self,
        out_page: usize,
        in_page: usize,
        progress: i32,
    ) -> Result<FrameBuffer> {
        let mut buffer = FrameBuffer::new();
        if let Some(data) = self.cache.get() {
            if data.days.len() <= 1 {
                return Ok(buffer);
            }
            let step_w = 128 / SLIDE_STEPS;
            // Outgoing shifts left off-screen; incoming enters from right.
            let out_off = -progress * step_w;
            let in_off = 128 - progress * step_w;
            if out_off > -128 {
                render_day(&mut buffer, &data.days, out_page + 1, out_off)?;
            }
            if in_off < 128 {
                render_day(&mut buffer, &data.days, in_page + 1, in_off)?;
            }
        }
        Ok(buffer)
    }
}

impl ContentProvider for Forecast {
    type ContentStream<'a> = impl Stream<Item = Result<FrameBuffer>> + 'a;

    fn stream(&mut self) -> Result<<Self as ContentProvider>::ContentStream<'_>> {
        info!("Registering Forecast display source.");

        Ok(try_stream! {
            use tokio::time::{interval, Duration, MissedTickBehavior};
            let mut tick = interval(Duration::from_millis(50));
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

            let mut page = 0usize;
            let mut page_ms = 0u64;
            let mut transitioning = false;
            let mut trans_step = 0i32;
            let mut next_page = 0usize;

            loop {
                // Emit frames.
                let frame = if transitioning {
                    // Push transition: old page slides left, new page enters
                    // from the right — icons, drops, bolts and all.
                    self.render_transition(page, next_page, trans_step + 1)?
                } else {
                    self.render_page(None, page, None)?
                };
                yield frame.clone();

                // Advance state machine every tick (50ms).
                tick.tick().await;
                page_ms += 50;

                if transitioning {
                    trans_step += 1;
                    if trans_step >= SLIDE_STEPS {
                        transitioning = false;
                        page = next_page;
                        page_ms = 0;
                    }
                } else if page_ms >= PAGE_MS {
                    next_page = (page + 1) % 5;
                    transitioning = true;
                    trans_step = 0;
                }
            }
        })
    }

    fn name(&self) -> &'static str {
        "forecast"
    }
}

fn register_callback(config: &Config, _focus_tx: FocusChannel) -> Result<Box<dyn ContentWrapper>> {
    info!("Registering Forecast display source.");

    let enabled = config.get_bool("forecast.enabled").unwrap_or(false);
    if !enabled {
        anyhow::bail!("forecast provider disabled");
    }
    let cache = std::sync::Arc::new(
        WeatherCache::from_config(config)
            .map_err(|e| anyhow::anyhow!("forecast config error: {}", e))?,
    );

    Ok(Box::new(Forecast { cache }))
}
