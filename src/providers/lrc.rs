//! LRC lyric parsing and timestamp lookup.
//!
//! An LRC line looks like `[mm:ss.xx] some words` (or `[mm:ss:xx]`). A single
//! line may carry several timestamps for a repeated chorus, e.g.
//! `[00:12.00][01:30.00] chorus line` — each becomes its own entry.
//!
//! Times are stored in microseconds to match MPRIS `Position`, which is also
//! microseconds (see the `ms here == microseconds` note in lyrics-on-panel).

/// One timestamped lyric line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LyricLine {
    /// Offset from track start, in microseconds.
    pub time_us: i64,
    /// Text to display. Empty string for instrumental gaps.
    pub lyric: String,
}

/// Parse LRC text into timestamped lines, sorted by time.
///
/// Malformed lines are skipped rather than failing the whole parse — real-world
/// `.lrc` files from the wild routinely contain metadata tags
/// (`[ar: Artist]`, `[ti: Title]`) mixed in with lyric lines.
pub fn parse_lrc(text: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();

    for raw in text.lines() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }

        // Collect every leading `[...]` group. The remainder is the lyric.
        let mut stamps: Vec<i64> = Vec::new();
        let mut rest = raw;

        while rest.starts_with('[') {
            let Some(close) = rest.find(']') else {
                // Unterminated '[' — not a timestamp, treat the line as text.
                break;
            };
            let inner = &rest[1..close];
            match parse_timestamp(inner) {
                Some(us) => stamps.push(us),
                // A non-timestamp tag ([ar: ...]) ends the stamp run.
                None => break,
            }
            rest = &rest[close + 1..];
        }

        if stamps.is_empty() {
            continue; // metadata or malformed line
        }

        let lyric = rest.trim();
        for time_us in stamps {
            lines.push(LyricLine {
                time_us,
                lyric: lyric.to_string(),
            });
        }
    }

    // Multi-timestamp lines push entries out of order; sort by time.
    lines.sort_by_key(|l| l.time_us);
    lines
}

/// Parse the inside of a `[...]` group as a timestamp. Returns microseconds.
///
/// Accepts `[mm:ss.xx]`, `[mm:ss:xx]`, and bare `[mm:ss]`. Returns `None` for
/// metadata tags so the caller can stop scanning for timestamps.
fn parse_timestamp(inner: &str) -> Option<i64> {
    // Metadata tags contain ':' in a non-numeric position, e.g. "ar: Artist".
    let parts: Vec<&str> = inner.split(&[':', ';'][..]).collect();
    if parts.len() < 2 {
        return None;
    }

    let minutes: f64 = parts[0].trim().parse().ok()?;
    if !minutes.is_finite() {
        return None;
    }

    // Seconds may carry the fraction inline ("ss.xx") or as a third group
    // ("ss:xx" / "ss;xx"). Both forms are in real-world .lrc files.
    let seconds_raw: f64 = parts[1].trim().parse().ok()?;
    if !seconds_raw.is_finite() {
        return None;
    }

    let whole = seconds_raw.trunc();
    let fraction = match parts.get(2) {
        Some(f) => {
            // Third-group form is centiseconds by convention.
            let v: f64 = f.trim().parse().ok()?;
            if !v.is_finite() {
                return None;
            }
            v / 100.0
        }
        None => seconds_raw - whole,
    };

    let total_seconds = (minutes * 60.0) + whole + fraction;
    Some((total_seconds * 1_000_000.0).round() as i64)
}

/// Find the lyric active at `position_us`.
///
/// Walks back from the last line whose timestamp is at or before the position
/// to the nearest line with non-empty text, so instrumental gaps (empty lines)
/// hold the previous lyric instead of blanking the panel.
pub fn current_lyric(lines: &[LyricLine], position_us: i64) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    let idx = lines.partition_point(|l| l.time_us <= position_us);
    lines[..idx]
        .iter()
        .rev()
        .find(|l| !l.lyric.is_empty())
        .map(|l| l.lyric.clone())
}

