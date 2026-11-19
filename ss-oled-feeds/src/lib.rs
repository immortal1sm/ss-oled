//! Body decoding for the `custom` provider — and for the GUI's live preview.
//!
//! The custom provider is a small engine: a URL, a list of `<path>: <label>`
//! field specs, and optionally an array path. All of it works on a
//! `serde_json::Value`. So XML support is not a second engine — it is one
//! converter that maps an XML document onto that same value tree, after which
//! `items`, `fields`, `article_fields`, size classes, scrolling and both GUI
//! editors behave exactly as they do for JSON.
//!
//! Mapping rules (documented for users in docs/PROVIDERS.md):
//!
//! | XML                                     | value tree                        |
//! |-----------------------------------------|-----------------------------------|
//! | `<title>Hello</title>`                  | `{"title": "Hello"}`              |
//! | repeated siblings `<item>…</item>`      | `{"item": [ … ]}`                 |
//! | `<link href="…"/>`                      | `{"link": {"@href": "…"}}`        |
//! | `<description>a <b>b</b></description>` | `{"description": {"b": "b", "#text": "a"}}` |
//! | `dc:creator`                            | `{"dc_creator": …, "creator": …}` |
//!
//! A namespace prefix becomes an underscore, and the local name is also
//! inserted as an alias when that name is unambiguous among its siblings.
//! Prefixes can never survive as a `:` in a key: the field-spec syntax
//! separates path from label with `:`, so a colon inside a path would swallow
//! the label. Aliasing is suppressed when two differently-prefixed elements
//! share a local name — `archlinux.org/feeds/news/` carries both `<link>` and
//! `<atom:link>` in `<channel>`, and collapsing them would turn a plain text
//! `channel.link` into an array.
//!
//! The GUI links against this crate too, rather than carrying a second copy of
//! the resolver and converter: a duplicated type or algorithm across that
//! binary boundary is a source of truth that drifts (see the `SizeCls` /
//! `SizeClass` episode and the three copies of `wrap_text`).

use anyhow::{anyhow, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

/// How a provider's response body should be read.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Format {
    /// Sniff the body: a leading `<` means XML, anything else JSON.
    #[default]
    Auto,
    Json,
    Xml,
}

impl Format {
    /// Parse a config value. Unknown words fall back to `Auto` rather than
    /// failing the provider: a typo should not take a panel off the rotation.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "json" => Format::Json,
            "xml" | "rss" | "atom" | "feed" => Format::Xml,
            _ => Format::Auto,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Format::Auto => "auto",
            Format::Json => "json",
            Format::Xml => "xml",
        }
    }

    pub fn is_xml(&self) -> bool {
        matches!(self, Format::Xml)
    }
}

/// Whether HTML stripping should default to on for a format.
///
/// An RSS `<description>` is usually a fragment of HTML (`<p>`, `<a href>`),
/// which on a 128x40 1-bit panel renders as tag soup. JSON APIs essentially
/// never carry markup, so there the default stays off — switching it on for
/// everybody would rewrite the output of existing providers.
pub fn default_strip_html(format: Format) -> bool {
    format.is_xml()
}

/// Sniff the body's format. A leading BOM and whitespace are skipped.
pub fn sniff(body: &str) -> Format {
    let body = body.trim_start_matches('\u{feff}').trim_start();
    if body.starts_with('<') {
        Format::Xml
    } else {
        Format::Json
    }
}

/// Resolve `Auto` against the body, so a caller can log what it actually did.
pub fn resolve(body: &str, requested: Format) -> Format {
    match requested {
        Format::Auto => sniff(body),
        other => other,
    }
}

/// Decode a response body into the value tree the provider engine consumes.
pub fn decode(body: &str, requested: Format) -> Result<Value> {
    decode_with(body, requested).map(|(v, _)| v)
}

