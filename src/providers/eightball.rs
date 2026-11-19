
use anyhow::Result;
use async_stream::try_stream;
use futures::Stream;
use log::info;
use std::{sync::LazyLock, time::Duration};

use crate::render::{
    notifications::{
        PANEL_H,
        BADGE_EDGE_INSET,
        Align, Badge, BadgeFrame, BadgePos, LineSpec, Notification, NotificationBuilder,
        NotificationProvider, Part, SizeClass,
    },
    scheduler::{NotificationWrapper, NOTIFICATION_PROVIDERS},
};
use config::Config;
use linkme::distributed_slice;

/// The classic 20 Magic 8-Ball answers, generated locally.
///
/// No network: a reading is one uniform random pick from this table, so the
/// overlay works offline and answers in nanoseconds instead of a cloud
/// round-trip. The set is the standard one — 10 affirmative, 5 non-committal,
/// 5 negative — all ASCII, so every glyph has a panel font entry.
const ANSWERS: [&str; 20] = [
    // Affirmative
    "It is certain",
    "It is decidedly so",
    "Without a doubt",
    "Yes definitely",
    "You may rely on it",
    "As I see it, yes",
    "Most likely",
    "Outlook good",
    "Yes",
    "Signs point to yes",
    // Non-committal
    "Reply hazy, try again",
    "Ask again later",
    "Better not tell you now",
    "Cannot predict now",
    "Concentrate and ask again",
    // Negative
    "Don't count on it",
    "My reply is no",
    "My sources say no",
    "Outlook not so good",
    "Very doubtful",
];

#[distributed_slice(NOTIFICATION_PROVIDERS)]
static PROVIDER_INIT: fn() -> Result<Box<dyn NotificationWrapper>> = register_callback;

#[allow(clippy::unnecessary_wraps)]
fn register_callback() -> Result<Box<dyn NotificationWrapper>> {
    info!("Registering 8ball on-demand source.");
    // The distributed-slice constructor takes no arguments, so the settings
    // are read here. Same pattern as the general desktop notification source.
    let mut settings = Config::default();
    if let Some(dir) = dirs::config_dir() {
        let _ = settings.merge(
            config::File::with_name(&dir.join("ss-oled/settings").to_string_lossy())
                .required(false),
        );
    }
    Ok(Box::new(EightBall::from_settings(&settings)))
}

/// Asking for one reading. Sent on the hotkey channel, drained by the stream.
static REQUESTED: LazyLock<std::sync::atomic::AtomicBool> =
    LazyLock::new(|| std::sync::atomic::AtomicBool::new(false));

/// Called from the hotkey handler. Returns false if a request is already in
/// flight, so holding the key down cannot queue a pile of duplicate requests.
pub fn request() -> bool {
    REQUESTED
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
}

/// Clear the in-flight flag. Called after the reading has been shown.
pub fn complete() {
    REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
}


/// A 14x14 8-ball: a ring, a gloss highlight, and a window whose contents
/// travel around the inside to read as a spin.
///
/// Hand-drawn as ASCII rather than a font glyph: the smallest bundled face is
/// 4x6 and cannot draw a circle, and this art exists nowhere else, so keeping
/// it inline in the source is the only place it is ever edited. Circle and
/// window are generated rather than hand-typed so the curve stays round.
const BALL_W: u32 = 26;
const BALL_H: u32 = 26;

/// Fixed points of the gloss highlight, in the generated art's coordinates.
const GLOSS: [(u32, u32); 2] = [(4, 3), (5, 3)];

/// Build one frame, optionally with the number window centred at (wx, wy).
///
/// The window is the SPIN indicator: a 3x3 plate clipped to the inner disc that
/// travels around the inside during the animation. It is only drawn when a
/// window position is supplied, because the settled frame shows the '8' and a
/// window behind it just reads as a box behind the digit.
fn ball_frame(window: Option<(f64, f64)>) -> BadgeFrame {
    let cx = BALL_W as f64 / 2.0;
    let cy = BALL_H as f64 / 2.0;
    // Outer/inner radii of the drawn ring.
    let r_out = BALL_W as f64 / 2.0 - 0.1;
    let r_in = r_out - 1.1;
    let mut f = BadgeFrame::blank(BALL_W, BALL_H);
    for y in 0..BALL_H {
        for x in 0..BALL_W {
            let d = (((x as f64 + 0.5) - cx).powi(2) + ((y as f64 + 0.5) - cy).powi(2)).sqrt();
            let mut ink = d >= r_in && d <= r_out;
            if !ink && d < r_in {
                ink = match window {
                    // Window plate, when this frame is spinning.
                    Some((wx, wy)) => {
                        (x as f64 + 0.5 - wx).abs() <= 1.5
                            && (y as f64 + 0.5 - wy).abs() <= 1.5
                    }
                    // Settled: gloss highlight only, so the '8' sits on a clean
                    // ball instead of on a lit plate.
                    None => GLOSS.contains(&(x, y)),
                };
            }
            f.set(x, y, ink);
        }
    }
    f
}

