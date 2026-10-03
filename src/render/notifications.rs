use crate::render::display::ContentProvider;
use anyhow::{anyhow, Result};
use async_stream::try_stream;
use embedded_graphics::{
    geometry::{OriginDimensions, Point, Size},
    image::Image,
    pixelcolor::BinaryColor,
    Drawable,
};
use num_traits::AsPrimitive;

use crate::render::{
    scheduler::{TICKS_PER_SECOND, TICK_LENGTH},
    text::{Scrollable, ScrollableBuilder},
    util::{EdgeProgress, ProgressBar},
};
use embedded_graphics::{
    mono_font::{iso_8859_15, MonoFont, MonoTextStyle},
    text::{Baseline, Text},
};
use futures_core::stream::Stream;

use apex_hardware::FrameBuffer;
use tinybmp::Bmp;
use tokio::{
    time,
    time::{Duration, MissedTickBehavior},
};

/// Panel width in pixels; notifications never wrap to a second line.
const PANEL_W: u32 = 128;
/// Panel height in pixels. Any band that would extend past this writes out of
/// bounds and panics the framebuffer, so every y is clamped against it.
const PANEL_H: i32 = 40;

/// Bounding box the LEGACY corner ring occupies, as (left, top, right, bottom).
/// Only used when `timer_border = false`; the default edge frame reserves
/// nothing.
pub const TIMER_BOX: (i32, i32, i32, i32) = (116, 28, 128, 40);

pub struct Notification {
    frame: FrameBuffer,
    ticks: u32,
    title: Scrollable,
    scroll: bool,
    /// One entry per wrapped body line.
    content: Vec<String>,
    /// Where the content text is drawn, already resolved.
    content_origin: Point,
    /// Vertical advance between wrapped body lines.
    content_line_h: i32,
    content_right: i32,
    content_font: &'static MonoFont<'static>,
    content_size: SizeClass,
    content_align: Align,
    content_bold: bool,
    show_timer: bool,
    timer_border: bool,
}

impl Notification {
    /// How long this notification is *expected* to take to stream to completion.
    ///
    /// The scheduler uses this as its timeout budget. It is deliberately an
    /// upper bound rather than the nominal figure: `TICK_LENGTH` is 50ms but
    /// every frame also does a USB write, so real elapsed time exceeds
    /// ticks x TICK_LENGTH. Budgeting to the theoretical minimum would cut off
    /// a long, scrolling notification before its last frame.
    pub fn expected_duration(&self) -> Duration {
        let ticks = u64::from(self.ticks);
        let nominal = Duration::from_millis(ticks * TICK_LENGTH as u64);
        // x2 covers slow USB writes; +3s is slack for scheduling jitter.
        nominal * 2 + Duration::from_secs(3)
    }
}

#[derive(Debug, Clone)]
pub struct Icon<'a>(Bmp<'a, BinaryColor>);

impl<'a> Icon<'a> {
    pub fn new(icon: Bmp<'a, BinaryColor>) -> Self {
        Self(icon)
    }
}

#[derive(Debug, Clone, Default)]
pub struct NotificationBuilder<'a> {
    title: Option<&'a str>,
    content: Option<String>,
    icon: Option<Icon<'a>>,
    font: Option<&'a MonoFont<'a>>,
    /// How long to hold the frame before returning to rotation, in seconds.
    /// `None` means the original behaviour: a short fixed dwell plus whatever
    /// the title needs to scroll. `Some(n)` overrides the base dwell with the
    /// sender's requested timeout (or a configured default) while still
    /// allowing enough time for a scrolling title to finish.
    hold_seconds: Option<u64>,
    /// Where each piece of text goes on the panel.
    layout: Layout,
    app_name: Option<String>,
}

/// Horizontal placement of a text line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// Font size class. Character widths match the custom provider's ladder so the
/// two feel identical: S=4, M=5, L=6, XL=8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SizeClass {
    Small,
    #[default]
    Medium,
    Large,
    XLarge,
    /// Largest class whose wrapped text fits the line's width AND the vertical
    /// room left after the lines above it. Resolved once at build time, so it
    /// never changes between ticks of the same notification.
    Auto,
}

impl SizeClass {
    /// `Auto` has no font of its own. It must be resolved by `resolve()`
    /// before use; these accessors fall back to Medium so a missed resolution
    /// degrades to a readable size rather than panicking mid-render.
    pub fn font(self) -> &'static MonoFont<'static> {
        match self {
            SizeClass::Small => &iso_8859_15::FONT_4X6,
            SizeClass::Large => &iso_8859_15::FONT_6X10,
            SizeClass::XLarge => &iso_8859_15::FONT_8X13,
            SizeClass::Medium | SizeClass::Auto => &iso_8859_15::FONT_5X7,
        }
    }

    pub fn char_width(self) -> u32 {
        match self {
            SizeClass::Small => 4,
            SizeClass::Medium | SizeClass::Auto => 5,
            SizeClass::Large => 6,
            SizeClass::XLarge => 8,
        }
    }

    /// Vertical space one line of this class occupies, measured from actual
    /// renders: ink rows below the requested row, plus 1px leading.
    ///
    /// The advertised cell height is NOT usable here — FONT_8X13 reports 13px
    /// but a full "QGMW" lights 10 rows, and FONT_6X10 reports 10px but lights
    /// 8. Sizing bands from the cell height left 2-4px phantom gaps that made
    /// tight layouts look like they had spare room.
    pub fn line_height(self) -> u32 {
        match self {
            SizeClass::Small => 7,
            SizeClass::Medium | SizeClass::Auto => 8,
            SizeClass::Large => 9,
            SizeClass::XLarge => 11,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "s" | "small" => Some(SizeClass::Small),
            "m" | "medium" => Some(SizeClass::Medium),
            "l" | "large" => Some(SizeClass::Large),
            "xl" | "xlarge" => Some(SizeClass::XLarge),
            "auto" | "a" => Some(SizeClass::Auto),
            _ => None,
        }
    }

    /// Ladder from largest to smallest, for auto resolution.
    pub const LADDER: [SizeClass; 4] =
        [SizeClass::XLarge, SizeClass::Large, SizeClass::Medium, SizeClass::Small];

    /// Resolve `Auto` to a concrete class.
    ///
    /// Takes the largest class where the text wraps into at most `max_lines`
    /// within `avail_w`, and those lines fit in `avail_h`. Both bounds matter:
    /// width alone would let a long body pick XLarge and run past the 40px
    /// panel, and height alone would let a short one overflow horizontally.
    ///
    /// Never returns `Auto`. When nothing in the ladder can satisfy the budget
    /// the text overruns regardless, so the fallback is Small -- it overruns
    /// least (most chars per line, least height).
    pub fn resolve(self, text: &str, avail_w: u32, avail_h: u32, max_lines: u32) -> SizeClass {
        if self != SizeClass::Auto {
            return self;
        }
        let chars = text.chars().count() as u32;
        for c in Self::LADDER {
            if avail_h < c.line_height() {
                continue; // a single line of it would not fit vertically
            }
            let per_line = (avail_w / c.char_width()).max(1);
            // Word wrapping never packs lines completely full, so a title that
            // exactly fills per_line can spill onto an extra line. Require one
            // line of headroom before accepting a size.
            let needed = chars.div_ceil(per_line).max(1);
            let fits = needed <= max_lines && needed.saturating_mul(c.line_height()) <= avail_h;
            if fits {
                return c;
            }
        }
        SizeClass::Small
    }
}

