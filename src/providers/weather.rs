//! Today's weather: animated condition icon + temp + precip chance + city
//! label. Refreshes from Open-Meteo every 15 min via WeatherCache.

use crate::{
    providers::weather_data::{Units, WeatherCache},
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
    geometry::Point,
    mono_font::{iso_8859_15, MonoTextStyle},
    pixelcolor::BinaryColor,
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

struct Weather {
    cache: std::sync::Arc<WeatherCache>,
    units: Units,
    label: String,
    /// Seconds the today view stays on screen (weather.duration).
    today_secs: u64,
    /// Seconds for the whole 5-day forecast cycle; each page gets 1/5
    /// (weather.forecast_duration).
    forecast_secs: u64,
}

impl Weather {
    fn render(&self, anim_frame: usize) -> Result<FrameBuffer> {
        let mut buffer = FrameBuffer::new();
        let data = self.cache.get();

        let (condition, temp, precip) = match &data {
            Some(d) => (
                d.current_condition
                    .unwrap_or(crate::providers::weather_data::Condition::Overcast),
                Some(d.current_temp),
                Some(d.current_precip_prob),
            ),
            None => (
                crate::providers::weather_data::Condition::PartlyCloudy,
                None,
                None,
            ),
        };

        draw_condition_icon(&mut buffer, condition, anim_frame);

        // Layout — panel is only 40px tall, everything must end by y=39.
        // Icon occupies x=0..44 (vertically centered).
        //   y=1..20    "25°C"   — FONT_10X20
        //   y=22..36   "Rain"   — FONT_9X15
        //   y=37..46 → clipped! Put precip beside condition instead.
        let text_x = 46;

        if let Some(t) = temp {
            let style = MonoTextStyle::new(&iso_8859_15::FONT_10X20, BinaryColor::On);
            // Degree symbol: iso_8859_15 has ° at 0xB0. embedded-graphics
            // mono fonts render any char in their glyph range.
            let text = format!("{:.0}{}", t, self.units.symbol());
            Text::with_baseline(&text, Point::new(text_x, 1), style, Baseline::Top)
                .draw(&mut buffer)?;
        }

        let mid = MonoTextStyle::new(&iso_8859_15::FONT_9X15, BinaryColor::On);
        let small = MonoTextStyle::new(&iso_8859_15::FONT_6X10, BinaryColor::On);

        // Condition label at y=22..36
        Text::with_baseline(
            condition.label(),
            Point::new(text_x, 22),
            mid,
            Baseline::Top,
        )
        .draw(&mut buffer)?;

        // Precip chance on the SAME row as the condition, right-aligned to
        // the panel edge — no room for a third text band on a 40px panel.
        if let Some(p) = precip {
            if p > 0 {
                let text = format!("{}%", p);
                let m = small.measure_string(&text, Point::zero(), Baseline::Top);
                let px = (128 - m.bounding_box.size.width as i32).max(text_x + 60);
                Text::with_baseline(&text, Point::new(px, 24), small, Baseline::Top)
                    .draw(&mut buffer)?;
            }
        }

        Ok(buffer)
    }
}

impl ContentProvider for Weather {
    type ContentStream<'a> = impl Stream<Item = Result<FrameBuffer>> + 'a;

    fn stream(&mut self) -> Result<<Self as ContentProvider>::ContentStream<'_>> {
        info!("Registering Weather display source (today + 5-day forecast combined).");

        Ok(try_stream! {
            use tokio::time::{interval, Duration, MissedTickBehavior};
            // 50ms master tick: smooth push-slides (~10fps during transitions);
            // icon animation advances every 6th tick (~300ms).
            let mut tick = interval(Duration::from_millis(50));
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

            // Configurable via [weather] duration / forecast_duration.
            let today_ms = self.today_secs.saturating_mul(1000).max(1_000);
            let page_ms = (self.forecast_secs.saturating_mul(1000) / 5).max(500);
            let slide_steps: i32 = 12;

            let mut anim = 0usize;
            let mut subframe = 0u32;
            let mut phase_ms = 0u64;
            // Phase::Today | Phase::Day(page) | mid-transition state:
            let mut in_forecast = false;
            let mut page = 0usize;          // 1..=5 (day index into days[])
            let mut trans_step = 0i32;
            let mut transitioning = false;
            let mut prev_page = 0usize;

            loop {
                let data = self.cache.get();

                let frame = if !in_forecast {
                    // Today view with animated icon.
                    self.render(anim)?
                } else if transitioning {
                    // Push transition between forecast pages.
                    let mut buffer = FrameBuffer::new();
                    if let Some(d) = &data {
                        let step_w = 128 / slide_steps;
                        let progress = trans_step + 1;
                        let out_off = -progress * step_w;
                        let in_off = 128 - progress * step_w;
                        let out_day = if prev_page == 0 { 0 } else { prev_page };
                        if out_off > -128 {
                            let _ = crate::providers::forecast::render_day(
                                &mut buffer, &d.days, out_day, out_off,
                                self.units.symbol(),
                            );
                        }
                        if in_off < 128 {
                            let _ = crate::providers::forecast::render_day(
                                &mut buffer, &d.days, page.max(1), in_off,
                                self.units.symbol(),
                            );
                        }
                    }
                    buffer
                } else {
                    // Static forecast page for this day.
                    let mut buffer = FrameBuffer::new();
                    if let Some(d) = &data {
                        let _ = crate::providers::forecast::render_day(
                            &mut buffer, &d.days, page.max(1), 0,
                            self.units.symbol(),
                        );
                    }
                    buffer
                };

                yield frame;
                // Icon animation ticks at ~300ms; slides run every tick.
                if subframe % 6 == 0 && !transitioning {
                    anim = anim.wrapping_add(1);
                }
                subframe = subframe.wrapping_add(1);
                tick.tick().await;
                phase_ms += 50;

                if transitioning {
                    trans_step += 1;
                    if trans_step >= slide_steps {
                        transitioning = false;
                        phase_ms = 0;
                    }
                } else if !in_forecast && phase_ms >= today_ms {
                    // Enter forecast cycle at tomorrow (day index 1).
                    in_forecast = true;
                    page = 1;
                    phase_ms = 0;
                } else if in_forecast && phase_ms >= page_ms {
                    // Slide to the next day; after day 5, back to today.
                    let next = if page >= 5 { 0 } else { page + 1 };
                    prev_page = page;
                    page = next;
                    transitioning = true;
                    trans_step = 0;
                    if next == 0 {
                        // Finished the cycle: return to today view after slide.
                        in_forecast = false;
                    }
                }
            }
        })
    }

    fn name(&self) -> &'static str {
        "weather"
    }
}

fn register_callback(config: &Config, _focus_tx: FocusChannel) -> Result<Box<dyn ContentWrapper>> {
    info!("Registering Weather display source.");

    let enabled = config.get_bool("weather.enabled").unwrap_or(false);
    let today_secs = config.get_int("weather.duration").unwrap_or(10).max(1) as u64;
    let forecast_secs = config
        .get_int("weather.forecast_duration")
        .unwrap_or(30)
        .max(5) as u64;
    if !enabled {
        anyhow::bail!("weather provider disabled");
    }

    let cache = std::sync::Arc::new(
        WeatherCache::from_config(config)
            .map_err(|e| anyhow::anyhow!("weather config error: {}", e))?,
    );
    let label: String = config.get_str("weather.label").unwrap_or_default();

    let units = Units::from_config(config);
    Ok(Box::new(Weather {
        cache,
        units,
        label,
        today_secs,
        forecast_secs,
    }))
}