/// A 13x13 '8' with 1px strokes, centred on the settled frame.
///
/// Hand-drawn rather than taken from a font: the bundled terminal faces have a
/// 4x6 '8' with no centre gap, which at 26x26 reads as a smudge.
///
/// 1px strokes, deliberately NOT bold. A 3px-stroke version was tried first
/// and looked worse: at this size the strokes plus the tight counters filled the
/// inner disc in and it read as a blob rather than an '8'. What makes an '8'
/// legible is the two OPEN counters, so the strokes stay thin and the waist is
/// a single doubled row (rows 6-7) where the two rings meet.
const EIGHT: &[&str] = &[
    ".............",
    "....#####....",
    "...#.....#...",
    "...#.....#...",
    "...#.....#...",
    "...#.....#...",
    "....#####....",
    "....#####....",
    "...#.....#...",
    "...#.....#...",
    "...#.....#...",
    "...#.....#...",
    "....#####....",
];

/// Draw the '8' into the middle of `f`, clipped to the ball's inner disc.
fn stamp_eight(f: &mut BadgeFrame, cx: f64, cy: f64) {
    let ew = EIGHT[0].chars().count() as i32;
    let eh = EIGHT.len() as i32;
    // Centre the glyph on (cx, cy) in whole pixels.
    let x0 = cx as i32 - ew / 2;
    let y0 = cy as i32 - eh / 2;
    let ball_cx = BALL_W as f64 / 2.0;
    let ball_cy = BALL_H as f64 / 2.0;
    let r_in = BALL_W as f64 / 2.0 - 1.2;
    for (gy, row) in EIGHT.iter().enumerate() {
        for (gx, c) in row.chars().enumerate() {
            if matches!(c, '.' | ' ') {
                continue;
            }
            let px = x0 + gx as i32;
            let py = y0 + gy as i32;
            let d = (((px as f64 + 0.5) - ball_cx).powi(2) + ((py as f64 + 0.5) - ball_cy).powi(2))
                .sqrt();
            // Only ink inside the ball, so the glyph never crosses the ring.
            if d < r_in {
                f.set(px as u32, py as u32, true);
            }
        }
    }
}

/// Six frames, the window stepping around the inside of the ball.
fn spin_frames() -> Vec<BadgeFrame> {
    let cx = BALL_W as f64 / 2.0;
    let cy = BALL_H as f64 / 2.0;
    let r_win = 2.6;
    (0..6)
        .map(|i| {
            let a = 2.0 * std::f64::consts::PI * (i as f64) / 6.0;
            ball_frame(Some((cx + r_win * a.cos(), cy + r_win * a.sin())))
        })
        .collect()
}

/// The settled ball shown once the reading is up: the '8' at its centre.
fn still_frame() -> BadgeFrame {
    let cx = BALL_W as f64 / 2.0;
    let cy = BALL_H as f64 / 2.0;
    let mut f = ball_frame(None);
    stamp_eight(&mut f, cx, cy);
    f
}

/// Corner the ball sits in, and the interior box it reserves.
///
/// Bottom-LEFT, inside the timer border: the reading then sits beside the ball
/// rather than above it, so a short answer gets the full height of the panel.
/// Inset by `BADGE_EDGE_INSET` because the timer border is drawn on the
/// outermost pixel ring and the two would otherwise collide.
const BALL_POS: BadgePos = BadgePos::LowerLeft;

/// (left, top, right, bottom) the ball's footprint occupies.
///
/// MUST match `Badge::draw_at`'s origin arithmetic, inset included. These two
/// disagreed by 1px on both axes when the badge was 20x20: the reserve claimed
/// a 1px gap the draw never honoured, so the ball landed on the timer's pixel
/// ring while the text was laid out as if it had not. A test in
/// `render::notifications` now pins the draw side of this contract.
fn ball_box() -> (i32, i32, i32, i32) {
    let left = BADGE_EDGE_INSET;
    let top = ball_top();
    (left, top, left + BALL_W as i32, top + BALL_H as i32)
}

/// Row the ball's top edge sits on: vertically centred in the panel.
///
/// The ball is lower than centre only when the panel height is not an exact
/// multiple of its own, and then by at most 1px, which is not visible. The
/// previous 4px lift was a hand-tuned offset that put the ball 2px low; this is
/// derived so it stays put when the artwork size changes.
fn ball_top() -> i32 {
    (PANEL_H as i32 - BALL_H as i32) / 2
}