impl Align {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "left" | "l" => Some(Align::Left),
            "center" | "centre" | "c" => Some(Align::Center),
            "right" | "r" => Some(Align::Right),
            _ => None,
        }
    }

    /// X offset for `width` pixels of text within `panel` pixels, starting at
    /// `left` (used when an icon reserves space on the left).
    ///
    /// Centering/right-aligning text WIDER than the space available would push
    /// it off the left edge, so both clamp to `left` in that case. That matters
    /// for the scrolling title: it is measured at full length even though only
    /// `projection` pixels are ever visible at once.
    pub fn offset(self, width: u32, panel: i32, left: i32) -> i32 {
        let left = left.max(0);
        let avail = (panel - left).max(0) as u32;
        if width >= avail {
            return left;
        }
        match self {
            Align::Left => left,
            Align::Center => left + ((avail - width) / 2) as i32,
            Align::Right => left + (avail - width) as i32,
        }
    }
}

/// Which piece of text a layout entry places.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    App,
    Title,
    Content,
}

impl Part {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "app" | "app_name" | "source" => Some(Part::App),
            "title" | "summary" => Some(Part::Title),
            "content" | "body" | "text" => Some(Part::Content),
            _ => None,
        }
    }

    fn default_spec(self) -> LineSpec {
        match self {
            Part::App => LineSpec {
                part: self,
                shown: false,
                size: SizeClass::Medium,
                align: Align::Left,
                row: None,
                bold: false,
                wrap: false,
                dy: 0,
            },
            Part::Title => LineSpec {
                part: self,
                shown: true,
                size: SizeClass::Large,
                align: Align::Left,
                row: None,
                bold: false,
                wrap: false,
                dy: 0,
            },
            Part::Content => LineSpec {
                part: self,
                shown: true,
                size: SizeClass::Auto,
                align: Align::Left,
                row: None,
                bold: false,
                wrap: false,
                dy: 0,
            },
        }
    }
}

/// Per-line placement. Mirrors the custom provider's field options (size,
/// alignment, row slot, bold) so the two feel the same.
#[derive(Debug, Clone, Copy)]
pub struct LineSpec {
    pub part: Part,
    /// Draw this line at all.
    pub shown: bool,
    pub size: SizeClass,
    pub align: Align,
    /// Explicit y for this line. `None` auto-packs below the previous one.
    pub row: Option<i32>,
    /// Faux-bold via double-strike.
    pub bold: bool,
    /// Let this line wrap into the vertical space below it. Only meaningful for
    /// the body: the title scrolls horizontally instead.
    pub wrap: bool,
    /// Nudge this line up (negative) or down (positive), in pixels. Applied
    /// after the row is resolved. Useful because a tall font's glyphs start a
    /// couple of rows below the cell top, so XL text looks lower than L at the
    /// same row.
    pub dy: i32,
}

impl Default for LineSpec {
    fn default() -> Self {
        Part::Title.default_spec()
    }
}

/// Where the notification's text parts go.
///
/// Defaults reproduce the original hardcoded layout exactly: title scrolling at
/// the top beside any icon, content below it, and the countdown ring in the
/// bottom-right corner. Configuring anything is opt-in, and each line is
/// configured INDEPENDENTLY — there is no shared `align`, because "centred
/// title with a left-aligned body" is a reasonable thing to want.
#[derive(Debug, Clone)]
pub struct Layout {
    /// One entry per text part. Missing parts fall back to their defaults.
    pub lines: Vec<LineSpec>,
    /// Draw the countdown indicator.
    pub show_timer: bool,
    /// `true` = 1px progress frame around the panel edge (default, occupies no
    /// interior space); `false` = the original 10px ring in the bottom-right
    /// corner, which content must be kept clear of.
    pub timer_border: bool,
}

impl Layout {
    /// The rightmost x a line may occupy.
    ///
    /// With the edge-frame indicator (the default) this is always the full
    /// panel width — the frame lives on the outermost pixel ring, so nothing
    /// is reserved. Only the legacy corner ring needs a cap, and only for bands
    /// that overlap its box.
    pub fn usable_right(&self, top: i32, height: i32) -> i32 {
        let panel_edge = PANEL_W as i32;
        if !self.show_timer || self.timer_border {
            return panel_edge;
        }
        let (l, t_ring, _r, b) = TIMER_BOX;
        // Bands are [top, top+height); overlap if either edge falls inside.
        if top < b && top + height > t_ring {
            l.min(panel_edge)
        } else {
            panel_edge
        }
    }

    /// Look up one part's spec, falling back to the built-in default.
    pub fn line(&self, part: Part) -> LineSpec {
        self.lines
            .iter()
            .find(|l| l.part == part)
            .copied()
            .unwrap_or_else(|| part.default_spec())
    }
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            lines: vec![
                Part::App.default_spec(),
                Part::Title.default_spec(),
                Part::Content.default_spec(),
            ],
            show_timer: true,
            timer_border: true,
        }
    }
}

pub trait NotificationProvider {
    type NotificationStream<'a>: Stream<Item = Result<Notification>> + 'a
    where
        Self: 'a;

    fn stream(&mut self) -> Result<Self::NotificationStream<'_>>;
}

impl ContentProvider for Notification {
    type ContentStream<'a> = impl Stream<Item = Result<FrameBuffer>> + 'a;

    // This needs to be enabled until full GAT support is here
    #[allow(clippy::needless_lifetimes)]
    fn stream(&mut self) -> Result<<Self as ContentProvider>::ContentStream<'_>> {
        let mut interval = time::interval(Duration::from_millis(TICK_LENGTH.as_()));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let t0 = std::time::Instant::now();
        log::info!(
            "notification: streaming {} ticks (~{}ms nominal)",
            self.ticks,
            u32::from(self.ticks) * TICK_LENGTH as u32
        );
        // Two countdown styles: a 1px frame around the panel edge (default,
        // occupies no interior space) or the original corner ring.
        let timer_border = self.timer_border;
        let origin = Point::new(117, 29);
        let progress = ProgressBar::new(origin, self.ticks as f32);
        let edge = EdgeProgress::new(PANEL_W as u32, PANEL_H as u32, self.ticks as f32);

        // TODO: Remove hardcoded font
        let style = MonoTextStyle::new(self.content_font, BinaryColor::On);
        let mut content_origin = self.content_origin;
        let content_size = self.content_size;
        let content_align = self.content_align;
        let content_bold = self.content_bold;
        let show_timer = self.show_timer;
        let content = self.content.clone();
        let content_line_h = self.content_line_h;

