//! Forecast page rendering shared with the weather provider's combined
//! cycle (today view -> day 2..6 push slides).

use crate::providers::weather_icons::draw_condition_icon;
use anyhow::Result;
use apex_hardware::FrameBuffer;
use embedded_graphics::{
    geometry::Size,
    mono_font::{iso_8859_15, MonoTextStyle},
    pixelcolor::BinaryColor,
    prelude::{Point, Primitive},
    primitives::{PrimitiveStyle, Rectangle},
    text::{renderer::TextRenderer, Baseline, Text},
    Drawable,
};

pub(crate) fn render_day(
    buffer: &mut FrameBuffer,
    days: &[crate::providers::weather_data::DayForecast],
    idx: usize,
    offset_x: i32,
    unit_sym: &str,
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
    let hi = format!("{:.0}{}", day.temp_max, unit_sym);
    Text::with_baseline(&hi, Point::new(tx, 1), big, Baseline::Top).draw(buffer)?;

    // Lo temp directly below hi (y=22..31)
    let lo = format!("{:.0}{}", day.temp_min, unit_sym);
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
