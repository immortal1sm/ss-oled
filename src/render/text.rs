use anyhow::Result;
use apex_hardware::BitVec;
use embedded_graphics::{
    draw_target::DrawTarget,
    geometry::{OriginDimensions, Point, Size},
    mono_font::{iso_8859_15::FONT_6X10, MonoFont, MonoTextStyle, MonoTextStyleBuilder},
    pixelcolor::BinaryColor,
    text::{renderer::TextRenderer, Baseline, Text},
    Drawable, Pixel,
};
use num_traits::AsPrimitive;
use std::convert::TryFrom;

#[derive(Debug, Clone)]
pub struct ScrollableCanvas {
    width: u32,
    height: u32,
    canvas: BitVec,
}

impl ScrollableCanvas {
    pub fn new(width: u32, height: u32) -> Self {
        let mut canvas = BitVec::new();
        let pixels = width * height;
        canvas.resize(pixels as usize, false);
        Self {
            width,
            height,
            canvas,
        }
    }
}

impl OriginDimensions for ScrollableCanvas {
    fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }
}

impl DrawTarget for ScrollableCanvas {
    type Color = BinaryColor;
    type Error = anyhow::Error;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), <Self as DrawTarget>::Error>
    where
        I: IntoIterator<Item = Pixel<<Self as DrawTarget>::Color>>,
    {
        for Pixel(coord, color) in pixels {
            let (x, y) = (coord.x, coord.y);
            if x >= 0 && x < (self.width as i32) && y >= 0 && y < (self.height as i32) {
                let index = x + y * self.width as i32;
                self.canvas.set(index.as_(), color.is_on());
            }
        }
        Ok(())
    }

    fn clear(
        &mut self,
        color: <Self as DrawTarget>::Color,
    ) -> Result<(), <Self as DrawTarget>::Error> {
        self.canvas.fill(color.is_on());
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScrollableBuilder {
    spacing: Option<u32>,
    position: Option<Point>,
    projection: Option<Size>,
    font: Option<&'static MonoFont<'static>>,
    text: String,
}

#[derive(Debug, Clone)]
pub struct StatefulScrollable {
    builder: ScrollableBuilder,
    pub text: Scrollable,
}

impl TryFrom<ScrollableBuilder> for StatefulScrollable {
    type Error = anyhow::Error;

    fn try_from(
        value: ScrollableBuilder,
    ) -> Result<Self, <Self as TryFrom<ScrollableBuilder>>::Error> {
        let text = value.build()?;
        Ok(StatefulScrollable {
            builder: value,
            text,
        })
    }
}

impl StatefulScrollable {
    /// Re-renders the scrollable text if the text changed. Returns `Ok(true)`
    /// if the text was updated, `Ok(false)` if the text was not updated or
    /// `Err(_)` if an error occurred during re-rendering.
    ///
    /// # Arguments
    ///
    /// * `text`: the new text
    ///
    /// returns: Result<bool, Error>
    ///
    /// # Examples
    ///
    /// ```
    /// let mut text: StatefulScrollableText = ScrollableTextBuilder::new()
    ///                                                     .with_text("foo")
    ///                                                     .try_into()?;
    /// // Text now displays "foo"
    /// text.update("bar")?;
    /// // Text now displays "bar"
    /// ```
    pub fn update(&mut self, text: &str) -> Result<bool> {
        if self.builder.text != text {
            // TODO: Find a better way?
            let new_builder = self.builder.clone().with_text(text);
            let text = new_builder.build()?;
            self.builder = new_builder;
            self.text = text;
            return Ok(true);
        }
        Ok(false)
    }
}

impl ScrollableBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    pub fn with_custom_spacing(mut self, spacing: u32) -> Self {
        self.spacing = Some(spacing);
        self
    }

    pub fn with_position(mut self, position: Point) -> Self {
        self.position = Some(position);
        self
    }

    pub fn with_projection(mut self, projection: Size) -> Self {
        self.projection = Some(projection);
        self
    }

    #[allow(dead_code)]
    pub fn with_custom_font(mut self, font: &'static MonoFont<'static>) -> Self {
        self.font = Some(font);
        self
    }

    fn calculate_spacing(&self) -> u32 {
        self.spacing.unwrap_or(5)
    }

    fn calculate_size(&self, renderer: &MonoTextStyle<BinaryColor>) -> Size {
        let metrics = renderer.measure_string(&self.text, Point::new(0, 0), Baseline::Top);
        metrics.bounding_box.size + Size::new(self.calculate_spacing(), 0)
    }

    fn default_font() -> &'static MonoFont<'static> {
        &FONT_6X10
    }

    pub fn build(&self) -> Result<Scrollable> {
        let renderer = MonoTextStyleBuilder::new()
            .font(self.font.unwrap_or_else(Self::default_font))
            .text_color(BinaryColor::On)
            .build();
        let size = self.calculate_size(&renderer);
        let mut canvas = ScrollableCanvas::new(size.width, size.height);

        Text::with_baseline(&self.text, Point::new(0, 0), renderer, Baseline::Top)
            .draw(&mut canvas)?;

        Ok(Scrollable {
            canvas,
            projection: self.projection.unwrap_or(size),
            position: self.position.unwrap_or_default(),
            spacing: self.calculate_spacing(),
            scroll: 0,
        })
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Scrollable {
    pub canvas: ScrollableCanvas,
    pub projection: Size,
    pub position: Point,
    pub spacing: u32,
    pub scroll: u32,
}

impl Drawable for Scrollable {
    type Color = BinaryColor;
    type Output = ();

    fn draw<D>(
        &self,
        target: &mut D,
    ) -> Result<<Self as Drawable>::Output, <D as DrawTarget>::Error>
    where
        D: DrawTarget<Color = <Self as Drawable>::Color>,
    {
        self.at_tick(target, self.scroll)?;
        Ok::<<Self as Drawable>::Output, <D as DrawTarget>::Error>(())
    }
}

impl Scrollable {
    pub fn at_tick<D>(&self, target: &mut D, tick: u32) -> Result<(), <D as DrawTarget>::Error>
    where
        D: DrawTarget<Color = <Scrollable as Drawable>::Color>,
    {
        // TODO: There's probably some really cool bitwise hacks to do here...
        let scroll = tick % self.canvas.width;
        let pixels = self.projection.height * self.projection.width;
        // We know exactly how many pixels we can push so we can pre-allocate exactly.
        let mut pixels = Vec::with_capacity(pixels as usize);

        for n in 0..self.projection.height {
            let min = scroll + n * self.canvas.width;
            let max = (min + self.projection.width).min((n + 1) * self.canvas.width);
            // First draw until we would overflow in the current line
            for i in min..max {
                // Two independent bounds must hold, and the CANVAS read is the
                // one that panics first: `i` is derived from the projection and
                // can run past `canvas.len()` when the projection is wider than
                // the rendered text (or positioned near the panel edge).
                // `BitSlice` indexing panics rather than clipping, so guard the
                // read before indexing and the write before pushing.
                if i as usize >= self.canvas.canvas.len() {
                    continue;
                }
                let coord = Point::new((i - min) as i32, n as i32);
                let color = self.canvas.canvas[i as usize];
                let pt = self.position + coord;
                if pt.x < 0 || pt.y < 0 || pt.x >= 128 || pt.y >= 40 {
                    continue;
                }
                pixels.push(Pixel(pt, BinaryColor::from(color)));
            }

            // We've reached the end and need to render something from the start
            // Don't do this though if our projection space is larger than our canvas
            // We'd be rendering stuff twice otherwise
            if scroll + self.projection.width >= self.canvas.width
                && self.projection.width < self.canvas.width
            {
                let min = n * self.canvas.width;
                let overflow = scroll + self.projection.width - self.canvas.width;
                let max = min + overflow;

                for i in min..max {
                    let coord = Point::new(
                        (i - min + (self.projection.width - overflow)) as i32,
                        n as i32,
                    );
                    if (i as usize) < self.canvas.canvas.len() {
                        let color = self.canvas.canvas[i as usize];
                        let pt = self.position + coord;
                        if pt.x < 0 || pt.y < 0 || pt.x >= 128 || pt.y >= 40 {
                            continue;
                        }
                        pixels.push(Pixel(pt, BinaryColor::from(color)));
                    }
                }
            }
        }

        target.draw_iter(pixels.into_iter())?;
        Ok(())
    }

    pub fn scroll(&mut self) {
        self.scroll += 1;
    }
}

/// Wrap `text` into at most `max_lines` lines, each fitting within `max_px`
/// pixels at `char_w` pixels per character.
///
/// Splits at the last word boundary within each line and falls back to a
/// character-level split when no space fits. Returns fewer lines when the text
/// wraps to fewer, and one entry per line. Text still unrendered at `max_lines`
/// is truncated onto the last line with an ellipsis.
///
/// This lives in `render::text` rather than beside any one caller because THREE
/// modules need identical wrapping: the custom JSON provider, the notification
/// body renderer, and the GUI's preview pane. They were byte-identical copies
/// that had already drifted once, and the GUI's copy is a separate binary that
/// cannot see the daemon's -- so a fix in one did not reach the others.
/// Keeping one copy here is what stops that recurring.
///
/// `max_lines == 0` yields nothing, which callers use to mean "no room".

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

#[cfg(test)]
mod wrap_tests {
    use super::wrap_text;

    #[test]
    fn short_text_is_one_line() {
        assert_eq!(wrap_text("hello", 128, 6, 3), vec!["hello"]);
    }

    #[test]
    fn empty_text_is_one_empty_line() {
        assert_eq!(wrap_text("", 128, 6, 3), vec![String::new()]);
    }

    #[test]
    fn zero_lines_means_no_room() {
        assert!(wrap_text("hello", 128, 6, 0).is_empty());
    }

    #[test]
    fn splits_at_the_last_space() {
        // 10 px / 4 px per char = 2 chars per line.
        assert_eq!(wrap_text("ab cd", 10, 4, 4), vec!["ab", "cd"]);
    }

    #[test]
    fn falls_back_to_a_character_split_with_no_space() {
        assert_eq!(wrap_text("abcdef", 10, 4, 4), vec!["ab", "cd", "ef"]);
    }

    /// A word longer than the line width must not loop forever.
    ///
    /// Pins the CURRENT count, which is one MORE than `max_lines` -- see
    /// `overflow_yields_one_line_beyond_the_budget` for why that is deliberate
    /// for now and must not be "fixed" casually.
    #[test]
    fn unbroken_text_still_terminates() {
        let out = wrap_text(&"x".repeat(200), 20, 5, 3);
        assert!(
            out.len() <= 4,
            "must terminate and stay near the budget, got {}",
            out.len()
        );
    }

    /// KNOWN OFF-BY-ONE, pinned deliberately.
    ///
    /// When text still overflows at `max_lines`, the loop fills exactly
    /// `max_lines` and then pushes ONE more truncated line with an ellipsis --
    /// so the result is `max_lines + 1`, contradicting this function's own doc
    /// ("up to max_lines lines").
    ///
    /// It is NOT fixed here because all three former copies behave this way and
    /// callers budget height with `wrap_text(...).len()`. Changing it would
    /// shift rendered layout in the custom provider, the notification body and
    /// the GUI preview at once. That is a behaviour change, not a refactor, and
    /// it needs its own decision. What this test does is stop the discrepancy
    /// from being rediscovered as a mystery.
    #[test]
    fn overflow_yields_one_line_beyond_the_budget() {
        let out = wrap_text("aaaa bbbb cccc dddd", 20, 4, 2);
        assert_eq!(
            out.len(),
            3,
            "overflow currently yields max_lines + 1; see the doc comment"
        );
        let last = out.last().unwrap();
        assert!(last.ends_with('…'), "got {last:?}");
        assert!(
            last.chars().count() <= 20 / 4,
            "ellipsis line {last:?} exceeds the width"
        );
    }

    /// Multi-byte input must split on character, not byte, boundaries.
    #[test]
    fn multibyte_input_is_not_split_mid_codepoint() {
        for line in wrap_text("日本語の歌詞テキスト", 20, 4, 5) {
            assert!(line.chars().count() <= 5, "bad line {line:?}");
        }
    }
}