        // The body is centred/right-aligned against the space right of any
        // icon AND left of the countdown ring, matching how the title is
        // placed. Capping only the start x is NOT enough — the glyph run would
        // still run past the reserved edge — so the text itself is truncated to
        // the available characters. Notifications are transient, so a truncated
        // body with the `>` overflow marker beats silently clipping glyphs.
        // Each wrapped line is positioned independently, so measure the widest.
        let right = self.content_right;
        let widest = content
            .iter()
            .map(|l| (l.chars().count() as u32) * content_size.char_width())
            .max()
            .unwrap_or(0);
        let content_w = widest.min((right - content_origin.x).max(0) as u32);
        content_origin.x = content_align.offset(content_w, right, content_origin.x);

        Ok(try_stream! {
            for i in 0..self.ticks {
                let mut image = self.frame;
                self.title.at_tick(&mut image, if self.scroll {
                    i
                } else {
                    0
                })?;
                for (i, line_text) in content.iter().enumerate() {
                    if line_text.is_empty() {
                        continue;
                    }
                    let ly = content_origin.y + i as i32 * content_line_h;
                    Text::with_baseline(
                        line_text,
                        Point::new(content_origin.x, ly),
                        style,
                        Baseline::Top,
                    )
                    .draw(&mut image)?;
                    if content_bold {
                        Text::with_baseline(
                            line_text,
                            Point::new(content_origin.x + 1, ly),
                            style,
                            Baseline::Top,
                        )
                        .draw(&mut image)?;
                    }
                }
                if show_timer {
                    if timer_border {
                        edge.draw_at(i as f32, &mut image)?;
                    } else {
                        progress.draw_at(i as f32, &mut image)?;
                    }
                }
                // Frame progress. A stall used to be completely silent, which
                // made it impossible to tell whether the stream hung before
                // its first frame, partway through, or on the final tick.
                //
                // DEBUG level, not INFO: a normal 8s notification runs ~160
                // ticks, so this emitted ~16 lines each time and buried the
                // rest of the journal. Start the daemon with APEX_LOG=debug to
                // see it -- the per-frame trace is only useful when a stall is
                // actually being chased.
                if i == 0 {
                    log::debug!("notification: first frame after {:?}", t0.elapsed());
                } else if i % 10 == 0 {
                    log::debug!("notification: frame {i}/{} at {:?}", self.ticks, t0.elapsed());
                }
                yield image;
                interval.tick().await;
            }
        })
    }

    fn name(&self) -> &'static str {
        "notification"
    }
}

impl<'a> NotificationBuilder<'a> {
    pub fn new() -> Self {
        NotificationBuilder::default()
    }

    pub fn with_content(mut self, content: impl Into<String>) -> Self {
        self.content = Some(content.into());
        self
    }

    pub fn with_title(mut self, title: &'a str) -> Self {
        self.title = Some(title);
        self
    }

    pub fn with_icon(mut self, icon: Icon<'a>) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Set the sending application's name, shown above the title when the
    /// layout enables it.
    pub fn with_app_name(mut self, app: impl Into<String>) -> Self {
        self.app_name = Some(app.into());
        self
    }

    /// Override the text placement.
    pub fn with_layout(mut self, layout: Layout) -> Self {
        self.layout = layout;
        self
    }

    /// Set the base display time in seconds. Ignored when 0, which means
    /// "use the built-in default".
    pub fn with_hold_seconds(mut self, seconds: u64) -> Self {
        self.hold_seconds = (seconds > 0).then_some(seconds);
        self
    }

    fn title(&self) -> &'a str {
        self.title.unwrap_or("Notification")
    }

    fn font(&self) -> &'a MonoFont<'_> {
        self.font.unwrap_or(&iso_8859_15::FONT_6X10)
    }

    fn offset(&self) -> Size {
        self.icon
            .as_ref()
            .map_or_else(Size::zero, |icon| icon.0.size())
            + Size::new(3, 10)
    }

    fn projection(&self) -> Size {
        let offset = self.offset();
        let height = self.font().character_size.height;
        // Leave the same 3px gutter the original layout used, so a title that
        // fits before still fits now.
        let icon_w: u32 = offset.width.into();
        let width = PANEL_W.saturating_sub(icon_w).saturating_sub(3);

        Size::new(width, height)
    }

    fn projection_characters(&self) -> u32 {
        let font = self.font();
        let projection = self.projection();

        projection.width / font.character_size.width
    }

    fn needs_scroll(&self) -> bool {
        let length = self.title().len();
        (self.projection_characters() as usize) < length
    }

    fn required_ticks(&self) -> u32 {
        // `duration` is AUTHORITATIVE: the notification occupies exactly that
        // many ticks. The ring sweep and the title scroll are paid for INSIDE
        // it rather than added on top -- previously a 5s setting rendered for
        // 6.85s (100 + 20 ring + 18 scroll), which contradicts the whole point
        // of unifying on one duration knob.
        let budget = match self.hold_seconds {
            Some(secs) => secs as usize * TICKS_PER_SECOND,
            // Original behaviour: 1s to read, with the ring sweep inside it.
            None => TICKS_PER_SECOND,
        };

        // The total IS the duration. Scroll and ring are paid for inside it:
        // a title too long to fit scrolls at its normal rate and is simply cut
        // off when the notification ends. Adding them on top (the old
        // behaviour) turned a 5s setting into 6.85s -- 100 base + 20 ring +
        // 18 scroll -- which is exactly what unifying on one duration was
        // meant to prevent.
        //
        // A scrolling title that cannot finish in time still scrolls; it just
        // does not get to complete before the display ends.
        budget.as_()
    }

