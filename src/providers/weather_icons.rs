//! Procedurally-drawn 1-bit weather icons for the 128x40 OLED.
//!
//! Everything is drawn with primitives (discs, lines, rects) at frame time —
//! no bitmap assets to maintain. Each condition exposes `draw(target, frame)`
//! where `frame` advances ~every 300ms for animation.

use apex_hardware::FrameBuffer;
use embedded_graphics::{
    geometry::Size,
    pixelcolor::BinaryColor,
    prelude::{Point, Primitive},
    primitives::{Circle, Line, PrimitiveStyle, Rectangle},
    Drawable,
};

use crate::providers::weather_data::Condition;

const ON: PrimitiveStyle<BinaryColor> = PrimitiveStyle::with_fill(BinaryColor::On);
const STROKE1: PrimitiveStyle<BinaryColor> = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

fn px(target: &mut FrameBuffer, x: i32, y: i32) {
    Rectangle::new(Point::new(x, y), Size::new(1, 1))
        .into_styled(ON)
        .draw(target)
        .ok();
}

/// Draw a filled circle via scanlines (embedded-graphics Circle works too,
/// but this keeps every icon visually consistent).
fn disc(target: &mut FrameBuffer, cx: i32, cy: i32, r: i32) {
    for dy in -r..=r {
        let half = ((r * r - dy * dy) as f32).sqrt() as i32;
        Rectangle::new(
            Point::new(cx - half, cy + dy),
            Size::new((half * 2 + 1) as u32, 1),
        )
        .into_styled(ON)
        .draw(target)
        .ok();
    }
}

/// Sun with rays. `frame` (0..3) rotates the ray pattern.
pub fn draw_sun(target: &mut FrameBuffer, cx: i32, cy: i32, frame: usize) {
    disc(target, cx, cy, 8);
    // Cut a small inner hole? No — solid sun reads better at this size.

    let step = std::f32::consts::TAU / 8.0;
    let phase = (frame % 4) as f32 * (step / 2.0);
    for i in 0..8 {
        let a = i as f32 * step + phase;
        let inner = 11.0f32;
        let outer = if i % 2 == 0 { 18.0 } else { 14.5 };
        let steps = 10;
        for s in 0..=steps {
            let t = inner + (outer - inner) * (s as f32 / steps as f32);
            px(target, cx + (a.cos() * t) as i32, cy + (a.sin() * t) as i32);
        }
    }
}

/// Cloud outline+fill, roughly 34x16. Returns nothing; drawn at x,y = top-left.
pub fn draw_cloud(target: &mut FrameBuffer, x: i32, y: i32) {
    // Base slab
    Rectangle::new(Point::new(x + 5, y + 9), Size::new(24, 7))
        .into_styled(ON)
        .draw(target)
        .ok();
    // Bumps
    disc(target, x + 10, y + 9, 6);
    disc(target, x + 20, y + 7, 7);
    disc(target, x + 27, y + 11, 5);
}

/// Rain drops falling below a cloud. `frame` shifts drop y positions.
pub fn draw_rain(target: &mut FrameBuffer, cloud_x: i32, cloud_y: i32, frame: usize) {
    draw_cloud(target, cloud_x, cloud_y);
    let base_y = cloud_y + 20;
    for (i, item) in [0usize, 1, 2, 3].iter().enumerate() {
        let dx = cloud_x + 8 + *item as i32 * 7;
        let dy = base_y + ((frame as i32 + *item as i32 * 3) % 12);
        if dy < cloud_y + 38 {
            Line::new(Point::new(dx, dy), Point::new(dx - 1, dy + 3))
                .into_styled(STROKE1)
                .draw(target)
                .ok();
        }
    }
}

/// Lightning bolt flashing below a cloud. `frame` toggles visibility.
pub fn draw_lightning(target: &mut FrameBuffer, cloud_x: i32, cloud_y: i32, frame: usize) {
    draw_cloud(target, cloud_x, cloud_y);
    if frame % 2 == 0 {
        // Bolt: zig-zag polyline from cloud base downward.
        let bx = cloud_x + 17;
        let by = cloud_y + 18;
        let pts = [
            (bx, by),
            (bx - 3, by + 6),
            (bx + 2, by + 7),
            (bx - 2, by + 14),
            (bx + 3, by + 15),
        ];
        for w in pts.windows(2) {
            Line::new(Point::new(w[0].0, w[0].1), Point::new(w[1].0, w[1].1))
                .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 2))
                .draw(target)
                .ok();
        }
    }
}

/// Snow flakes falling below a cloud.
pub fn draw_snow(target: &mut FrameBuffer, cloud_x: i32, cloud_y: i32, frame: usize) {
    draw_cloud(target, cloud_x, cloud_y);
    let base_y = cloud_y + 20;
    for item in [0usize, 1, 2, 3] {
        let dx = cloud_x + 8 + item as i32 * 7;
        let dy = base_y + ((frame as i32 * 2 + item as i32 * 4) % 12);
        if dy < cloud_y + 36 {
            // tiny plus-shaped flake
            px(target, dx, dy);
            px(target, dx.wrapping_sub(1), dy);
            px(target, dx + 1, dy);
            px(target, dx, dy.wrapping_sub(1));
            px(target, dx, dy + 1);
        }
    }
}

/// Fog wisps under a cloud.
pub fn draw_fog(target: &mut FrameBuffer, cloud_x: i32, cloud_y: i32, frame: usize) {
    draw_cloud(target, cloud_x, cloud_y);
    let offset = (frame % 4) as i32;
    for row in 0..3 {
        let y = cloud_y + 21 + row * 5;
        let w = 26 - row * 4;
        let x = cloud_x + 4 + offset.max(0) % 3 - row;
        Rectangle::new(Point::new(x, y), Size::new(w.max(0) as u32, 1))
            .into_styled(ON)
            .draw(target)
            .ok();
    }
}

/// Partly cloudy: sun peeking behind a smaller cloud.
pub fn draw_partly_cloudy(target: &mut FrameBuffer, x: i32, y: i32, frame: usize) {
    draw_sun(target, x + 12, y + 12, frame);
    draw_cloud(target, x + 3, y + 14);
}

/// Dispatch per condition. Icon region occupies roughly the left 44px of the
/// screen; text lives to the right of it.
pub fn draw_condition_icon(target: &mut FrameBuffer, condition: Condition, frame: usize) {
    let fx = 2;
    let fy = 0;
    match condition {
        Condition::Clear => draw_sun(target, fx + 22, fy + 20, frame),
        Condition::PartlyCloudy => draw_partly_cloudy(target, fx, fy, frame),
        Condition::Overcast => draw_cloud(target, fx + 3, fy + 10),
        Condition::Fog => draw_fog(target, fx + 3, fy + 4, frame),
        Condition::Rain => draw_rain(target, fx + 3, fy + 4, frame),
        Condition::Snow => draw_snow(target, fx + 3, fy + 4, frame),
        Condition::Thunderstorm => draw_lightning(target, fx + 3, fy + 4, frame),
    }
}