/// How far the badge draw sits above the `LowerLeft` corner it is anchored to.
fn ball_lift() -> i32 {
    PANEL_H as i32 - BALL_H as i32 - BADGE_EDGE_INSET - ball_top()
}

/// The spin badge, used while the request is in flight.
fn spin_badge() -> Badge {
    Badge::new(spin_frames(), BALL_POS).with_dy(-ball_lift())
}

/// The resting badge for a finished reading.
fn still_badge() -> Badge {
    Badge::new(vec![still_frame()], BALL_POS).with_dy(-ball_lift())
}

pub struct EightBall {
    duration_seconds: u64,
    layout: crate::render::notifications::Layout,
}

impl EightBall {
    /// Read `[eightball]`. Layout mirrors the notification line specs so the
    /// reading prints like any other overlay, and the timer border defaults ON:
    /// an on-demand answer has no provider rotating behind it to show how long
    /// it will stay.
    fn from_settings(settings: &Config) -> Self {
        let default_layout = crate::render::notifications::Layout::default();
        // Reading defaults, chosen for this specific overlay:
        //
        //  - the app name is suppressed: "8ball" on its own line would push the
        //    reading down into the ball, and the ball already says what this is.
        //  - the reading is Auto so `resolve` picks the largest class whose
        //    wrapped lines fit both the text column and the height budget, and
        //    it wraps onto further lines rather than truncating.
        //  - row 2 puts the text in the top band, clear of the 20x20 ball which
        //    occupies rows 19..39.
        const DEFAULT_READING_ROW: i32 = 0;
        let mut lines = Vec::new();
        for (key, part) in [
            ("app", Part::App),
            ("title", Part::Title),
            ("content", Part::Content),
        ] {
            let prefix = format!("eightball.lines.{key}");
            let fallback = default_layout.line(part);
            if settings.get_table(&prefix).is_err() {
                let mut spec = fallback;
                match key {
                    // The ball already says what this is, and an app line would
                    // steal a row from the reading.
                    // The title carries nothing for this overlay, so it is
                    // gone rather than merely hidden: it is not wired to the
                    // API response at all.
                    "app" | "title" => spec.shown = false,
                    // Auto + wrap: with the ball on the left the text column is
                    // 98px, holding 12-16 chars per line at XL/L. An answer
                    // longer than one line wraps at full size instead of
                    // dropping a size class or scrolling sideways.
                    "content" => {
                        spec.size = SizeClass::Auto;
                        spec.align = Align::Left;
                        spec.row = Some(DEFAULT_READING_ROW);
                        spec.wrap = true;
                    }
                    _ => {}
                }
                lines.push(spec);
                continue;
            }
            lines.push(LineSpec {
                part,
                shown: settings
                    .get_bool(&format!("{prefix}.shown"))
                    .unwrap_or(fallback.shown),
                size: settings
                    .get_str(&format!("{prefix}.size"))
                    .ok()
                    .and_then(|v| SizeClass::parse(&v))
                    .unwrap_or(fallback.size),
                align: settings
                    .get_str(&format!("{prefix}.align"))
                    .ok()
                    .and_then(|v| Align::parse(&v))
                    .unwrap_or(fallback.align),
                row: settings.get_int(&format!("{prefix}.row")).ok().map(|v| v as i32),
                bold: settings
                    .get_bool(&format!("{prefix}.bold"))
                    .unwrap_or(fallback.bold),
                wrap: settings
                    .get_bool(&format!("{prefix}.wrap"))
                    .unwrap_or(fallback.wrap),
                dy: settings.get_int(&format!("{prefix}.dy")).ok().map(|v| v as i32).unwrap_or(0),
            });
        }
        Self {
            // Clamped so a typo cannot wedge the display for an hour.
            duration_seconds: settings
                .get_int("eightball.duration")
                .unwrap_or(8)
                .clamp(1, 300) as u64,
            layout: crate::render::notifications::Layout {
                lines,
                show_timer: settings.get_bool("eightball.show_timer").unwrap_or(true),
                timer_border: settings
                    .get_bool("eightball.timer_border")
                    .unwrap_or(true),
                // Reserve the ball's corner so a long reading cannot run its
                // last line underneath the artwork.
                badge_box: Some(ball_box()),
            },
        }
    }
}

