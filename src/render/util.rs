use embedded_graphics::{
    pixelcolor::BinaryColor,
    prelude::Pixel,
    prelude::{Angle, AngleUnit, DrawTarget, Point, Primitive},
    primitives::{Arc, PrimitiveStyle},
    Drawable,
};

/// A 1px progress frame drawn around the edge of the display.
///
/// Used instead of the corner ring for the notification countdown: the ring
/// occupied a 12x12 box in the bottom-right that text had to be kept clear of,
/// while the frame lives entirely on the outermost pixel ring, so the whole
/// panel interior stays available to content.
///
/// Progress runs clockwise from the top-left corner; the remaining border is
/// simply not drawn (a 1-bit panel has no dim state).
pub struct EdgeProgress {
    maximum_value: f32,
    width: u32,
    height: u32,
}

impl EdgeProgress {
    pub fn new(width: u32, height: u32, max: impl Into<f32>) -> Self {
        Self {
            maximum_value: max.into(),
            width,
            height,
        }
    }

    /// Total pixels in the outermost ring of the panel.
    fn perimeter(&self) -> u32 {
        2 * (self.width + self.height) - 4
    }

    /// The `n`th border pixel, walking clockwise from the top-left corner.
    fn pixel(&self, n: u32) -> (i32, i32) {
        let w = self.width;
        let h = self.height;
        let last = self.perimeter() - 1;
        let n = n.min(last);
        // Segment boundaries, in pixels: top row, right column, bottom row
        // (walked right-to-left), left column (walked bottom-to-top).
        let top = w;
        let right = top + h - 1;
        let bottom = right + w - 1;
        if n < top {
            (n as i32, 0)
        } else if n < right {
            (w as i32 - 1, (n - top) as i32)
        } else if n < bottom {
            (w as i32 - 1 - (n - right) as i32, h as i32 - 1)
        } else {
            (0, h as i32 - 1 - (n - bottom) as i32)
        }
    }

    pub fn draw_at<T: DrawTarget<Color = BinaryColor>>(
        &self,
        current: impl Into<f32>,
        target: &mut T,
    ) -> Result<(), <T as DrawTarget>::Error> {
        let frac = (current.into() / self.maximum_value).clamp(0.0, 1.0);
        let lit = (frac * self.perimeter() as f32) as u32;
        for n in 0..lit {
            let (x, y) = self.pixel(n);
            target.draw_iter(std::iter::once(Pixel(
                Point::new(x, y),
                BinaryColor::On,
            )))?;
        }
        Ok(())
    }
}

pub struct ProgressBar {
    maximum_value: f32,
    origin: Point,
    style: PrimitiveStyle<BinaryColor>,
}

impl ProgressBar {
    const DIAMETER: u32 = 10;

    pub fn new(origin: Point, max: impl Into<f32>) -> Self {
        let style = PrimitiveStyle::with_stroke(BinaryColor::On, 2);
        Self {
            maximum_value: max.into(),
            origin,
            style,
        }
    }

    fn calculate_progress(&self, current: f32) -> Angle {
        (-((current / self.maximum_value) * 360.0)).deg()
    }

    pub fn draw_at<T: DrawTarget<Color = BinaryColor>>(
        &self,
        current: impl Into<f32>,
        target: &mut T,
    ) -> Result<(), <T as DrawTarget>::Error> {
        let progress = self.calculate_progress(current.into());
        Arc::new(self.origin, Self::DIAMETER, 90.0_f32.deg(), progress)
            .into_styled(self.style)
            .draw(target)?;
        Ok(())
    }
}
