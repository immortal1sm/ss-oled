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
            None => (crate::providers::weather_data::Condition::Fog, None, None),
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
        info!("Registering Weather display source.");

        let mut interval = tokio::time::interval(std::time::Duration::from_millis(300));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        Ok(try_stream! {
            let mut frame = 0usize;
            loop {
                if let Ok(image) = self.render(frame) {
                    yield image;
                }
                frame = frame.wrapping_add(1);
                interval.tick().await;
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
    }))
}