/// As [`decode`], but also reports the format actually used.
pub fn decode_with(body: &str, requested: Format) -> Result<(Value, Format)> {
    let format = resolve(body, requested);
    let value = match format {
        Format::Json => {
            serde_json::from_str(body).map_err(|e| anyhow!("JSON parse failed: {e}"))?
        }
        Format::Xml => xml_to_value(body)?,
        Format::Auto => unreachable!("resolve() never returns Auto"),
    };
    Ok((value, format))
}

/// Simple path resolver shared by the daemon and the GUI: `a.b[0].c`.
///
/// Both callers used to carry their own copy; a change to one silently left the
/// GUI preview reading paths the daemon could not.
pub fn get_path<'a>(json: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = json;
    for seg in path.split('.') {
        if seg.is_empty() {
            continue;
        }
        if let Some((key, idx)) = seg.split_once('[') {
            // `[0]` (bare index) or `key[0]` (key then index).
            if !key.is_empty() {
                cur = cur.get(key.trim_end_matches('['))?;
            }
            let idx: usize = idx.trim_end_matches(']').parse().ok()?;
            cur = cur.get(idx)?;
        } else {
            cur = cur.get(seg)?;
        }
    }
    Some(cur)
}

/// Render a resolved value as panel text.
///
/// An object is not shown as JSON: XML gives every element with attributes an
/// object shape, and `<guid isPermaLink="false">tag:…</guid>` or Atom's
/// `<link href="…"/>` would otherwise dump a brace-and-quote blob onto the
/// panel. The element's own text wins, then `@href`.
pub fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Object(map) => {
            if let Some(Value::String(s)) = map.get("#text") {
                return s.clone();
            }
            if let Some(Value::String(s)) = map.get("@href") {
                return s.clone();
            }
            value.to_string()
        }
        other => other.to_string(),
    }
}