/// Pick one reading locally: a uniform draw from the classic answer table.
/// Nanoseconds and zero I/O, so it runs directly on the reactor — no
/// `spawn_blocking` hop the way the synchronous cloud fetch needed.
fn local_reading() -> &'static str {
    use rand::Rng;
    ANSWERS[rand::rng().random_range(0..ANSWERS.len())]
}

impl NotificationProvider for EightBall {
    type NotificationStream<'a>
        = impl Stream<Item = Result<Notification>> + 'a
    where
        Self: 'a;

    /// Idle until a request arrives, then a brief spin and the reading.
    ///
    /// The spin is a SEPARATE notification from the reading, not a badge
    /// update on one long-lived notification: `Notification` is immutable
    /// once built, and a second notification is the only way to change the
    /// text without tearing down the stream mid-await. Each is short, so the
    /// scheduler's overlay rules apply unchanged.
    fn stream(&mut self) -> Result<Self::NotificationStream<'_>> {
        let duration_seconds = self.duration_seconds;
        let layout = self.layout.clone();
        Ok(try_stream! {
            loop {
                // Park here with no pending frames. Polling the flag on a timer
                // rather than blocking on a channel keeps this a plain stream
                // with no extra plumbing, and the poll interval is short enough
                // that a hotkey press feels immediate.
                while !REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }

                // --- spinner phase ---
                // A short held notification whose only content is the badge.
                // The reading is generated locally and is ready immediately,
                // so this is pure showmanship: one second of spin before the
                // reveal, not a fetch timeout window.
                let spin = NotificationBuilder::new()
                    .with_title("")
                    .with_app_name("8ball")
                    .with_layout(layout.clone())
                    .with_badge(spin_badge())
                    .with_hold_seconds(1)
                    .build()?;
                // Yield the notification itself, not its frames: the
                // scheduler owns the per-frame loop and the overlay rules. The
                // spinner is a real notification, so it interrupts rotation
                // exactly like any other.
                yield spin;

                // --- reading, generated locally ---
                // One uniform pick from the classic answer table: no network,
                // no blocking task, no failure path.
                let reading = local_reading();
                complete();
                info!("8ball reading: {reading}");

                // The reading goes in the BODY, not the title. The title is a
                // single horizontally-scrolling line by design, so a long answer
                // would drift sideways; the body wraps onto further lines at a
                // fixed size, which is what is wanted here.
                let notif = NotificationBuilder::new()
                    .with_title("")
                    .with_content(reading)
                    .with_app_name("8ball")
                    .with_layout(layout.clone())
                    .with_badge(still_badge())
                    .with_hold_seconds(duration_seconds)
                    .build()?;
                yield notif;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn answer_set_is_the_classic_twenty() {
        assert_eq!(ANSWERS.len(), 20);
        let mut sorted = ANSWERS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 20, "duplicate answer");
        for a in ANSWERS {
            assert!(!a.is_empty());
            assert!(a.is_ascii(), "non-ASCII glyph would render blank: {a}");
        }
    }

    #[test]
    fn every_pick_is_a_member() {
        for _ in 0..1_000 {
            assert!(ANSWERS.contains(&local_reading()));
        }
    }

    #[test]
    fn distribution_covers_all_answers() {
        let mut counts = [0u32; 20];
        for _ in 0..20_000 {
            let r = local_reading();
            counts[ANSWERS.iter().position(|a| *a == r).unwrap()] += 1;
        }
        // Expected 1000 per answer; a uniform draw lands well inside this band.
        for (a, c) in ANSWERS.iter().zip(counts) {
            assert!((600..=1_400).contains(&c), "{a}: {c} draws");
        }
    }

    #[test]
    fn generation_cost_is_nanoseconds() {
        let n = 100_000u32;
        let t = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(local_reading());
        }
        let ns_per = t.elapsed().as_nanos() as f64 / f64::from(n);
        println!("local_reading: {ns_per:.0} ns/op over {n} draws");
        assert!(ns_per < 10_000.0, "{ns_per} ns/op is not local-cheap");
    }

    #[tokio::test]
    async fn stream_yields_spin_then_reading() {
        let mut eb = EightBall::from_settings(&Config::default());
        let stream = eb.stream().unwrap();
        futures::pin_mut!(stream);
        assert!(request(), "flag should start clear");
        let t = std::time::Instant::now();
        let spin = stream.next().await.unwrap().unwrap();
        let reading = stream.next().await.unwrap().unwrap();
        let elapsed = t.elapsed();
        complete();
        // The reading holds longer than the spin, so its timeout budget is
        // strictly larger; both carry the +3s scheduling slack.
        assert!(spin.expected_duration() < reading.expected_duration());
        // No network in the path: the pair lands within a few poll ticks.
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
        println!("request -> spin -> reading in {elapsed:?}");
    }
}