/// The next line with text after `position_us`, if any.
pub fn next_lyric(lines: &[LyricLine], position_us: i64) -> Option<String> {
    let idx = lines.partition_point(|l| l.time_us <= position_us);
    lines[idx..]
        .iter()
        .find(|l| !l.lyric.is_empty())
        .map(|l| l.lyric.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_timestamps() {
        let lrc = "[00:12.50] hello world\n[01:05.00] second line";
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].time_us, 12_500_000);
        assert_eq!(lines[0].lyric, "hello world");
        assert_eq!(lines[1].time_us, 65_000_000);
    }

    #[test]
    fn parses_colon_fraction_separator() {
        let lines = parse_lrc("[00:12:50] colon style");
        assert_eq!(lines[0].time_us, 12_500_000);
    }

    #[test]
    fn multi_timestamp_line_expands() {
        let lines = parse_lrc("[00:10.00][01:20.00] chorus");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].time_us, 10_000_000);
        assert_eq!(lines[1].time_us, 80_000_000);
        assert!(lines.iter().all(|l| l.lyric == "chorus"));
    }

    #[test]
    fn metadata_tags_are_skipped() {
        let lrc = "[ar: Some Artist]\n[ti: Some Title]\n[00:05.00] real lyric";
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].lyric, "real lyric");
    }

    #[test]
    fn out_of_order_timestamps_are_sorted() {
        let lines = parse_lrc("[00:30.00] later\n[00:10.00] earlier");
        assert_eq!(lines[0].lyric, "earlier");
        assert_eq!(lines[1].lyric, "later");
    }

    #[test]
    fn current_lyric_walks_back_over_instrumental_gap() {
        let lines = parse_lrc("[00:05.00] singing\n[00:10.00]\n[00:20.00] later line");
        // Inside the empty gap at 15s: should hold "singing", not blank.
        assert_eq!(current_lyric(&lines, 15_000_000).as_deref(), Some("singing"));
        assert_eq!(
            current_lyric(&lines, 25_000_000).as_deref(),
            Some("later line")
        );
    }

    #[test]
    fn current_lyric_none_before_first_timestamp() {
        let lines = parse_lrc("[00:10.00] first");
        assert_eq!(current_lyric(&lines, 1_000_000), None);
    }

    #[test]
    fn next_lyric_skips_empty_entries() {
        let lines = parse_lrc("[00:05.00] a\n[00:10.00]\n[00:20.00] b");
        assert_eq!(next_lyric(&lines, 1_000_000).as_deref(), Some("a"));
        assert_eq!(next_lyric(&lines, 6_000_000).as_deref(), Some("b"));
    }

    /// Real payload from lrclib.net for "Yesterday" (The Beatles, Help!, 125s).
    /// Guards against format drift in the upstream API.
    #[test]
    fn parses_real_lrclib_payload() {
        let lrc = "[00:05.06] Yesterday, all my troubles seemed so far away\n[00:13.53] Now it looks as though they're here to stay\n[00:17.24] Oh, I believe in yesterday\n[00:22.64] Suddenly, I'm not half the man I used to be\n[00:30.02] There's a shadow hanging over me\n[00:34.37] Oh, yesterday came suddenly\n[00:39.88] Why she had to go\n[00:44.20] I don't know, she wouldn't say\n[00:49.54] I said something wrong\n[00:54.10] Now I long for yesterday\n[00:59.87] Yesterday, love was such an easy game to play\n[01:07.87] Now I need a place to hide away\n[01:11.77] Oh, I believe in yesterday\n[01:17.47] Why she had to go\n[01:21.73] I don't know, she wouldn't say\n[01:27.20] I said something wrong\n[01:31.53] Now I long for yesterday\n[01:37.40] Yesterday, love was such an easy game to play\n[01:45.42] Now I need a place to hide away\n[01:49.23] Oh, I believe in yesterday\n[01:53.30] ";
        let lines = parse_lrc(lrc);
        assert!(lines.len() > 10, "expected many lines, got {}", lines.len());
        assert_eq!(lines[0].lyric, "Yesterday, all my troubles seemed so far away");
        // Timestamps ascend and are in microseconds.
        assert!(lines.windows(2).all(|w| w[0].time_us <= w[1].time_us));
        assert_eq!(lines[0].time_us, 5_060_000);
        // Lookup at a known timestamp resolves the right line.
        let hit = current_lyric(&lines, 14_000_000).unwrap();
        assert_eq!(hit, "Now it looks as though they're here to stay");
    }

    #[test]
    fn empty_and_malformed_input_is_safe() {
        assert!(parse_lrc("").is_empty());
        assert!(parse_lrc("no timestamps at all").is_empty());
        assert!(parse_lrc("[broken").is_empty());
        assert_eq!(current_lyric(&[], 0), None);
    }
}