/// Recursively strip HTML from every string in the tree.
pub fn strip_html_in_place(value: &mut Value) {
    match value {
        Value::String(s) => {
            let cleaned = strip_html(s);
            if cleaned != *s {
                *s = cleaned;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_html_in_place),
        Value::Object(map) => map.values_mut().for_each(strip_html_in_place),
        _ => {}
    }
}

/// Depth cap. Real feeds nest a handful of levels; a runaway or hostile
/// document should fail loudly instead of eating the stack.
const MAX_DEPTH: usize = 128;
/// Cap on repeated siblings kept per parent. Matches the provider's own
/// `max_items` clamp of 200, so nothing reachable on the panel is dropped.
const MAX_REPEATS: usize = 200;

/// A name exactly as the document gives it, kept in the three forms the
/// mapping needs: what a path may say, and whether a prefix was present.
#[derive(Clone)]
struct Name {
    /// Name as it appears in a path: `dc:creator` -> `dc_creator`.
    canonical: String,
    /// Name without the namespace prefix: `dc:creator` -> `creator`.
    local: String,
    prefixed: bool,
}

impl Name {
    fn parse(raw: &[u8]) -> Self {
        let raw = String::from_utf8_lossy(raw);
        let raw = raw.trim();
        match raw.rsplit_once(':') {
            Some((prefix, local)) => Self {
                canonical: format!("{}_{}", prefix.replace(':', "_"), local),
                local: local.to_string(),
                prefixed: true,
            },
            None => Self {
                canonical: raw.to_string(),
                local: raw.to_string(),
                prefixed: false,
            },
        }
    }
}

/// A named value inside an element.
struct Entry {
    name: Name,
    value: Value,
}

/// Insert entries under their canonical names, plus a local-name alias for
/// prefixed entries whose local name is unambiguous among these siblings.
fn insert_entries(obj: &mut Map<String, Value>, entries: Vec<Entry>) {
    let mut owners: HashMap<String, HashSet<String>> = HashMap::new();
    for entry in &entries {
        owners
            .entry(entry.name.local.clone())
            .or_default()
            .insert(entry.name.canonical.clone());
    }
    for entry in &entries {
        let unambiguous = owners
            .get(&entry.name.local)
            .map(|set| set.len() == 1)
            .unwrap_or(false);
        if entry.name.prefixed && unambiguous && entry.name.local != entry.name.canonical {
            obj.entry(entry.name.local.clone())
                .or_insert_with(|| entry.value.clone());
        }
    }
    for entry in entries {
        obj.insert(entry.name.canonical, entry.value);
    }
}

/// One element under construction.
struct Builder {
    name: Name,
    attrs: Vec<Entry>,
    children: Vec<Entry>,
    text: String,
    capped: bool,
}

impl Builder {
    fn new(name: Name, attrs: Vec<Entry>) -> Self {
        Self {
            name,
            attrs,
            children: Vec::new(),
            text: String::new(),
            capped: false,
        }
    }

    fn push_text(&mut self, s: &str) {
        self.text.push_str(s);
    }

    /// Repeated siblings with the same name collapse into an array — that is
    /// what makes an RSS `<item>` list renderable through `items = "…"`.
    fn push_child(&mut self, entry: Entry) {
        if let Some(slot) = self
            .children
            .iter_mut()
            .rev()
            .find(|existing| existing.name.canonical == entry.name.canonical)
        {
            match &mut slot.value {
                Value::Array(items) => {
                    if items.len() >= MAX_REPEATS {
                        self.capped = true;
                    } else {
                        items.push(entry.value);
                    }
                }
                previous => {
                    let first = std::mem::replace(previous, Value::Null);
                    *previous = Value::Array(vec![first, entry.value]);
                }
            }
            return;
        }
        self.children.push(entry);
    }

    fn finish(self) -> Value {
        if self.capped {
            log::warn!(
                "custom: <{}> repeated more than {MAX_REPEATS} times; extra entries dropped",
                self.name.canonical
            );
        }
        let text = collapse_ws(&self.text);
        let mut obj = Map::new();
        insert_entries(&mut obj, self.attrs);
        insert_entries(&mut obj, self.children);
        // An element with no children and no attributes IS its text, which is
        // what makes `<title>Hello</title>` resolve as a plain string.
        if obj.is_empty() {
            return Value::String(text);
        }
        if !text.is_empty() {
            obj.insert("#text".to_string(), Value::String(text));
        }
        Value::Object(obj)
    }
}

/// Convert an XML document (RSS, Atom, or any generic XML) into a value tree.
///
/// Tolerant by design, because feeds in the wild are: a dangling `&` is kept,
/// a truncated body still yields whatever arrived before the failure,
/// `<!DOCTYPE>`, comments and processing instructions are ignored, and unknown
/// entity references are preserved verbatim rather than failing the fetch.
pub fn xml_to_value(xml: &str) -> Result<Value> {
    let mut reader = Reader::from_str(xml);
    {
        let cfg = reader.config_mut();
        cfg.allow_dangling_amp = true;
        cfg.check_comments = false;
    }

    let mut stack: Vec<Builder> = Vec::new();
    let mut roots: Vec<Entry> = Vec::new();
    let mut events: usize = 0;
    let mut failure: Option<String> = None;

    loop {
        match reader.read_event() {
            Err(e) => {
                // Nothing usable at all -> a real failure the user must see.
                if roots.is_empty() && stack.is_empty() {
                    return Err(anyhow!("XML parse failed: {e}"));
                }
                log::warn!("custom: XML truncated after {events} events ({e}); using what parsed");
                failure = Some(e.to_string());
                break;
            }
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                if stack.len() >= MAX_DEPTH {
                    return Err(anyhow!("XML nested deeper than {MAX_DEPTH} levels"));
                }
                events += 1;
                let name = Name::parse(e.name().as_ref());
                let attrs = collect_attrs(&e);
                stack.push(Builder::new(name, attrs));
            }
            Ok(Event::Empty(e)) => {
                events += 1;
                let name = Name::parse(e.name().as_ref());
                let attrs = collect_attrs(&e);
                let value = Builder::new(name.clone(), attrs).finish();
                attach(&mut stack, &mut roots, Entry { name, value });
            }
            Ok(Event::End(_)) => {
                if let Some(builder) = stack.pop() {
                    let name = builder.name.clone();
                    let value = builder.finish();
                    attach(&mut stack, &mut roots, Entry { name, value });
                }
            }
            Ok(Event::Text(t)) => {
                if let Ok(s) = t.decode() {
                    if let Some(top) = stack.last_mut() {
                        let resolved = unescape_loose(&s);
                        top.push_text(&resolved);
                    }
                }
            }
            // CDATA is literal by definition: no entity resolution inside it.
            Ok(Event::CData(c)) => {
                if let Ok(s) = c.decode() {
                    if let Some(top) = stack.last_mut() {
                        top.push_text(&s);
                    }
                }
            }
            // The reader splits `&amp;` / `&#8217;` out of text as its own
            // event, so references are resolved here rather than guessed at
            // from the text chunks.
            Ok(Event::GeneralRef(r)) => {
                if let Ok(s) = r.decode() {
                    if let Some(top) = stack.last_mut() {
                        let resolved = resolve_ref(&s);
                        top.push_text(&resolved);
                    }
                }
            }
            // Decl / Comment / PI / DocType carry nothing we render.
            Ok(_) => {}
        }
    }

    // Elements left open by a truncated document are closed innermost-first.
    while let Some(builder) = stack.pop() {
        let name = builder.name.clone();
        let value = builder.finish();
        attach(&mut stack, &mut roots, Entry { name, value });
    }

    // The document's own top level keeps its names: a `<rss>` root becomes a
    // `rss` key, so a path reads `rss.channel.item[0].title` — the same shape
    // a JSON API's envelope has.
    let mut top = Map::new();
    insert_entries(&mut top, roots);
    if top.is_empty() {
        // Nothing parsed at all. An empty result is a valid outcome, so
        // silence here would render NO DATA with nothing in the journal to
        // explain it — say which of the two failures happened instead.
        return Err(match failure {
            Some(failure) => anyhow!("XML parse failed: {failure}"),
            None => anyhow!("XML parse produced no elements"),
        });
    }
    Ok(Value::Object(top))
}