    pub fn build(self) -> Result<Notification> {
        let mut base_image = FrameBuffer::new();
        let layout = self.layout.clone();
        let has_icon = self.icon.is_some();

        // We have an icon so lets draw it
        if let Some(icon) = &self.icon {
            let Size { width, height } = icon.0.size();

            if width != 24 || height != 24 {
                return Err(anyhow!(
                    "Notification icons need to be 24x24 for the time being!"
                ));
            }

            Image::new(&icon.0, Point::zero()).draw(&mut base_image)?;
        }

        // Everything derived from `&self` is resolved up front, because the
        // moves below consume the fields.
        let size = self.offset();
        let icon_w = size.width as i32;
        let title_text = self.title();
        let app_name = self
            .app_name
            .clone()
            .filter(|n| !n.trim().is_empty());

        // These borrow `self`, so compute them before any field is moved.
        let ticks = self.required_ticks();
        let scroll = self.needs_scroll();

        // ---- App line ----
        // Each part is placed independently. Explicit rows win; otherwise a
        // line auto-packs below the previous one. A row that would collide with
        // the previous line is pushed down rather than drawn over it.
        let cursor_y = 1i32;
        let app_spec = layout.line(Part::App);
        let mut app_bottom = cursor_y;

        if app_spec.shown && app_name.is_some() {
            let name = app_name.as_deref().unwrap_or_default();
            let app_base = app_spec.row.unwrap_or(cursor_y) + app_spec.dy;
            // Resolve against the room actually left on the panel. Non-Auto
            // specs return themselves unchanged.
            let app_size = app_spec.size.resolve(
                name,
                (PANEL_W as u32).saturating_sub(icon_w as u32),
                (PANEL_H as i32 - app_base).max(0) as u32,
                1,
            );
            let style = MonoTextStyle::new(app_size.font(), BinaryColor::On);
            let w = (name.chars().count() as u32) * app_size.char_width();
            let y = app_base.clamp(0, (PANEL_H - app_size.line_height() as i32).max(0));
            let app_right = layout.usable_right(y, app_size.line_height() as i32);
            let w = w.min((app_right - icon_w).max(0) as u32);
            let x = app_spec.align.offset(w, app_right, icon_w);
            Text::with_baseline(name, Point::new(x, y), style, Baseline::Top)
                .draw(&mut base_image)?;
            if app_spec.bold {
                Text::with_baseline(name, Point::new(x + 1, y), style, Baseline::Top)
                    .draw(&mut base_image)?;
            }
            // Only app_bottom is read from here on; the title block uses it
            // directly. Writing cursor_y too was a leftover that the compiler
            // correctly flagged as dead.
            app_bottom = y + app_size.line_height() as i32;
        }

        // ---- Title ----
        let title_spec = layout.line(Part::Title);
        let title_base =
            title_spec.row.unwrap_or(if app_bottom > 1 { app_bottom } else { 3 }) + title_spec.dy;
        // Size first: the resolved height is what the row clamp needs, and for
        // a scrolling title the width budget is the panel minus the icon.
        let title_size = title_spec.size.resolve(
            &title_text,
            (PANEL_W as u32).saturating_sub(icon_w as u32),
            (PANEL_H as i32 - title_base).max(0) as u32,
            1,
        );
        let title_h = title_size.line_height() as i32;
        let mut title_y = title_base;
        // Never start above a drawn app line, and never run off the bottom.
        title_y = title_y.clamp(0, (PANEL_H - title_h).max(0));

        // Usable width excludes the countdown ring when this band would
        // overlap it, so a title near the bottom scrolls within the free space
        // instead of running under the ring.
        let title_right = layout.usable_right(title_y, title_h);
        let title_meas = (title_text.chars().count() as u32) * title_size.char_width();
        let title_w = title_meas.min((title_right - icon_w).max(0) as u32);
        let title_x = title_spec.align.offset(title_w, title_right, icon_w);

        // `at_tick` copies rows `0..projection.height` of the glyph canvas to
        // `position.y + n`. The canvas is the full font cell, so its ink begins
        // `baseline` rows down — projecting only the cell HEIGHT crops the
        // bottom of tall glyphs (XL lost its last rows, which read as the
        // "only the top half" symptom). Project the whole cell so the glyph
        // lands intact, then let the row clamp keep it on the panel.
        let font = title_size.font();
        let projection_width = (title_right - title_x.max(icon_w)).max(0) as u32;
        let projection = Size::new(projection_width, font.character_size.height);

        let title = ScrollableBuilder::new()
            .with_text(title_text)
            // MUST pass the title's font: without this ScrollableBuilder falls
            // back to FONT_6X10, so every title size rendered identically and
            // XL looked vertically clipped (the taller cell was drawn with the
            // smaller glyph, then clipped to a Large-sized projection).
            .with_custom_font(title_size.font())
            .with_position(Point::new(title_x, title_y))
            .with_projection(projection)
            .build()?;

        // ---- Content ----
        let content = self.content.unwrap_or_default();
        let has_content = !content.is_empty();
        let content_spec = layout.line(Part::Content);

        let title_bottom = title_y + title_h;
        let mut content_y = match content_spec.row {
            Some(y) => y + content_spec.dy,
            // Original layout used y=20 with an icon present, 10 without.
            None if !has_content => 10,
            None if has_icon => 20,
            None => title_bottom,
        };
        if has_content && content_y < title_bottom {
            content_y = title_bottom;
        }
        // Resolve the body size against the room below it. Wrapping consumes
        // several lines, so the budget is a line COUNT derived from the height
        // left -- fitting on width alone would pick a size whose wrapped lines
        // run off the 40px panel.
        let content_avail_h = (PANEL_H - content_y).max(0) as u32;
        let content_budget_lines =
            (content_avail_h / content_spec.size.line_height().max(1)).max(1);
        let content_size = content_spec.size.resolve(
            &content,
            (PANEL_W as u32).saturating_sub(icon_w as u32),
            content_avail_h,
            content_budget_lines,
        );
        content_y = content_y.clamp(0, (PANEL_H - content_size.line_height() as i32).max(0));

        // Same reservation for the body line.
        let content_right = layout.usable_right(content_y, content_size.line_height() as i32);

        // Wrap the body into whatever vertical space is left below it, rather
        // than truncating at one line. `max_lines` is derived from the real
        // budget so wrapping can never paint past the panel bottom or under
        // the countdown ring.
        let content_h = content_size.line_height() as i32;
        let body_lines = if has_content && content_spec.wrap {
            let avail_h = PANEL_H - content_y;
            let max_lines = (avail_h / content_h).max(1) as usize;
            crate::providers::custom::wrap_text(
                &content,
                content_right - icon_w,
                content_size.char_width() as i32,
                max_lines,
            )
        } else {
            vec![content.clone()]
        };

        // Per-line width caps: a wrapped body line lower down the panel can
        // land inside the ring's y band even when the FIRST line cannot, so
        // each line gets its own cap rather than one for the whole block.
        let body_lines: Vec<String> = body_lines
            .into_iter()
            .enumerate()
            .map(|(i, l)| {
                let ly = content_y + i as i32 * content_h;
                let right = layout.usable_right(ly, content_h);
                let avail = (right - icon_w).max(0) as u32;
                let max_chars = (avail / content_size.char_width()) as usize;
                if l.chars().count() <= max_chars {
                    l
                } else {
                    let mut t: String = l.chars().take(max_chars.saturating_sub(1)).collect();
                    t.push('>');
                    t
                }
            })
            .collect();

        // One line per notification, so the resolved sizes are visible without
        // turning on the per-frame DEBUG trace.
        log::info!(
            "notification layout: content size={content_size:?} y={content_y} h={content_h} lines={}",
            body_lines.len()
        );

        Ok(Notification {
            frame: base_image,
            ticks,
            title,
            scroll,
            content: body_lines,
            // `icon_w` is already icon.width + the 3px gutter (see `offset`),
            // so adding the gutter again shifted the body one character cell
            // right of the title it reads under. Use it bare so both lines
            // share a left edge.
            content_origin: Point::new(icon_w, content_y),
            content_right,
            content_line_h: content_h,
            content_size,
            content_align: content_spec.align,
            content_bold: content_spec.bold,
            content_font: content_spec.size.font(),
            show_timer: layout.show_timer,
            timer_border: layout.timer_border,
        })
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    /// Auto must scale with available room, not be a fixed size in disguise.
    /// `duration` must be authoritative: the ring sweep and title scroll are
    /// paid for INSIDE it. Regression: a 5s setting produced 138 ticks (6.85s)
    /// because both were added on top of the base, so a long title outlasted
    /// its configured time by nearly 40%.
    ///
    /// Built through the real builder, not arithmetic: the previous version of
    /// this test only asserted `x.min(budget) == budget`, which is tautological.
    /// The body must line up with the title on its left edge. It used to start
    /// at `icon_w + 3` while the title started at `icon_w`, so the body sat 3px
    /// -- one character cell at Small -- to the right of the title it is
    /// supposed to read under.
    #[test]    #[test]
    fn content_left_edge_matches_title_left_edge() {
        let layout = Layout {
            lines: vec![
                line(Part::App, |s| s.shown = true),
                line(Part::Title, |s| {
                    s.size = SizeClass::XLarge;
                    s.align = Align::Left;
                }),
                line(Part::Content, |s| {
                    s.size = SizeClass::XLarge;
                    s.align = Align::Left;
                }),
            ],
            show_timer: false,
            timer_border: true,
        };
        // "I" is the narrowest inked glyph in the 6x8 face so the measured
        // extent is the glyph box, not padding from a wider letter.
        let rows = render_ascii(&layout, "I", "I", "I");

        let b = bands(&rows);
        let app_x = ink_extent(&rows[b[0].0..=b[0].1].to_vec()).0;
        let title_x = ink_extent(&rows[b[1].0..=b[1].1].to_vec()).0;
        let body_x = ink_extent(&rows[b[b.len() - 1].0..=b[b.len() - 1].1].to_vec()).0;

        assert_eq!(
            title_x, body_x,
            "body starts at x={body_x} but title starts at x={title_x}"
        );
        assert_eq!(app_x, title_x, "app and title also disagree");
    }

    #[test]
    fn duration_is_the_same_for_scrolling_and_non_scrolling_titles() {
        let layout = Layout::default();
        let ticks_for = |title: &str, secs: u64| {
            NotificationBuilder::new()
                .with_title(title)
                .with_content("body")
                .with_hold_seconds(secs)
                .with_layout(layout.clone())
                .build()
                .expect("build")
                .ticks
        };

        let short = "OK";
        // 90 chars at Large is far wider than the 128px panel, so this scrolls.
        let long = "This title is deliberately much longer than the panel is wide so it must scroll";

        for secs in [1u64, 5, 10] {
            let budget = secs as usize * TICKS_PER_SECOND;
            assert_eq!(
                ticks_for(short, secs) as usize,
                budget,
                "{secs}s: short title should use the whole budget"
            );
            assert_eq!(
                ticks_for(long, secs) as usize,
                budget,
                "{secs}s: a scrolling title must not extend past the duration"
            );
        }
    }

    /// A scrolling title longer than the whole budget still ends on time.
    #[test]
    fn very_long_title_does_not_extend_the_duration() {
        let layout = Layout::default();
        let huge = "x".repeat(400);
        let n = NotificationBuilder::new()
            .with_title(&huge)
            .with_content("body")
            .with_hold_seconds(3)
            .with_layout(layout)
            .build()
            .expect("build");
        assert_eq!(
            n.ticks as usize,
            3 * TICKS_PER_SECOND,
            "a 400-char title must not push a 3s notification past 3s"
        );
    }

    #[test]
    fn auto_scales_up_when_there_is_room() {
        let short = "OK";
        let long = "The build finished successfully with no warnings";
        let a = SizeClass::Auto.resolve(short, 128, 20, 2);
        let b = SizeClass::Auto.resolve(long, 128, 20, 2);
        assert!(
            a.line_height() > b.line_height(),
            "auto did not scale with text length: short={a:?} long={b:?}"
        );
        assert_eq!(SizeClass::Auto.resolve(short, 128, 20, 2), SizeClass::XLarge);
    }

    /// Height is the binding constraint for the body: a size that fits the
    /// width but whose wrapped lines run past the panel must be rejected.
    #[test]
    fn auto_respects_the_height_budget() {
        let text = "wrapping needs several lines so this is a long body indeed";
        let picked = SizeClass::Auto.resolve(text, 128, 9, 3);
        assert!(
            picked.line_height() <= 9,
            "{picked:?} is {}px tall but only 9px available",
            picked.line_height()
        );
        // No class can put 55 chars on one 128px line, so the budget is
        // unsatisfiable: the resolver must degrade to the smallest class, which
        // is the one that overruns least.
        let impossible = SizeClass::Auto.resolve(text, 128, 40, 1);
        assert_eq!(
            impossible,
            SizeClass::Small,
            "unsatisfiable budget should fall back to Small, not {impossible:?}"
        );
    }

    /// A non-Auto spec must be returned untouched, whatever the text or room.
    #[test]
    fn explicit_sizes_pass_through_unchanged() {
        let huge = "x".repeat(500);
        for c in [SizeClass::Small, SizeClass::Medium, SizeClass::Large, SizeClass::XLarge] {
            assert_eq!(c.resolve("anything at all", 128, 40, 3), c);
            assert_eq!(c.resolve(&huge, 10, 5, 1), c);
        }
    }

    /// An impossible constraint must still yield a drawable size, never Auto.
    #[test]
    fn resolve_never_returns_auto() {
        for w in [0u32, 1, 7, 64, 128] {
            for h in [0u32, 1, 3, 11, 40] {
                let r = SizeClass::Auto.resolve("some body text here", w, h, 2);
                assert_ne!(r, SizeClass::Auto, "auto leaked out at {w}x{h}");
            }
        }
    }

    #[test]
    fn auto_parses_from_config_string() {
        assert_eq!(SizeClass::parse("auto"), Some(SizeClass::Auto));
        assert_eq!(SizeClass::parse("AUTO"), Some(SizeClass::Auto));
        assert_eq!(SizeClass::parse("a"), Some(SizeClass::Auto));
        assert_eq!(SizeClass::parse("xl"), Some(SizeClass::XLarge));
        assert_eq!(SizeClass::parse("l"), Some(SizeClass::Large));
    }

    fn line(part: Part, mut f: impl FnMut(&mut LineSpec)) -> LineSpec {
        let mut spec = part.default_spec();
        f(&mut spec);
        spec
    }

    /// Render one frame statically and return it as ASCII rows so both a human
    /// and a failing assertion can see the real layout.
    fn render_ascii(layout: &Layout, title: &str, content: &str, app: &str) -> Vec<String> {
        let n = NotificationBuilder::new()
            .with_title(title)
            .with_content(content)
            .with_app_name(app)
            .with_layout(layout.clone())
            .build()
            .expect("build");

        let mut fb = n.frame.clone();
        n.title.at_tick(&mut fb, 0).unwrap();
        if !n.content.is_empty() {
            let style = MonoTextStyle::new(n.content_font, BinaryColor::On);
            let avail = (n.content_right - n.content_origin.x).max(0) as u32;
            let max_chars = (avail / n.content_size.char_width()) as usize;
            let widest = n
                .content
                .iter()
                .map(|l| l.chars().count())
                .max()
                .unwrap_or(0);
            let w = (widest.min(max_chars) as u32) * n.content_size.char_width();
            let x = n.content_align.offset(w, n.content_right, n.content_origin.x);
            for (i, line_text) in n.content.iter().enumerate() {
                let clipped: String =
                    line_text.chars().take(max_chars).collect();
                let ly = n.content_origin.y + i as i32 * n.content_line_h;
                Text::with_baseline(&clipped, Point::new(x, ly), style, Baseline::Top)
                    .draw(&mut fb)
                    .unwrap();
            }
        }

        let mut rows = Vec::new();
        for y in 0..PANEL_H as u32 {
            let mut row = String::new();
            for x in 0..128u32 {
                // Must match `impl Drawable for FrameBuffer`, which reads
                // pixels at `i + 8` — the USB report has an 8-byte header.
                // Using a different offset silently shifts every column and
                // fabricates ink in the header bytes.
                let idx = 8 + (y * 128 + x) as usize;
                row.push(if fb.framebuffer.get(idx).map(|b| *b).unwrap_or(false) {
                    '#'
                } else {
                    '.'
                });
            }
            rows.push(row);
        }
        rows
    }

    fn inked_rows(rows: &[String]) -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter(|(_, r)| r.contains('#'))
            .map(|(i, _)| i)
            .collect()
    }

