//! A very small HTML-to-text renderer for the desktop Browser window.
//!
//! This is deliberately not a layout engine. It strips tags, drops the
//! contents of `script`, `style` and `head`, turns block-level elements into
//! line breaks, decodes the common character entities and word-wraps the
//! result to a fixed number of columns. That is enough to make real pages
//! (example.com, simple articles, documentation pages) readable in the 8x8
//! pixel console font.

use alloc::string::String;
use alloc::vec::Vec;

/// Renders `html` into display lines no wider than `columns` characters.
pub fn render(html: &str, columns: usize) -> Vec<String> {
    let columns = columns.max(8);
    wrap(&html_to_text(html), columns)
}

/// The page title from `<title>...</title>`, if the document has one.
pub fn title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open = start + lower[start..].find('>')? + 1;
    let end = lower[open..].find("</title>")? + open;
    let text = html[open..end].trim();
    if text.is_empty() {
        None
    } else {
        Some(decode_all(text))
    }
}

/// The status line (the first line of an HTTP response head).
pub fn status_line(response: &str) -> String {
    response.lines().next().unwrap_or("").trim().into()
}

/// The body of an HTTP response, after the blank line separating it from the
/// headers. Falls back to the whole text when no separator is present.
pub fn body(response: &str) -> &str {
    match response.split_once("\r\n\r\n") {
        Some((_, body)) => body,
        None => match response.split_once("\n\n") {
            Some((_, body)) => body,
            None => response,
        },
    }
}

// ---------------------------------------------------------------------------
// Tag stripping
// ---------------------------------------------------------------------------

/// Elements whose contents are not rendered as text.
const SKIP_ELEMENTS: &[&str] = &[
    "script", "style", "head", "title", "noscript", "svg", "template",
];

/// Elements that start a new line when they appear.
const BLOCK_ELEMENTS: &[&str] = &[
    "p",
    "div",
    "br",
    "hr",
    "li",
    "ul",
    "ol",
    "dl",
    "dt",
    "dd",
    "tr",
    "td",
    "th",
    "table",
    "thead",
    "tbody",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "footer",
    "section",
    "article",
    "nav",
    "aside",
    "main",
    "form",
    "blockquote",
    "pre",
    "figure",
    "figcaption",
    "fieldset",
    "address",
    "details",
    "summary",
    "body",
    "html",
];

fn is_block(name: &str) -> bool {
    BLOCK_ELEMENTS.contains(&name)
}

/// Strips the markup from `html`, returning collapsed plain text.
fn html_to_text(html: &str) -> String {
    // `to_ascii_lowercase` keeps byte offsets aligned (it only rewrites ASCII
    // bytes), so the lowered copy can be used to find tags by name while the
    // original supplies the text.
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut out = String::new();
    let mut i = 0usize;

    while i < bytes.len() {
        // Skip whole elements whose contents are not text.
        let mut skipped = false;
        for name in SKIP_ELEMENTS {
            let open = alloc::format!("<{name}");
            if lower[i..].starts_with(open.as_str()) {
                let close = alloc::format!("</{name}");
                if let Some(pos) = lower[i..].find(close.as_str()) {
                    i += pos + close.len();
                    if let Some(end) = lower[i..].find('>') {
                        i += end + 1;
                    }
                    skipped = true;
                } else {
                    return finish(out);
                }
                break;
            }
        }
        if skipped {
            continue;
        }

        let ch = bytes[i];

        if ch == b'<' {
            let Some(rel) = html[i..].find('>') else {
                break;
            };
            let tag = &lower[i + 1..i + rel];
            let name = tag
                .trim_start_matches('/')
                .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
                .next()
                .unwrap_or("");
            if is_block(name) {
                push_newline(&mut out);
            }
            i += rel + 1;
            continue;
        }

        if ch == b'&' {
            if let Some(rel) = html[i..].find(';') {
                if rel <= 12 {
                    let entity = &html[i + 1..i + rel];
                    if let Some(decoded) = decode_entity(entity) {
                        push_text(&mut out, &decoded);
                        i += rel + 1;
                        continue;
                    }
                }
            }
        }

        if ch.is_ascii_whitespace() {
            push_space(&mut out);
            i += 1;
            continue;
        }

        if ch < 0x80 {
            out.push(ch as char);
            i += 1;
            continue;
        }

        // Multibyte UTF-8: copy the whole character.
        let rest = &html[i..];
        match rest.chars().next() {
            Some(c) => {
                out.push(c);
                i += c.len_utf8();
            }
            None => break,
        }
    }

    finish(out)
}