fn attach(stack: &mut Vec<Builder>, roots: &mut Vec<Entry>, entry: Entry) {
    match stack.last_mut() {
        Some(parent) => parent.push_child(entry),
        None => roots.push(entry),
    }
}

fn collect_attrs(element: &BytesStart<'_>) -> Vec<Entry> {
    let mut entries = Vec::new();
    for attr in element.attributes().with_checks(false).flatten() {
        let key = attr.key.as_ref();
        let mut name = Name::parse(key);
        // Attributes are reachable as `@name`, which keeps them from colliding
        // with a child element of the same name.
        name.canonical.insert(0, '@');
        name.local.insert(0, '@');
        // Namespace declarations are plumbing, not data.
        if name.canonical == "@xmlns" || name.canonical.starts_with("@xmlns_") {
            continue;
        }
        let value = unescape_loose(&String::from_utf8_lossy(&attr.value));
        entries.push(Entry {
            name,
            value: Value::String(value),
        });
    }
    entries
}

/// Named character references, beyond the five XML predefines.
const HTML_ENTITIES: &[(&str, &str)] = &[
    ("nbsp", " "),
    ("mdash", "—"),
    ("ndash", "–"),
    ("hellip", "…"),
    ("lsquo", "‘"),
    ("rsquo", "’"),
    ("ldquo", "“"),
    ("rdquo", "”"),
    ("bull", "•"),
    ("middot", "·"),
    ("laquo", "«"),
    ("raquo", "»"),
    ("copy", "©"),
    ("reg", "®"),
    ("trade", "™"),
    ("deg", "°"),
    ("times", "×"),
    ("divide", "÷"),
    ("euro", "€"),
    ("pound", "£"),
    ("yen", "¥"),
    ("agrave", "à"),
    ("eacute", "é"),
    ("ntilde", "ñ"),
    ("uuml", "ü"),
];