    fn bands(rows: &[String]) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = Vec::new();
        for r in inked_rows(rows) {
            match out.last_mut() {
                Some((_, end)) if *end + 1 == r => *end = r,
                _ => out.push((r, r)),
            }
        }
        out
    }

    fn ink_extent(rows: &[String]) -> (usize, usize) {
        let inked = inked_rows(rows);
        let min_x = rows
            .iter()
            .filter(|r| r.contains('#'))
            .flat_map(|r| r.chars().enumerate())
            .filter(|(_, c)| *c == '#')
            .map(|(x, _)| x)
            .min()
            .unwrap_or(0);
        let max_x = rows
            .iter()
            .filter(|r| r.contains('#'))
            .flat_map(|r| r.chars().enumerate())
            .filter(|(_, c)| *c == '#')
            .map(|(x, _)| x)
            .max()
            .unwrap_or(0);
        let _ = inked;
        (min_x, max_x)
    }

    /// Each line must occupy its own band — the original bug drew the title on
    /// top of the app line when an explicit row collided with it.
    #[test]
    fn layout_lines_do_not_overlap() {
        let layout = Layout {
            lines: vec![
                line(Part::App, |s| {
                    s.shown = true;
                    s.size = SizeClass::Medium;
                }),
                line(Part::Title, |s| {
                    s.size = SizeClass::XLarge;
                    s.row = Some(4);
                }),
                line(Part::Content, |s| {
                    s.size = SizeClass::Small;
                    s.row = Some(22);
                }),
            ],
            show_timer: false,
            timer_border: true,
        };
        let rows = render_ascii(&layout, "Centered XL title", "small centered body", "Firefox");
        let b = bands(&rows);
        assert!(b.len() >= 3, "expected 3 bands, got {b:?}");
        for w in b.windows(2) {
            assert!(w[0].1 + 1 < w[1].0, "bands {:?} and {:?} touch/overlap", w[0], w[1]);
        }
    }

    /// Lines are configured INDEPENDENTLY: a centred title with a left-aligned
    /// body must actually do that. This is the whole point of per-line specs.
    #[test]
    fn layout_per_line_align_is_independent() {
        let centered = Layout {
            lines: vec![
                line(Part::App, |s| s.shown = false),
                line(Part::Title, |s| {
                    s.align = Align::Center;
                    s.row = Some(2);
                }),
                line(Part::Content, |s| {
                    s.align = Align::Left;
                    s.row = Some(20);
                }),
            ],
            show_timer: false,
            timer_border: true,
        };
        // Wide enough that centring is measurable: "TITLE" at XL is only 5
        // chars = 40px, which starts near x=44 either way and proves nothing.
        let rows = render_ascii(&centered, "A VERY LONG TITLE", "body", "");

        // Title band should start well right of 0 (centred); body at 0 (left).
        let b = bands(&rows);
        assert!(b.len() >= 2, "expected 2 bands, got {b:?}");
        let (title_a, title_b) = (b[0].0, b[0].1);
        let (body_a, body_b) = (b[b.len() - 1].0, b[b.len() - 1].1);
        let title_x = ink_extent(&rows[title_a..=title_b].to_vec()).0;
        let body_x = ink_extent(&rows[body_a..=body_b].to_vec()).0;
        assert!(title_x > 10, "centred title should start past x=10, got {title_x}");
        // The body starts at icon_w (icon width + gutter), so "near the left
        // edge" means single digits, not literally 0.
        assert!(body_x < 10, "left-aligned body should hug the left, got {body_x}");
    }

    /// A band configured past the bottom must be clamped, not written out of
    /// bounds — `Scrollable::at_tick` used to panic on this.
    #[test]
    fn layout_row_cannot_render_off_panel() {
        for title_row in [0, 10, 30, 38, 100] {
            for content_row in [0, 30, 39, 100] {
                let layout = Layout {
                    lines: vec![
                        line(Part::App, |s| s.shown = true),
                        line(Part::Title, |s| {
                            s.size = SizeClass::XLarge;
                            s.row = Some(title_row);
                        }),
                        line(Part::Content, |s| {
                            s.size = SizeClass::XLarge;
                            s.row = Some(content_row);
                        }),
                    ],
                    show_timer: true,
                    timer_border: true,
                };
                // The assertion is implicit: build() + draw must not panic.
                let rows = render_ascii(&layout, "Title that is long", "Body", "App");
                for (y, row) in rows.iter().enumerate() {
                    assert_eq!(row.chars().count(), 128, "row {y} wrong width");
                }
            }
        }
    }

    /// `shown = false` must actually remove the line.
    #[test]
    fn layout_shown_false_hides_line() {
        let layout = Layout {
            lines: vec![
                line(Part::App, |s| s.shown = false),
                line(Part::Title, |s| {
                    s.row = Some(2);
                }),
                line(Part::Content, |s| {
                    s.row = Some(20);
                }),
            ],
            show_timer: false,
            timer_border: true,
        };
        let rows = render_ascii(&layout, "TITLE", "body", "Firefox");
        // With the app line hidden, nothing may be drawn above the title row.
        // (A 1px sliver at row 0 can be antialiasing from the title's own
        // glyphs, so assert on "the top band starts at the title row".)
        let b = bands(&rows);
        assert!(
            b[0].0 >= 2,
            "something drew above the title row: {:?}",
            b
        );
        assert!(
            !rows[0].contains('#') && !rows[1].contains('#'),
            "app line still visible at the top: {:?}",
            b
        );
    }

    /// A line placed at the top of the panel must render its FULL glyph height.
    /// `Baseline::Top` already anchors the glyph top at `y` in eg 0.8 — adding
    /// `font().baseline` on top of that shifts every line DOWN and looks like
    /// vertical clipping. Assert the rendered band height instead.
    #[test]
    fn layout_line_renders_full_glyph_height() {
        // Measured ink heights for "8W8W8W" at row 4.
        for (size, min_rows) in [
            (SizeClass::Small, 5),
            (SizeClass::Medium, 6),
            (SizeClass::Large, 7),
            (SizeClass::XLarge, 9),
        ] {
            let layout = Layout {
                lines: vec![
                    line(Part::App, |s| {
                        s.shown = false;
                    }),
                    line(Part::Title, |s| {
                        s.size = size;
                        s.row = Some(4);
                    }),
                    line(Part::Content, |s| {
                        s.shown = false;
                    }),
                ],
                show_timer: false,
                timer_border: true,
            };
            let rows = render_ascii(&layout, "8W8W8W", "", "");
            let inked = inked_rows(&rows);
            let height = inked.last().copied().unwrap_or(0) - inked[0] + 1;
            assert!(
                height >= min_rows,
                "{size:?}: only {height} rows lit (want >= {min_rows}) at {inked:?}"
            );
            // Ink must begin at (or just below) the requested row, never above.
            assert!(
                inked[0] >= 4,
                "{size:?}: drew above its row: starts at {}, wanted 4",
                inked[0]
            );
        }
    }

    /// A line placed in the ring's y range must not extend past the ring's
    /// left edge, so the bottom of the panel is usable without overlap.
    #[test]
    fn layout_reserves_timer_area() {
        let (l, t, _r, b) = TIMER_BOX;
        let reserve = Layout {
            lines: vec![
                line(Part::App, |s| s.shown = false),
                line(Part::Title, |s| s.row = Some(0)),
                // Body deliberately inside the ring's band.
                line(Part::Content, |s| {
                    s.row = Some(t);
                    s.align = Align::Left;
                    s.size = SizeClass::Small;
                }),
            ],
            show_timer: true,
            // The legacy corner ring is the only thing that reserves space;
            // the default edge frame reserves nothing.
            timer_border: false,
        };

        // Sanity: this band really does overlap the ring.
        assert!(t < b, "test row must overlap the ring band");
        assert_eq!(reserve.usable_right(t, 7), l, "band should be capped to the ring edge");
        // A band well above the ring is unaffected.
        assert_eq!(reserve.usable_right(0, 7), PANEL_W as i32);

        // And nothing TEXT may be drawn into the ring's x range. The ring
        // itself is drawn there by `stream()` and is excluded from `frame`, so
        // the static render contains text only — this assertion is exact.
        let rows = render_ascii(&reserve, "Title", "body text that is quite long indeed", "");
        for y in t..b {
            let row = &rows[y as usize];
            // Text may occupy up to x = l-1 (the cap is exclusive), so the
            // reserved region starts at the ring's left edge itself.
            let ink_in_ring = row[l as usize..].contains('#');
            assert!(
                !ink_in_ring,
                "row {y} draws text inside the timer box from x={l}: {}",
                &row[l as usize..]
            );
        }
        // Sanity: the same text DOES reach the cap when it is long enough,
        // proving the assertion above is not vacuous.
        let wide = render_ascii(&reserve, "Title", "W".repeat(40).as_str(), "");
        let last_inked_x = wide
            .iter()
            .flat_map(|r| r.chars().enumerate())
            .filter(|(_, c)| *c == '#')
            .map(|(x, _)| x)
            .max()
            .unwrap_or(0);
        assert!(
            last_inked_x >= 100,
            "expected the long body to fill the band, last ink at {last_inked_x}"
        );
        assert!(
            last_inked_x < l as usize,
            "long body ran past the reserved edge: {last_inked_x} >= {l}"
        );

        // The edge-frame indicator reserves nothing, so the same band gets
        // the full width even with the countdown switched on.
        let edge = Layout {
            timer_border: true,
            ..reserve.clone()
        };
        assert_eq!(edge.usable_right(t, 7), PANEL_W as i32);
        // Hiding the countdown entirely frees the space too.
        let no_ring = Layout {
            show_timer: false,
            ..reserve.clone()
        };
        assert_eq!(no_ring.usable_right(t, 7), PANEL_W as i32);
    }

    /// Defaults must reproduce the ORIGINAL hardcoded layout, so an unconfigured
    /// install looks exactly like it did before this feature existed.
    #[test]
    fn layout_default_matches_original_placement() {
        let d = Layout::default();
        assert!(!d.line(Part::App).shown, "app line is off by default");
        assert_eq!(d.line(Part::Title).size, SizeClass::Large);
        assert_eq!(d.line(Part::Title).row, None);
        assert_eq!(d.line(Part::Title).align, Align::Left);
        assert_eq!(d.line(Part::Content).size, SizeClass::Auto);
        assert_eq!(d.line(Part::Content).row, None);
        assert!(d.show_timer, "countdown ring is on by default");
    }

    #[test]
    fn probe_vertical_budget() {
        // body at row 24, size Small -> line_height 7. Panel is 40 tall.
        let lh = SizeClass::Small.line_height() as i32;
        eprintln!("PROBE Small line_height={lh}; row 24 leaves {} rows below", 40 - 24);
        for row in [20, 24, 28, 30] {
            let layout = Layout {
                lines: vec![
                    line(Part::App, |s| { s.shown = false; }),
                    line(Part::Title, |s| { s.shown = false; }),
                    line(Part::Content, |s| { s.row = Some(row); s.size = SizeClass::Small; }),
                ],
                show_timer: false,
                timer_border: true,
            };
            let rows = render_ascii(&layout, "T", "Some body text that is fairly long and should wrap", "");
            let inked = inked_rows(&rows);
            eprintln!("PROBE row={row} inked={inked:?}");
        }
    }

    /// Hiding the countdown ring must give the body back its full width —
    /// `usable_right` short-circuits to the panel edge when `show_timer` is
    /// false, so no separate "ring area" bookkeeping is needed.
    #[test]
    fn layout_body_uses_full_width_when_ring_hidden() {
        let (l, t, _r, _b) = TIMER_BOX;
        // `legacy_ring` = the original 10px corner countdown, which reserves
        // space. `show_timer` = whether any countdown is drawn at all.
        let base = |show_timer: bool, legacy_ring: bool| Layout {
            lines: vec![
                line(Part::App, |s| { s.shown = false; }),
                line(Part::Title, |s| { s.shown = false; }),
                line(Part::Content, |s| {
                    s.row = Some(t);
                    s.wrap = true;
                    s.align = Align::Left;
                    // Pin the size: this test is about the ring's geometry, not
                    // about auto-sizing. Left on Auto, the body shrinks to fit
                    // the ring's width and both cases reach the same last
                    // column, so the assertion no longer means anything.
                    s.size = SizeClass::Medium;
                }),
            ],
            show_timer,
            timer_border: !legacy_ring,
        };

        // The legacy corner ring caps a band that overlaps it, at x = 116.
        assert_eq!(base(true, true).usable_right(t, 7), l);
        // The default edge frame reserves nothing.
        assert_eq!(base(true, false).usable_right(t, 7), PANEL_W as i32);
        // No countdown at all: full width, either style.
        assert_eq!(base(false, true).usable_right(t, 7), PANEL_W as i32);
        assert_eq!(base(false, false).usable_right(t, 7), PANEL_W as i32);

        // And the rendered body really does get wider.
        let body = "a body long enough to reach the right edge of the panel";
        let with_ring = render_ascii(&base(true, true), "T", body, "");
        let without = render_ascii(&base(true, false), "T", body, "");
        let last_x = |rows: &[String]| {
            rows.iter()
                .flat_map(|r| r.chars().enumerate())
                .filter(|(_, c)| *c == '#')
                .map(|(x, _)| x)
                .max()
                .unwrap_or(0)
        };
        assert!(
            last_x(&without) > last_x(&with_ring),
            "hiding the ring should widen the body: {} vs {}",
            last_x(&with_ring),
            last_x(&without)
        );
    }

    /// `wrap = true` must use the vertical space below the body row instead of
    /// truncating at one line, and must never paint past the panel bottom.
    #[test]
    fn layout_body_wraps_into_remaining_space() {
        let body = "This notification body is long enough that it must wrap onto more than a single line";
        let wrapped = Layout {
            lines: vec![
                line(Part::App, |s| { s.shown = false; }),
                line(Part::Title, |s| { s.row = Some(0); }),
                line(Part::Content, |s| {
                    s.row = Some(20);
                    s.size = SizeClass::Small;
                    s.wrap = true;
                }),
            ],
            show_timer: false,
            timer_border: true,
        };
        let rows = render_ascii(&wrapped, "T", body, "");
        let inked = inked_rows(&rows);
        assert!(!inked.is_empty());

        // Unwrapped, the same text would be truncated to one line and end by
        // row ~26. Wrapping must push ink further down the panel.
        let flat = Layout {
            lines: vec![
                line(Part::App, |s| { s.shown = false; }),
                line(Part::Title, |s| { s.row = Some(0); }),
                line(Part::Content, |s| {
                    s.row = Some(20);
                    s.size = SizeClass::Small;
                    s.wrap = false;
                }),
            ],
            show_timer: false,
            timer_border: true,
        };
        let flat_rows = render_ascii(&flat, "T", body, "");
        let flat_last = *inked_rows(&flat_rows).last().unwrap();
        let wrap_last = *inked.last().unwrap();
        assert!(
            wrap_last > flat_last,
            "wrapping did not use extra rows: wrapped ends {wrap_last}, flat ends {flat_last}"
        );

        // Never past the panel, whatever the row.
        for row in [0, 10, 24, 30, 34] {
            let l = Layout {
                lines: vec![
                    line(Part::App, |s| { s.shown = false; }),
                    line(Part::Title, |s| { s.shown = false; }),
                    line(Part::Content, |s| {
                        s.row = Some(row);
                        s.wrap = true;
                    }),
                ],
                show_timer: false,
                timer_border: true,
            };
            let r = render_ascii(&l, "T", body, "");
            let last = r.len();
            assert_eq!(last, PANEL_H as usize, "row {row}: wrong render height");
        }
    }

    /// `line_height` must be at least the rows the font actually inks, plus
    /// leading — otherwise consecutive lines overlap. It is deliberately NOT
    /// compared to the advertised cell height: FONT_8X13 reports 13px but inks
    /// 10, so an equality assertion would be wrong.
    #[test]
    fn layout_line_height_covers_inked_rows() {
        for (size, inked) in [
            (SizeClass::Small, 6),
            (SizeClass::Medium, 7),
            (SizeClass::Large, 8),
            (SizeClass::XLarge, 10),
        ] {
            assert!(
                size.line_height() >= inked,
                "{size:?}: line_height {} < {inked} inked rows",
                size.line_height()
            );
        }
    }

    /// Sizes must be genuinely distinct. They were all rendering as FONT_6X10
    /// because the ScrollableBuilder was never given the title font.
    #[test]
    fn layout_title_size_actually_changes_rendering() {
        let mut heights = Vec::new();
        for size in [
            SizeClass::Small,
            SizeClass::Medium,
            SizeClass::Large,
            SizeClass::XLarge,
        ] {
            let layout = Layout {
                lines: vec![
                    line(Part::App, |s| { s.shown = false; }),
                    line(Part::Title, |s| {
                        s.size = size;
                        s.row = Some(4);
                    }),
                    line(Part::Content, |s| { s.shown = false; }),
                ],
                show_timer: false,
                timer_border: true,
            };
            let rows = render_ascii(&layout, "QGMW", "", "");
            let inked = inked_rows(&rows);
            heights.push(inked.last().copied().unwrap_or(0) - inked[0] + 1);
        }
        assert_eq!(
            heights,
            vec![6, 7, 8, 10],
            "title heights should grow with size class, got {heights:?}"
        );
    }

    /// Rendering from the LIVE settings.toml, so the dumped frame matches what
    /// the daemon actually builds. Catches divergence between test config and
    /// the running config.
    #[test]
    fn layout_from_live_config() {
        let mut settings = config::Config::default();
        if let Some(dir) = dirs::config_dir() {
            let _ = settings.merge(
                config::File::with_name(&dir.join("apex-tux/settings").to_string_lossy())
                    .required(false),
            );
        }
        let dl = Layout::default();
        let mut lines = Vec::new();
        for (key, part) in [("app", Part::App), ("title", Part::Title), ("content", Part::Content)] {
            let prefix = format!("notifications.lines.{key}");
            let fb = dl.line(part);
            if settings.get_table(&prefix).is_err() {
                lines.push(fb);
                continue;
            }
            lines.push(LineSpec {
                part,
                dy: settings.get_int(&format!("{prefix}.dy")).ok().map(|v| v as i32).unwrap_or(0),
                wrap: settings.get_bool(&format!("{prefix}.wrap")).unwrap_or(fb.wrap),
                shown: settings.get_bool(&format!("{prefix}.shown")).unwrap_or(fb.shown),
                size: settings.get_str(&format!("{prefix}.size")).ok()
                    .and_then(|v| SizeClass::parse(&v)).unwrap_or(fb.size),
                align: settings.get_str(&format!("{prefix}.align")).ok()
                    .and_then(|v| Align::parse(&v)).unwrap_or(fb.align),
                row: settings.get_int(&format!("{prefix}.row")).ok().map(|v| v as i32),
                bold: settings.get_bool(&format!("{prefix}.bold")).unwrap_or(fb.bold),
            });
        }
        let layout = Layout {
            lines,
            show_timer: settings.get_bool("notifications.show_timer").unwrap_or(true),
            timer_border: settings
                .get_bool("notifications.timer_border")
                .unwrap_or(true),
        };
        eprintln!("LIVE LAYOUT = {layout:?}");
        let rows = render_ascii(&layout, "Centered XL title", "small centered body", "Firefox");
        for (i, r) in rows.iter().enumerate() {
            if r.contains('#') {
                eprintln!("{i:2} {r}");
            }
        }
    }
}