fn finish(mut out: String) -> String {
    while out.ends_with([' ', '\n']) {
        out.pop();
    }
    out
}

/// Appends `text`, collapsing any run of whitespace to a single space.
fn push_text(out: &mut String, text: &str) {
    for ch in text.chars() {
        if ch.is_whitespace() {
            push_space(out);
        } else {
            out.push(ch);
        }
    }
}

fn push_space(out: &mut String) {
    if !out.is_empty() && !out.ends_with(' ') && !out.ends_with('\n') {
        out.push(' ');
    }
}

fn push_newline(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
    if out.ends_with('\n') || out.is_empty() {
        return;
    }
    out.push('\n');
}

// ---------------------------------------------------------------------------
// Character entities
// ---------------------------------------------------------------------------

fn decode_all(text: &str) -> String {
    let mut out = String::new();
    let mut i = 0usize;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(rel) = text[i..].find(';') {
                if rel <= 12 {
                    let entity = &text[i + 1..i + rel];
                    if let Some(decoded) = decode_entity(entity) {
                        push_text(&mut out, &decoded);
                        i += rel + 1;
                        continue;
                    }
                }
            }
        }
        if bytes[i].is_ascii_whitespace() {
            push_space(&mut out);
            i += 1;
        } else if bytes[i] < 0x80 {
            out.push(bytes[i] as char);
            i += 1;
        } else {
            match text[i..].chars().next() {
                Some(c) => {
                    out.push(c);
                    i += c.len_utf8();
                }
                None => break,
            }
        }
    }
    finish(out)
}

/// Decodes one entity body (the text between `&` and `;`).
fn decode_entity(entity: &str) -> Option<String> {
    if let Some(number) = entity.strip_prefix('#') {
        let code = if let Some(hex) = number.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            number.parse::<u32>().ok()?
        };
        // The 8x8 font only has glyphs for the low 7 bits; map the common
        // punctuation entities onto ASCII so they are still readable.
        let ch = match code {
            0xa0 => ' ',
            0x2013 | 0x2014 => '-',
            0x2018 | 0x2019 => '\'',
            0x201c | 0x201d => '"',
            0x2026 => '.',
            _ => char::from_u32(code)?,
        };
        return Some(String::from(ch));
    }

    let ch = match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" | "laquo" | "raquo" | "ldquo" | "rdquo" => '"',
        "apos" | "lsquo" | "rsquo" => '\'',
        "nbsp" => ' ',
        "mdash" | "ndash" | "minus" => '-',
        "hellip" => '.',
        "copy" => 'c',
        "reg" => 'r',
        "trade" => 't',
        "deg" => 'o',
        "middot" | "bull" => '*',
        "times" => 'x',
        "divide" => '/',
        "euro" | "pound" | "yen" | "cent" => '$',
        _ => return None,
    };
    Some(String::from(ch))
}

// ---------------------------------------------------------------------------
// Word wrapping
// ---------------------------------------------------------------------------

fn wrap(text: &str, columns: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();

    for paragraph in text.split('\n') {
        let paragraph = paragraph.trim();
        if paragraph.is_empty() {
            if lines.last().is_some_and(|line| !line.is_empty()) {
                lines.push(String::new());
            }
            continue;
        }

        let mut line = String::new();
        for word in paragraph.split(' ') {
            if word.is_empty() {
                continue;
            }
            let mut word = word;
            loop {
                let used = line.chars().count();
                let space = usize::from(!line.is_empty());
                let available = columns.saturating_sub(used + space);
                let width = word.chars().count();

                if width <= available {
                    if !line.is_empty() {
                        line.push(' ');
                    }
                    line.push_str(word);
                    break;
                }

                if available == 0 {
                    lines.push(core::mem::take(&mut line));
                    continue;
                }

                // The word does not fit: take as much as will.
                let cut = word
                    .char_indices()
                    .nth(available)
                    .map_or(word.len(), |(index, _)| index);
                let (head, tail) = word.split_at(cut);
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(head);
                lines.push(core::mem::take(&mut line));
                word = tail;
            }
        }
        lines.push(line);
    }

    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}