/// Resolve one reference name (`amp`, `#39`, `#x2019`) to text. An unknown
/// name is returned as the literal `&name;` so no content is ever lost.
fn resolve_ref(name: &str) -> String {
    if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
        if let Ok(cp) = u32::from_str_radix(hex, 16) {
            if let Some(c) = char::from_u32(cp) {
                if !c.is_control() {
                    return c.to_string();
                }
            }
        }
    } else if let Some(dec) = name.strip_prefix('#') {
        if let Ok(cp) = dec.parse::<u32>() {
            if let Some(c) = char::from_u32(cp) {
                if !c.is_control() {
                    return c.to_string();
                }
            }
        }
    } else {
        let predefined = match name {
            "amp" => Some("&"),
            "lt" => Some("<"),
            "gt" => Some(">"),
            "quot" => Some("\""),
            "apos" => Some("'"),
            _ => None,
        };
        if let Some(s) = predefined.or_else(|| {
            HTML_ENTITIES
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| *v)
        }) {
            return s.to_string();
        }
    }
    format!("&{name};")
}

/// Replace every resolvable entity reference, leaving the rest untouched.
/// Used for attribute values and for text the reader handed over whole.
pub fn unescape_loose(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find('&') {
        out.push_str(&rest[..idx]);
        let after = &rest[idx + 1..];
        // A reference name is short; a `;` further away is unrelated text.
        match after.find(';').filter(|end| *end <= 32) {
            Some(end) => {
                out.push_str(&resolve_ref(&after[..end]));
                rest = &after[end + 1..];
            }
            None => {
                // Dangling `&` (common in real feeds) — keep it literally.
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Collapse every run of whitespace to a single space and trim the ends.
///
/// Pretty-printed feeds indent their elements, so `<title>\n\tFoo\n</title>`
/// would otherwise render with the indentation baked in.
pub fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    out
}

/// Tags whose boundaries separate words once their markup is removed.
const BLOCK_TAGS: &[&str] = &[
    "br",
    "p",
    "div",
    "li",
    "ul",
    "ol",
    "tr",
    "td",
    "th",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "blockquote",
    "section",
    "article",
    "figure",
    "figcaption",
    "table",
];

/// Remove HTML markup from a fragment, then resolve leftover entities.
///
/// Order matters: tags are stripped FIRST, so a double-escaped feed (where
/// `&amp;lt;p&amp;gt;` decodes to the literal text `&lt;p&gt;`) shows that
/// markup as text instead of having it re-created as tags and re-stripped.
pub fn strip_html(s: &str) -> String {
    if !s.contains('<') {
        return collapse_ws(&unescape_loose(s));
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '<' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // <script> / <style> contents are code, not text: drop the whole block.
        if let Some(end) = block_skip_end(&chars, i) {
            out.push(' ');
            i = end;
            continue;
        }
        let mut j = i + 1;
        let mut closing = false;
        if j < chars.len() && chars[j] == '/' {
            closing = true;
            j += 1;
        }
        let mut name = String::new();
        while j < chars.len() && !chars[j].is_whitespace() && chars[j] != '>' && chars[j] != '/' {
            name.push(chars[j].to_ascii_lowercase());
            j += 1;
        }
        let mut quote: Option<char> = None;
        while j < chars.len() {
            let c = chars[j];
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) => {}
                None if c == '"' || c == '\'' => quote = Some(c),
                None if c == '>' => break,
                None => {}
            }
            j += 1;
        }
        if closing || BLOCK_TAGS.contains(&name.as_str()) {
            out.push(' ');
        }
        i = (j + 1).min(chars.len());
    }
    collapse_ws(&unescape_loose(&out))
}

/// If a `<script…>` / `<style…>` block starts at `start`, return the index just
/// past its closing tag.
fn block_skip_end(chars: &[char], start: usize) -> Option<usize> {
    let head: String = chars[start + 1..(start + 10).min(chars.len())]
        .iter()
        .collect::<String>()
        .to_ascii_lowercase();
    let tag = if head.starts_with("script") {
        "script"
    } else if head.starts_with("style") {
        "style"
    } else {
        return None;
    };
    let closer = format!("</{tag}");
    let text: String = chars[start..]
        .iter()
        .collect::<String>()
        .to_ascii_lowercase();
    match text.find(&closer) {
        Some(pos) => match text[pos..].find('>') {
            Some(end) => Some(start + pos + end + 1),
            None => Some(chars.len()),
        },
        None => Some(chars.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &Value, path: &str) -> String {
        match get_path(v, path) {
            Some(found) => value_to_text(found),
            None => "<missing>".into(),
        }
    }

    const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:content="http://purl.org/rss/1.0/modules/content/" xmlns:dc="http://purl.org/dc/elements/1.1/">
  <channel>
    <title>Example &amp; Co</title>
    <link>https://example.com/</link>
    <atom:link href="https://example.com/feed" rel="self"/>
    <item>
      <title>First post</title>
      <link>https://example.com/1</link>
      <pubDate>Mon, 01 Jan 2026 10:00:00 GMT</pubDate>
      <dc:creator>Allan</dc:creator>
      <description><![CDATA[<p>Hello &amp; welcome</p>]]></description>
      <content:encoded><![CDATA[<p>Full body</p>]]></content:encoded>
    </item>
    <item>
      <title>Second post</title>
      <link>https://example.com/2</link>
      <description>A &lt;b&gt;bold&lt;/b&gt; claim &#8212; really</description>
    </item>
  </channel>
</rss>"#;

    #[test]
    fn rss_items_become_a_list() {
        let v = xml_to_value(RSS).expect("parse");
        assert_eq!(s(&v, "rss.channel.title"), "Example & Co");
        let items = get_path(&v, "rss.channel.item")
            .and_then(Value::as_array)
            .expect("array");
        assert_eq!(items.len(), 2);
        assert_eq!(s(&v, "rss.channel.item[0].title"), "First post");
        assert_eq!(s(&v, "rss.channel.item[1].link"), "https://example.com/2");
    }

    #[test]
    fn prefixed_elements_get_both_names_unless_ambiguous() {
        let v = xml_to_value(RSS).expect("parse");
        // dc:creator is reachable under both names.
        assert_eq!(s(&v, "rss.channel.item[0].dc_creator"), "Allan");
        assert_eq!(s(&v, "rss.channel.item[0].creator"), "Allan");
        assert_eq!(s(&v, "rss.channel.item[0].content_encoded"), "<p>Full body</p>");
        assert_eq!(
            s(&v, "rss.channel.item[0].encoded"),
            "<p>Full body</p>",
            "an unambiguous prefixed element is aliased to its local name"
        );
    }

    #[test]
    fn a_prefixed_sibling_does_not_hijack_the_plain_name() {
        // The real case: archlinux.org/feeds/news/ has <link> and <atom:link>
        // in the same <channel>. Local-name collapsing turned channel.link
        // into an array of the text and the element.
        let v = xml_to_value(RSS).expect("parse");
        assert_eq!(s(&v, "rss.channel.link"), "https://example.com/");
        assert_eq!(s(&v, "rss.channel.atom_link.@href"), "https://example.com/feed");
        assert_eq!(s(&v, "rss.channel.atom_link.@rel"), "self");
    }

    #[test]
    fn cdata_is_text_and_entities_in_it_stay_raw() {
        let v = xml_to_value(RSS).expect("parse");
        let d = s(&v, "rss.channel.item[0].description");
        assert_eq!(d, "<p>Hello &amp; welcome</p>");
        assert_eq!(strip_html(&d), "Hello & welcome");
    }

    #[test]
    fn escaped_markup_in_text_is_resolved_then_stripped() {
        let v = xml_to_value(RSS).expect("parse");
        assert_eq!(
            strip_html(&s(&v, "rss.channel.item[1].description")),
            "A bold claim — really"
        );
    }

    #[test]
    fn attributes_read_as_at_keys_and_objects_render_their_text() {
        let atom = r#"<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <title type="html">Hi</title>
    <link rel="alternate" href="https://example.com/a"/>
    <updated>2026-01-01T10:00:00Z</updated>
  </entry>
  <entry>
    <title>Second</title>
    <link rel="self" href="https://example.com/self"/>
    <link rel="alternate" href="https://example.com/alt"/>
    <updated>2026-01-02T10:00:00Z</updated>
  </entry>
</feed>"#;
        let v = xml_to_value(atom).expect("parse");
        // One <link> in an entry is an object, not a one-element array.
        assert_eq!(s(&v, "feed.entry[0].link.@href"), "https://example.com/a");
        assert_eq!(s(&v, "feed.entry[0].link.@rel"), "alternate");
        // <title type="html">Hi</title> is an object; the panel shows the text.
        assert_eq!(s(&v, "feed.entry[0].title"), "Hi");
        assert_eq!(s(&v, "feed.entry[0].title.@type"), "html");
        // A href-only object renders its href, not a JSON blob.
        assert_eq!(s(&v, "feed.entry[0].link"), "https://example.com/a");
        // Two <link>s DO become an array, and each keeps its own attributes.
        assert_eq!(s(&v, "feed.entry[1].link[0].@rel"), "self");
        assert_eq!(s(&v, "feed.entry[1].link[1].@href"), "https://example.com/alt");
        assert_eq!(s(&v, "feed.entry[0].missing"), "<missing>");
    }

    #[test]
    fn a_single_entry_still_produces_a_plain_lookup() {
        let one = r#"<rss><channel><item><title>Only</title></item></channel></rss>"#;
        let v = xml_to_value(one).expect("parse");
        assert_eq!(s(&v, "rss.channel.item.title"), "Only");
    }

    #[test]
    fn numeric_and_unknown_references() {
        assert_eq!(resolve_ref("#8212"), "—");
        assert_eq!(resolve_ref("#x2019"), "’");
        assert_eq!(resolve_ref("amp"), "&");
        assert_eq!(resolve_ref("nbsp"), " ");
        assert_eq!(resolve_ref("weird"), "&weird;");
        assert_eq!(
            unescape_loose("a &amp; b &none; c & d"),
            "a & b &none; c & d"
        );
    }

    #[test]
    fn whitespace_is_collapsed() {
        let v = xml_to_value("<a>\n  <b>  spaced\n\tout  </b>\n</a>").expect("parse");
        assert_eq!(s(&v, "a.b"), "spaced out");
    }

    #[test]
    fn html_stripping_handles_blocks_scripts_and_escapes() {
        assert_eq!(strip_html("<p>one</p><p>two</p>"), "one two");
        assert_eq!(strip_html("a<br>b"), "a b");
        assert_eq!(strip_html("x<script>var a = 1;</script>y"), "x y");
        assert_eq!(strip_html("<a href=\"?a=1&b=2\">link</a>"), "link");
        assert_eq!(strip_html("&lt;p&gt;literal&lt;/p&gt;"), "<p>literal</p>");
        assert_eq!(strip_html("  plain   text  "), "plain text");
        assert_eq!(strip_html("no markup at all"), "no markup at all");
    }

    #[test]
    fn strip_html_in_place_reaches_every_string() {
        let mut v = xml_to_value(RSS).expect("parse");
        strip_html_in_place(&mut v);
        assert_eq!(s(&v, "rss.channel.item[0].description"), "Hello & welcome");
        assert_eq!(s(&v, "rss.channel.item[1].title"), "Second post");
    }

    #[test]
    fn dangling_amp_and_truncated_documents_are_tolerated() {
        let v = xml_to_value("<rss><channel><item><title>AT&T</title></item></channel></rss>")
            .expect("dangling amp");
        assert_eq!(s(&v, "rss.channel.item.title"), "AT&T");

        let truncated =
            xml_to_value("<rss><channel><item><title>Kept</title></item>").expect("truncated");
        assert_eq!(s(&truncated, "rss.channel.item.title"), "Kept");
    }

    #[test]
    fn a_body_with_no_markup_is_an_error_but_markup_is_accepted() {
        // The reader is deliberately lenient, so the failure worth catching is
        // a body that produced NO elements at all — an HTML error page served
        // as text, an empty response — rather than odd-looking markup.
        assert!(xml_to_value("Service Unavailable").is_err());
        assert!(xml_to_value("").is_err());
        assert!(xml_to_value("<rss><channel><item>").is_ok(), "partial document still renders");
        assert!(xml_to_value("<rss/>").is_ok(), "an empty root is still a document");
    }

    #[test]
    fn format_detection() {
        assert_eq!(sniff("  <rss>"), Format::Xml);
        assert_eq!(sniff("\u{feff}<feed>"), Format::Xml);
        assert_eq!(sniff("{\"a\":1}"), Format::Json);
        assert_eq!(resolve("{\"a\":1}", Format::Auto), Format::Json);
        assert_eq!(resolve("{}", Format::Xml), Format::Xml);
        assert_eq!(Format::parse("RSS"), Format::Xml);
        assert_eq!(Format::parse("nonsense"), Format::Auto);
        assert_eq!(decode("{\"a\":1}", Format::Auto).unwrap()["a"], Value::from(1));
        assert!(decode("<rss><channel/></rss>", Format::Auto).is_ok());
        assert_eq!(decode_with("{}", Format::Auto).unwrap().1, Format::Json);
        assert!(default_strip_html(Format::Xml));
        assert!(!default_strip_html(Format::Json));
    }

    #[test]
    fn repeated_siblings_beyond_the_cap_are_dropped_not_unbounded() {
        let mut xml = String::from("<rss><channel>");
        for i in 0..(MAX_REPEATS + 25) {
            xml.push_str(&format!("<item><title>{i}</title></item>"));
        }
        xml.push_str("</channel></rss>");
        let v = xml_to_value(&xml).expect("parse");
        let items = get_path(&v, "rss.channel.item")
            .and_then(Value::as_array)
            .expect("array");
        assert_eq!(items.len(), MAX_REPEATS);
    }

    /// The shape the feature was asked for, against the live Arch news feed
    /// (a real RSS 2.0 document, captured 2026-10-23).
    #[test]
    fn arch_news_feed_resolves_the_paths_a_provider_would_use() {
        let body = include_str!("../tests/fixtures/arch-news.xml");
        let mut v = decode_with(body, Format::Auto).expect("parse").0;
        assert_eq!(sniff(body), Format::Xml);

        assert_eq!(s(&v, "rss.channel.title"), "Arch Linux: Recent news updates");
        assert_eq!(s(&v, "rss.channel.link"), "https://archlinux.org/news/");

        let items = get_path(&v, "rss.channel.item")
            .and_then(Value::as_array)
            .expect("item array");
        assert!(items.len() >= 5, "expected a news list, got {}", items.len());

        let first = &items[0];
        for path in ["title", "link", "pubDate", "guid", "dc_creator"] {
            let value = get_path(first, path).unwrap_or_else(|| panic!("{path} missing"));
            assert!(
                !value_to_text(value).trim().is_empty(),
                "{path} resolved empty"
            );
        }
        assert_eq!(
            s(&v, "rss.channel.item[0].guid.#text").split(':').next(),
            Some("tag")
        );

        strip_html_in_place(&mut v);
        let body_text = s(&v, "rss.channel.item[0].description");
        assert!(!body_text.contains('<'), "HTML tags survived stripping");
        assert!(
            body_text.contains("PHP"),
            "stripped body lost its text: {body_text}"
        );
        assert!(
            body_text.len() > 60,
            "stripped body looks truncated: {body_text}"
        );
    }
}