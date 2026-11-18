//! A miniature browser engine for the desktop Browser window.
//!
//! Its pipeline follows the same broad shape as Gecko/Servo (the engine behind
//! Firefox), written from scratch for a `no_std` kernel:
//!
//! 1. an HTML5-ish tokenizer and tree builder produce a real DOM,
//! 2. CSS rules (from `<style>` blocks plus a user-agent default sheet) are
//!    matched with descendant/child selectors and a specificity-based cascade,
//! 3. computed styles are inherited down the DOM,
//! 4. a block/inline layout engine turns the styled tree into a display list of
//!    lines and boxes that the window manager paints.
//!
//! This is deliberately small: no JavaScript, no floats, no full CSS layout,
//! no separate fetching of external stylesheets or images. It does, however,
//! give real headings, nested elements, cascaded colours, margins/padding,
//! lists, links, rules, code blocks and form/image placeholders.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::console::Color;

/// Default page background.
pub const DEFAULT_BACKGROUND: Color = Color(0xFF_FF_FF);
const DEFAULT_TEXT: Color = Color(0x20_20_20);

// ---------------------------------------------------------------------------
// Public display list types (consumed by the window manager)
// ---------------------------------------------------------------------------

/// Why a line exists in the display list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Normal,
    Heading(u8),
    ListItem(usize),
    Quote,
    Pre,
    Rule,
    Image,
    Button,
    /// Empty vertical space used for margins.
    Spacer(usize),
}

/// Text style for one run of characters.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SpanStyle {
    pub color: Color,
    pub bold: bool,
    pub underline: bool,
    pub italic: bool,
    /// 1 = 8x8 font, 2 = 16x16, 3 = 24x24.
    pub scale: usize,
}

impl Default for SpanStyle {
    fn default() -> Self {
        Self {
            color: DEFAULT_TEXT,
            bold: false,
            underline: false,
            italic: false,
            scale: 1,
        }
    }
}

/// One run of text sharing a style.
#[derive(Clone)]
pub struct Span {
    pub text: String,
    pub style: SpanStyle,
    /// Target of the enclosing `<a href>`, if this run is a link.
    pub link: Option<String>,
}

/// Horizontal alignment of a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// A background box drawn behind one or more lines.
#[derive(Clone, Copy)]
pub struct BoxDecor {
    pub x: usize,
    pub w: usize,
    pub color: Color,
}

/// One display line.
#[derive(Clone)]
pub struct Line {
    pub kind: LineKind,
    pub spans: Vec<Span>,
    pub indent: usize,
    pub background: Option<Color>,
    pub align: Align,
    pub decor: Option<BoxDecor>,
}

impl Line {
    fn new(kind: LineKind) -> Self {
        Self {
            kind,
            spans: Vec::new(),
            indent: 0,
            background: None,
            align: Align::Left,
            decor: None,
        }
    }

    /// A blank spacer line.
    pub fn blank() -> Self {
        Self::new(LineKind::Normal)
    }

    /// A heading line used for the document title.
    pub fn heading(text: &str) -> Self {
        let mut line = Self::new(LineKind::Heading(1));
        line.spans.push(Span {
            text: text.to_string(),
            style: SpanStyle {
                scale: 3,
                bold: true,
                ..SpanStyle::default()
            },
            link: None,
        });
        line
    }

    /// Height of this line in pixels.
    #[must_use]
    pub fn height(&self) -> usize {
        match self.kind {
            LineKind::Spacer(height) => height,
            LineKind::Heading(1) => 28,
            LineKind::Heading(2) => 22,
            LineKind::Heading(_) => 17,
            LineKind::Rule => 10,
            LineKind::Image => 52,
            LineKind::Button => 26,
            _ => {
                let scale = self.spans.first().map_or(1, |span| span.style.scale.max(1));
                8 * scale + 4
            }
        }
    }
}

/// A laid-out page ready for the window manager to paint.
#[derive(Clone)]
pub struct Page {
    pub lines: Vec<Line>,
    pub background: Color,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            background: DEFAULT_BACKGROUND,
        }
    }
}

impl Page {
    /// Builds a page from plain strings (status and error pages).
    #[must_use]
    pub fn from_text(lines: &[&str], _columns: usize) -> Self {
        let mut page = Page::default();
        for text in lines {
            let mut line = Line::new(LineKind::Normal);
            line.spans.push(Span {
                text: (*text).to_string(),
                style: SpanStyle::default(),
                link: None,
            });
            page.lines.push(line);
        }
        page
    }

    /// Total height of the page in pixels.
    #[must_use]
    pub fn height(&self) -> usize {
        self.lines.iter().map(Line::height).sum()
    }
}

/// Renders HTML into a styled page. `columns` controls the wrap width.
#[must_use]
pub fn render(html: &str, columns: usize) -> Page {
    let width = columns.saturating_mul(8).max(160);
    let dom = Dom::parse(html);
    let sheets = inline_stylesheets(&dom);
    Engine::new(&dom, &sheets, width).run()
}

/// The page title from `<title>`, if any.
#[must_use]
pub fn title(html: &str) -> Option<String> {
    let dom = Dom::parse(html);
    let id = dom.elements().find(|&id| dom.tag(id) == "title")?;
    let mut text = String::new();
    collect_text(&dom, id, &mut text);
    let text = decode_entities(text.trim());
    (!text.is_empty()).then_some(text)
}

// ---------------------------------------------------------------------------
// Small shared utilities
// ---------------------------------------------------------------------------

fn decode_entities(text: &str) -> String {
    let mut out = String::new();
    let mut i = 0usize;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(rel) = text[i..].find(';') {
                if rel <= 12 {
                    let entity = &text[i + 1..i + rel];
                    if let Some(decoded) = decode_entity(entity) {
                        out.push_str(&decoded);
                        i += rel + 1;
                        continue;
                    }
                }
            }
        }
        if let Some(ch) = text[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn decode_entity(entity: &str) -> Option<String> {
    if let Some(number) = entity.strip_prefix('#') {
        let code = if let Some(hex) = number.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            number.parse::<u32>().ok()?
        };
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
        "quot" | "ldquo" | "rdquo" => '"',
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

fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() == 3 {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()?;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()?;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()?;
            return Some(Color(
                ((r * 17) as u32) << 16 | ((g * 17) as u32) << 8 | (b * 17) as u32,
            ));
        }
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color((r as u32) << 16 | (g as u32) << 8 | b as u32));
        }
        return None;
    }
    if let Some(inner) = value.strip_prefix("rgb(").and_then(|v| v.strip_suffix(')')) {
        let mut channels = inner.split(',');
        let r = channels.next()?.trim().parse::<u8>().ok()?;
        let g = channels.next()?.trim().parse::<u8>().ok()?;
        let b = channels.next()?.trim().parse::<u8>().ok()?;
        return Some(Color((r as u32) << 16 | (g as u32) << 8 | b as u32));
    }
    let named = match value {
        "black" => 0x00_00_00,
        "white" => 0xFF_FF_FF,
        "red" => 0xFF_00_00,
        "green" => 0x00_80_00,
        "blue" => 0x00_00_FF,
        "navy" => 0x00_00_80,
        "teal" => 0x00_80_80,
        "gray" | "grey" => 0x80_80_80,
        "silver" => 0xC0_C0_C0,
        "yellow" => 0xFF_FF_00,
        "orange" => 0xFF_A5_00,
        "purple" => 0x80_00_80,
        "maroon" => 0x80_00_00,
        "darkblue" => 0x00_00_8B,
        "lightgray" | "lightgrey" => 0xD3_D3_D3,
        _ => return None,
    };
    Some(Color(named))
}

/// Parses a CSS length into pixels. `em` is the current font size.
fn parse_length(value: &str, em: usize) -> Option<usize> {
    let value = value.trim();
    if value == "0" {
        return Some(0);
    }
    if let Some(px) = value.strip_suffix("px") {
        return px.trim().parse::<usize>().ok();
    }
    if let Some(em_value) = value.strip_suffix("em") {
        return parse_relative_length(em_value, em, 1);
    }
    if let Some(percent) = value.strip_suffix('%') {
        return parse_relative_length(percent, em, 100);
    }
    None
}

/// Parses a decimal number that is multiplied by `em` and divided by `divisor`.
/// Fixed-point arithmetic keeps this `no_std` engine free of floating point.
fn parse_relative_length(value: &str, em: usize, divisor: usize) -> Option<usize> {
    let value = value.trim();
    let (whole_part, fraction) = value.split_once('.').unwrap_or((value, ""));
    let whole = if whole_part.is_empty() {
        0
    } else {
        whole_part.parse::<usize>().ok()?
    };
    let mut digits = [b'0'; 3];
    for (index, byte) in fraction.bytes().take(3).enumerate() {
        if !byte.is_ascii_digit() {
            return None;
        }
        digits[index] = byte;
    }
    let fraction = ((digits[0] - b'0') as usize) * 100
        + ((digits[1] - b'0') as usize) * 10
        + (digits[2] - b'0') as usize;
    let scaled = whole.saturating_mul(1000).saturating_add(fraction);
    Some(scaled.saturating_mul(em) / (1000 * divisor))
}

// ---------------------------------------------------------------------------
// DOM: tokenizer and tree builder
// ---------------------------------------------------------------------------

enum Node {
    Element(Elem),
    Text(String),
}

struct Elem {
    tag: String,
    attrs: Vec<(String, String)>,
    children: Vec<usize>,
}

/// A parsed document tree. Nodes are indices so children can borrow freely.
pub struct Dom {
    nodes: Vec<Node>,
    /// Parents are stored for every node (including text) so inherited styles
    /// work for text runs as well as elements.
    parents: Vec<Option<usize>>,
    root: usize,
}

impl Dom {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            parents: Vec::new(),
            root: 0,
        }
    }

    fn push(&mut self, node: Node, parent: Option<usize>) -> usize {
        let id = self.nodes.len();
        self.nodes.push(node);
        self.parents.push(parent);
        id
    }

    fn parse(html: &str) -> Self {
        let mut dom = Dom::new();
        dom.root = dom.push(
            Node::Element(Elem {
                tag: String::from("html"),
                attrs: Vec::new(),
                children: Vec::new(),
            }),
            None,
        );
        let mut stack = vec![dom.root];
        for token in tokenize(html) {
            match token {
                Token::Open {
                    name,
                    attrs,
                    self_closing,
                } => {
                    if is_void(&name) || self_closing {
                        let parent = *stack.last().unwrap_or(&dom.root);
                        dom.append(
                            parent,
                            Node::Element(Elem {
                                tag: name,
                                attrs,
                                children: Vec::new(),
                            }),
                        );
                        continue;
                    }
                    implicit_close(&mut stack, &dom, &name);
                    let parent = *stack.last().unwrap_or(&dom.root);
                    let id = dom.push(
                        Node::Element(Elem {
                            tag: name,
                            attrs,
                            children: Vec::new(),
                        }),
                        Some(parent),
                    );
                    if let Some(parent_elem) = dom.nodes[parent].as_element_mut() {
                        parent_elem.children.push(id);
                    }
                    stack.push(id);
                }
                Token::Close { name } => {
                    if let Some(pos) = stack.iter().rposition(|&id| dom.tag(id) == name) {
                        stack.truncate(pos);
                    }
                }
                Token::Text(text) => {
                    if text.is_empty() {
                        continue;
                    }
                    let parent = *stack.last().unwrap_or(&dom.root);
                    dom.append(parent, Node::Text(text));
                }
            }
        }
        dom
    }

    fn append(&mut self, parent: usize, node: Node) {
        let id = self.push(node, Some(parent));
        if let Some(elem) = self.nodes[parent].as_element_mut() {
            elem.children.push(id);
        }
    }

    fn elements(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.nodes.len()).filter(|&id| matches!(self.nodes[id], Node::Element(_)))
    }

    fn tag(&self, id: usize) -> &str {
        match &self.nodes[id] {
            Node::Element(elem) => &elem.tag,
            Node::Text(_) => "",
        }
    }

    fn attr(&self, id: usize, name: &str) -> Option<&str> {
        let elem = self.nodes[id].as_element()?;
        elem.attrs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn parent(&self, id: usize) -> Option<usize> {
        self.parents.get(id).copied().flatten()
    }

    fn children(&self, id: usize) -> &[usize] {
        self.nodes[id]
            .as_element()
            .map_or(&[], |elem| elem.children.as_slice())
    }
}

impl Node {
    fn as_element(&self) -> Option<&Elem> {
        match self {
            Node::Element(elem) => Some(elem),
            Node::Text(_) => None,
        }
    }

    fn as_element_mut(&mut self) -> Option<&mut Elem> {
        match self {
            Node::Element(elem) => Some(elem),
            Node::Text(_) => None,
        }
    }
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "source"
            | "track"
            | "wbr"
    )
}

fn is_raw_text(name: &str) -> bool {
    matches!(name, "script" | "style" | "title" | "textarea" | "noscript")
}

fn is_block(tag: &str) -> bool {
    matches!(
        tag,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "center"
            | "div"
            | "dl"
            | "dt"
            | "dd"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hr"
            | "html"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "table"
            | "tbody"
            | "td"
            | "tfoot"
            | "th"
            | "thead"
            | "tr"
            | "ul"
    )
}

fn implicit_close(stack: &mut Vec<usize>, dom: &Dom, new_tag: &str) {
    while let Some(&top) = stack.last() {
        let top_tag = dom.tag(top);
        let close = match (top_tag, new_tag) {
            ("p", tag) if tag == "p" || is_block(tag) => true,
            ("li", "li")
            | ("option", "option")
            | ("tr", "tr")
            | ("dt" | "dd", "dt" | "dd")
            | ("td" | "th", "td" | "th") => true,
            _ => false,
        };
        if !close {
            break;
        }
        stack.pop();
    }
}

enum Token {
    Open {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    Close {
        name: String,
    },
    Text(String),
}

fn tokenize(html: &str) -> Vec<Token> {
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let Some(rel) = lower[i..].find('<') else {
            tokens.push(Token::Text(decode_entities(&html[i..])));
            break;
        };
        if rel > 0 {
            tokens.push(Token::Text(decode_entities(&html[i..i + rel])));
        }
        i += rel;
        if lower[i..].starts_with("<!--") {
            if let Some(end) = lower[i + 4..].find("-->") {
                i += 4 + end + 3;
                continue;
            }
            break;
        }
        if lower[i..].starts_with("<!") || lower[i..].starts_with("<?") {
            if let Some(end) = lower[i..].find('>') {
                i += end + 1;
                continue;
            }
            break;
        }
        let Some(end) = lower[i..].find('>') else {
            break;
        };
        let content = &html[i + 1..i + end];
        let content_lower = &lower[i + 1..i + end];
        i += end + 1;

        let trimmed = content_lower.trim_start();
        let closing = trimmed.starts_with('/');
        let body = trimmed.trim_start_matches('/').trim_start();
        let name_end = body
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(body.len());
        let name = body[..name_end].to_string();
        if name.is_empty() {
            continue;
        }
        if closing {
            tokens.push(Token::Close { name });
            continue;
        }

        let attrs = parse_attrs(&content[name_end + (content.len() - content_lower.len())..]);
        let self_closing = content_lower.trim_end().ends_with('/');
        tokens.push(Token::Open {
            name: name.clone(),
            attrs,
            self_closing,
        });

        if is_raw_text(&name) && !self_closing {
            let close = alloc::format!("</{name}");
            if let Some(pos) = lower[i..].find(close.as_str()) {
                tokens.push(Token::Text(html[i..i + pos].to_string()));
                i += pos;
            }
        }
    }
    tokens
}

fn parse_attrs(input: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] == b'/' {
            break;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
            i += 1;
        }
        let name = input[start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                value.push_str(&input[start..i.min(input.len())]);
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                let start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'/' {
                    i += 1;
                }
                value.push_str(&input[start..i]);
            }
        }
        if !name.is_empty() {
            attrs.push((name, value));
        }
    }
    attrs
}

fn collect_text(dom: &Dom, id: usize, out: &mut String) {
    match &dom.nodes[id] {
        Node::Text(text) => out.push_str(text),
        Node::Element(_) => {
            for &child in dom.children(id) {
                collect_text(dom, child, out);
            }
        }
    }
}

fn inline_stylesheets(dom: &Dom) -> Vec<String> {
    let mut sheets = Vec::new();
    for id in dom.elements() {
        if dom.tag(id) == "style" {
            let mut text = String::new();
            collect_text(dom, id, &mut text);
            sheets.push(text);
        }
    }
    sheets
}

// ---------------------------------------------------------------------------
// CSS: selectors, declarations and cascade
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Compound {
    tag: Option<String>,
    classes: Vec<String>,
    ids: Vec<String>,
    attrs: Vec<String>,
}

#[derive(Clone, Copy)]
enum Combinator {
    Descendant,
    Child,
}

#[derive(Clone)]
struct Selector {
    compounds: Vec<Compound>,
    combinators: Vec<Combinator>,
    specificity: (u32, u32, u32),
}

#[derive(Clone)]
struct Decl {
    property: String,
    value: String,
    important: bool,
}

#[derive(Clone)]
struct Rule {
    selectors: Vec<Selector>,
    decls: Vec<Decl>,
    order: usize,
}

struct Stylesheet {
    rules: Vec<Rule>,
}

/// Sort key for the CSS cascade: importance, specificity and source order.
type CascadeKey = (bool, u32, u32, u32, usize);

fn parse_stylesheet(css: &str, order: usize) -> Stylesheet {
    let css = strip_comments(css);
    let mut rules = Vec::new();
    let mut rest = css.as_str();
    while let Some(open) = rest.find('{') {
        let prelude = rest[..open].trim();
        let Some(close) = matching_brace(rest, open) else {
            break;
        };
        let body = &rest[open + 1..close];
        rest = &rest[close + 1..];
        if prelude.starts_with('@') {
            // Parse the contents of @media/@supports blocks as ordinary rules.
            rules.extend(parse_stylesheet(body, order).rules);
            continue;
        }
        let selectors = prelude
            .split(',')
            .filter_map(parse_selector)
            .collect::<Vec<_>>();
        if selectors.is_empty() {
            continue;
        }
        rules.push(Rule {
            selectors,
            decls: parse_decls(body),
            order,
        });
    }
    Stylesheet { rules }
}

fn parse_inline(css: &str, order: usize) -> Rule {
    Rule {
        selectors: vec![Selector {
            compounds: vec![Compound {
                tag: None,
                classes: Vec::new(),
                ids: Vec::new(),
                attrs: Vec::new(),
            }],
            combinators: Vec::new(),
            specificity: (u32::MAX, 0, 0),
        }],
        decls: parse_decls(css),
        order,
    }
}

fn strip_comments(css: &str) -> String {
    let mut out = String::new();
    let mut rest = css;
    while let Some(pos) = rest.find("/*") {
        out.push_str(&rest[..pos]);
        if let Some(end) = rest[pos + 2..].find("*/") {
            rest = &rest[pos + 2 + end + 2..];
        } else {
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

fn matching_brace(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, ch) in text[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_selector(text: &str) -> Option<Selector> {
    let mut compounds = Vec::new();
    let mut combinators = Vec::new();
    let mut current = String::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let ch = bytes[i] as char;
        if ch.is_whitespace() {
            if !current.is_empty() {
                compounds.push(parse_compound(&current)?);
                current.clear();
                combinators.push(Combinator::Descendant);
            }
            i += 1;
        } else if ch == '>' {
            if !current.is_empty() {
                compounds.push(parse_compound(&current)?);
                current.clear();
            }
            combinators.push(Combinator::Child);
            i += 1;
        } else {
            current.push(ch);
            i += 1;
        }
    }
    if !current.is_empty() {
        compounds.push(parse_compound(&current)?);
    }
    // A trailing combinator means the selector is malformed.
    if combinators.len() >= compounds.len() {
        return None;
    }
    let specificity = compounds.iter().fold((0, 0, 0), |mut acc, part| {
        acc.0 += part.ids.len() as u32;
        acc.1 += part.classes.len() as u32 + part.attrs.len() as u32;
        acc.2 += u32::from(part.tag.is_some());
        acc
    });
    if compounds.is_empty() {
        None
    } else {
        Some(Selector {
            compounds,
            combinators,
            specificity,
        })
    }
}

fn parse_compound(text: &str) -> Option<Compound> {
    let mut compound = Compound {
        tag: None,
        classes: Vec::new(),
        ids: Vec::new(),
        attrs: Vec::new(),
    };
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] as char {
            // The universal selector leaves every constraint unset, which
            // already matches every element.
            '*' => i += 1,
            '.' => {
                i += 1;
                let start = i;
                while i < bytes.len() && is_ident_byte(bytes[i]) {
                    i += 1;
                }
                compound.classes.push(text[start..i].to_ascii_lowercase());
            }
            '#' => {
                i += 1;
                let start = i;
                while i < bytes.len() && is_ident_byte(bytes[i]) {
                    i += 1;
                }
                compound.ids.push(text[start..i].to_ascii_lowercase());
            }
            '[' => {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
                compound.attrs.push(text[start..i].to_ascii_lowercase());
                if i < bytes.len() {
                    i += 1;
                }
            }
            _ if is_ident_byte(bytes[i]) => {
                let start = i;
                while i < bytes.len() && is_ident_byte(bytes[i]) {
                    i += 1;
                }
                compound.tag = Some(text[start..i].to_ascii_lowercase());
            }
            _ => return None,
        }
    }
    Some(compound)
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

fn parse_decls(body: &str) -> Vec<Decl> {
    let mut decls = Vec::new();
    for part in body.split(';') {
        let Some((property, value)) = part.split_once(':') else {
            continue;
        };
        let important = value.trim_end().ends_with("!important");
        let value = if important {
            value.trim_end().strip_suffix("!important").unwrap_or(value)
        } else {
            value
        };
        let property = property.trim().to_ascii_lowercase();
        let value = value.trim().to_ascii_lowercase();
        if property.is_empty() || value.is_empty() {
            continue;
        }
        decls.push(Decl {
            property,
            value,
            important,
        });
    }
    decls
}

fn compound_matches(dom: &Dom, id: usize, compound: &Compound) -> bool {
    if let Some(tag) = &compound.tag {
        if dom.tag(id) != tag {
            return false;
        }
    }
    let classes: Vec<&str> = dom
        .attr(id, "class")
        .unwrap_or("")
        .split_whitespace()
        .collect();
    for wanted in &compound.classes {
        if !classes
            .iter()
            .any(|class| class.eq_ignore_ascii_case(wanted))
        {
            return false;
        }
    }
    if let Some(wanted) = dom.attr(id, "id") {
        if !compound
            .ids
            .iter()
            .all(|id| id.eq_ignore_ascii_case(wanted))
        {
            return false;
        }
    } else if !compound.ids.is_empty() {
        return false;
    }
    for wanted in &compound.attrs {
        if dom.attr(id, wanted).is_none() {
            return false;
        }
    }
    true
}

fn selector_matches(dom: &Dom, id: usize, selector: &Selector) -> bool {
    let last = selector.compounds.len() - 1;
    if !compound_matches(dom, id, &selector.compounds[last]) {
        return false;
    }
    let mut current = id;
    for index in (0..last).rev() {
        match selector.combinators[index] {
            Combinator::Child => {
                let Some(parent) = dom.parent(current) else {
                    return false;
                };
                if !compound_matches(dom, parent, &selector.compounds[index]) {
                    return false;
                }
                current = parent;
            }
            Combinator::Descendant => {
                let mut ancestor = dom.parent(current);
                let mut found = false;
                while let Some(parent) = ancestor {
                    if compound_matches(dom, parent, &selector.compounds[index]) {
                        found = true;
                        current = parent;
                        break;
                    }
                    ancestor = dom.parent(parent);
                }
                if !found {
                    return false;
                }
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Computed styles
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Display {
    None,
    Block,
    Inline,
    InlineBlock,
}

// Text rendering flags are intentionally stored as plain booleans: the engine
// multiplies them by the million of nodes only when it computes a page.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone)]
struct Style {
    color: Color,
    background: Color,
    font_size: usize,
    bold: bool,
    italic: bool,
    underline: bool,
    align: Align,
    display: Display,
    margin: [usize; 4],
    padding: [usize; 4],
    width: Option<usize>,
    pre: bool,
    list_marker: Option<String>,
}

impl Style {
    fn initial() -> Self {
        Self {
            color: DEFAULT_TEXT,
            background: DEFAULT_BACKGROUND,
            font_size: 12,
            bold: false,
            italic: false,
            underline: false,
            align: Align::Left,
            display: Display::Inline,
            margin: [0; 4],
            padding: [0; 4],
            width: None,
            pre: false,
            list_marker: None,
        }
    }

    fn scale(&self) -> usize {
        if self.font_size <= 16 {
            1
        } else if self.font_size <= 24 {
            2
        } else {
            3
        }
    }

    fn inherit(&self) -> Self {
        Self {
            color: self.color,
            background: DEFAULT_BACKGROUND,
            font_size: self.font_size,
            bold: self.bold,
            italic: self.italic,
            underline: self.underline,
            align: self.align,
            display: Display::Inline,
            margin: [0; 4],
            padding: [0; 4],
            width: None,
            pre: self.pre,
            list_marker: self.list_marker.clone(),
        }
    }
}

const UA_SHEET: &str = "
html, body { display: block; margin: 0; color: #202020; background: #ffffff; font-size: 12px; }
address, article, aside, blockquote, center, div, dl, dt, dd, fieldset, figure,
figcaption, footer, form, header, hgroup, main, nav, ol, p, pre, section, table,
ul { display: block; margin-top: 8px; margin-bottom: 8px; }
h1 { display: block; font-size: 30px; font-weight: bold; margin-top: 14px; margin-bottom: 8px; }
h2 { display: block; font-size: 24px; font-weight: bold; margin-top: 12px; margin-bottom: 8px; }
h3 { display: block; font-size: 19px; font-weight: bold; margin-top: 10px; margin-bottom: 6px; }
h4, h5, h6 { display: block; font-size: 16px; font-weight: bold; margin-top: 8px; margin-bottom: 6px; }
a { color: #0000ee; text-decoration: underline; }
b, strong { font-weight: bold; }
i, em, cite, q { font-style: italic; }
pre, code, kbd, samp { font-family: monospace; }
pre { white-space: pre; }
ul { list-style-type: disc; }
ol { list-style-type: decimal; }
li { display: block; }
blockquote { margin-left: 16px; font-style: italic; }
hr { display: block; margin-top: 6px; margin-bottom: 6px; }
img, input, button, select, textarea { display: inline-block; }
table { display: block; }
tr { display: block; }
td, th { display: inline-block; padding-right: 8px; }
script, style, head, title, noscript, template, meta, link, base { display: none; }
";

fn compute_styles(dom: &Dom, sheets: &[String]) -> Vec<Style> {
    let mut all_rules = Vec::new();
    all_rules.extend(parse_stylesheet(UA_SHEET, 0).rules);
    for (index, sheet) in sheets.iter().enumerate() {
        all_rules.extend(parse_stylesheet(sheet, index + 1).rules);
    }
    let inline_order = sheets.len() + 1;

    let mut styles = alloc::vec![Style::initial(); dom.nodes.len()];
    for id in 0..dom.nodes.len() {
        if !matches!(dom.nodes[id], Node::Element(_)) {
            styles[id] = styles
                .get(dom.parent(id).unwrap_or(0))
                .cloned()
                .unwrap_or_else(Style::initial);
            continue;
        }

        let inherited = styles
            .get(dom.parent(id).unwrap_or(0))
            .map_or_else(Style::initial, Style::inherit);
        let mut style = inherited;

        // Cascade individual declarations, not whole rules: `!important`
        // belongs to one declaration and must still beat a later inline one.
        let mut cascade: Vec<(&Decl, CascadeKey)> = Vec::new();
        for rule in &all_rules {
            if rule
                .selectors
                .iter()
                .any(|selector| selector_matches(dom, id, selector))
            {
                let spec = rule.selectors.iter().fold((0, 0, 0), |acc, sel| {
                    (
                        acc.0.max(sel.specificity.0),
                        acc.1.max(sel.specificity.1),
                        acc.2.max(sel.specificity.2),
                    )
                });
                for decl in &rule.decls {
                    cascade.push((decl, (decl.important, spec.0, spec.1, spec.2, rule.order)));
                }
            }
        }
        let inline = dom
            .attr(id, "style")
            .map(|css| parse_inline(css, inline_order));
        if let Some(rule) = &inline {
            for decl in &rule.decls {
                cascade.push((decl, (decl.important, u32::MAX, 0, 0, rule.order)));
            }
        }
        cascade.sort_by_key(|(_, key)| *key);
        for &(decl, _) in &cascade {
            apply_decls(&mut style, core::slice::from_ref(decl));
        }

        // Element defaults that the UA sheet cannot express with selectors.
        match dom.tag(id) {
            "script" | "style" | "head" | "title" | "noscript" | "template" | "meta" | "link"
            | "base" => style.display = Display::None,
            "pre" => style.pre = true,
            "br" => style.display = Display::Inline,
            _ => {}
        }
        styles[id] = style;
    }
    styles
}

fn apply_decls(style: &mut Style, decls: &[Decl]) {
    for decl in decls {
        let value = decl.value.as_str();
        match decl.property.as_str() {
            "color" => {
                if let Some(color) = parse_color(value) {
                    style.color = color;
                }
            }
            "background" | "background-color" => {
                if let Some(color) = parse_color(value) {
                    style.background = color;
                }
            }
            "font-size" => {
                if let Some(size) = parse_length(value, style.font_size) {
                    style.font_size = size.clamp(6, 96);
                } else if matches!(value, "large" | "larger" | "x-large" | "xx-large") {
                    style.font_size = (style.font_size * 3 / 2).min(96);
                } else if matches!(value, "small" | "smaller" | "x-small") {
                    style.font_size = (style.font_size * 2 / 3).max(6);
                }
            }
            "font-weight" => {
                style.bold = matches!(value, "bold" | "bolder" | "600" | "700" | "800" | "900");
            }
            "font-style" => style.italic = matches!(value, "italic" | "oblique"),
            "text-decoration" | "text-decoration-line" => {
                if !value.contains("none") {
                    style.underline = value.contains("underline");
                }
            }
            "text-align" => {
                style.align = match value {
                    "center" => Align::Center,
                    "right" | "end" => Align::Right,
                    _ => Align::Left,
                }
            }
            "display" => {
                style.display = match value {
                    "none" => Display::None,
                    "block" => Display::Block,
                    "inline-block" => Display::InlineBlock,
                    _ => Display::Inline,
                }
            }
            "white-space" => style.pre = matches!(value, "pre" | "pre-wrap"),
            "list-style-type" | "list-style" => {
                style.list_marker = match value {
                    "none" => None,
                    _ => Some(value.to_string()),
                }
            }
            "width" => style.width = parse_length(value, style.font_size),
            "margin" => {
                let parts = parse_box(value, style.font_size);
                style.margin = box_sides(&parts);
            }
            "padding" => {
                let parts = parse_box(value, style.font_size);
                style.padding = box_sides(&parts);
            }
            "margin-top" => style.margin[0] = parse_length(value, style.font_size).unwrap_or(0),
            "margin-right" => style.margin[1] = parse_length(value, style.font_size).unwrap_or(0),
            "margin-bottom" => style.margin[2] = parse_length(value, style.font_size).unwrap_or(0),
            "margin-left" => style.margin[3] = parse_length(value, style.font_size).unwrap_or(0),
            "padding-top" => style.padding[0] = parse_length(value, style.font_size).unwrap_or(0),
            "padding-right" => style.padding[1] = parse_length(value, style.font_size).unwrap_or(0),
            "padding-bottom" => {
                style.padding[2] = parse_length(value, style.font_size).unwrap_or(0);
            }
            "padding-left" => style.padding[3] = parse_length(value, style.font_size).unwrap_or(0),
            _ => {}
        }
    }
}

fn parse_box(value: &str, em: usize) -> Vec<usize> {
    value
        .split_whitespace()
        .filter_map(|part| parse_length(part, em))
        .collect()
}

fn box_sides(parts: &[usize]) -> [usize; 4] {
    match parts {
        [] => [0; 4],
        [all] => [*all; 4],
        [vertical, horizontal] => [*vertical, *horizontal, *vertical, *horizontal],
        [top, horizontal, bottom] => [*top, *horizontal, *bottom, *horizontal],
        [top, right, bottom, left, ..] => [*top, *right, *bottom, *left],
    }
}

// ---------------------------------------------------------------------------
// Layout: turns the styled DOM into a display list
// ---------------------------------------------------------------------------

struct Flow {
    lines: Vec<Line>,
    current: Line,
    used: usize,
    width: usize,
    /// Href of the `<a>` currently being laid out, if any.
    link: Option<String>,
}

impl Flow {
    fn new(width: usize) -> Self {
        Self {
            lines: Vec::new(),
            current: Line::new(LineKind::Normal),
            used: 0,
            width: width.max(8),
            link: None,
        }
    }

    /// Lays out children with a new link target, then restores the old one.
    fn with_link(&mut self, href: &str, children: impl FnOnce(&mut Self)) {
        let old = self.link.replace(href.to_string());
        children(self);
        self.link = old;
    }

    fn flush(&mut self) {
        if !self.current.spans.is_empty() {
            let line = core::mem::replace(&mut self.current, Line::new(LineKind::Normal));
            self.lines.push(line);
        }
        self.used = 0;
    }

    fn push_text(&mut self, text: &str, style: &Style) {
        let collapsed = if style.pre {
            text.to_string()
        } else {
            collapse_whitespace(text)
        };
        let scale = style.scale();
        let char_width = 8 * scale;
        for paragraph in collapsed.split('\n') {
            for word in paragraph.split_whitespace() {
                let mut word = word;
                loop {
                    let width = word.chars().count() * char_width;
                    if self.used > 0 && self.used + char_width + width > self.width {
                        self.flush();
                        continue;
                    }
                    if self.used > 0 {
                        self.push_span(" ", style, scale);
                        self.used += char_width;
                    }
                    if width <= self.width.saturating_sub(self.used) || self.width <= char_width {
                        self.push_span(word, style, scale);
                        self.used += width;
                        break;
                    }
                    let room = self.width.saturating_sub(self.used) / char_width;
                    let cut = word
                        .char_indices()
                        .nth(room.max(1))
                        .map_or(word.len(), |(index, _)| index);
                    let (head, tail) = word.split_at(cut);
                    self.push_span(head, style, scale);
                    self.flush();
                    word = tail;
                }
            }
            if style.pre {
                self.flush();
            }
        }
    }

    fn push_span(&mut self, text: &str, style: &Style, scale: usize) {
        let span_style = SpanStyle {
            color: style.color,
            bold: style.bold,
            italic: style.italic,
            underline: style.underline,
            scale,
        };
        if let Some(last) = self.current.spans.last_mut() {
            if last.style == span_style {
                last.text.push_str(text);
                return;
            }
        }
        self.current.spans.push(Span {
            text: text.to_string(),
            style: span_style,
            link: self.link.clone(),
        });
    }

    fn push_line(&mut self, line: Line) {
        self.flush();
        self.lines.push(line);
    }
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(ch);
        }
    }
    out
}

struct Engine<'a> {
    dom: &'a Dom,
    styles: Vec<Style>,
    page: Page,
    width: usize,
    list_stack: Vec<(bool, usize)>,
}

impl<'a> Engine<'a> {
    fn new(dom: &'a Dom, sheets: &[String], width: usize) -> Self {
        let styles = compute_styles(dom, sheets);
        let mut engine = Self {
            dom,
            styles,
            page: Page::default(),
            width,
            list_stack: Vec::new(),
        };
        // Body background wins over the default white page.
        for id in dom.elements() {
            if matches!(dom.tag(id), "body" | "html") {
                engine.page.background = engine.styles[id].background;
            }
        }
        engine
    }

    fn run(mut self) -> Page {
        let root = self.dom.root;
        let mut flow = Flow::new(self.width);
        self.layout_node(root, None, &mut flow);
        self.drain_flow(&mut flow);
        self.page
    }

    /// Lays out every child of `id`, used for both inline and block boxes.
    fn layout_children(&mut self, id: usize, decor: Option<BoxDecor>, flow: &mut Flow) {
        for &child in self.dom.children(id) {
            self.layout_node(child, decor, flow);
        }
    }

    fn drain_flow(&mut self, flow: &mut Flow) {
        for line in flow.lines.drain(..) {
            if self.page.lines.len() < 1200 {
                self.page.lines.push(line);
            }
        }
    }

    fn spacer(&mut self, height: usize) {
        if height == 0 || self.page.lines.len() >= 1200 {
            return;
        }
        self.page.lines.push(Line::new(LineKind::Spacer(height)));
    }

    #[allow(clippy::too_many_lines)]
    fn layout_node(&mut self, id: usize, decor: Option<BoxDecor>, flow: &mut Flow) {
        match &self.dom.nodes[id] {
            Node::Text(text) => {
                let style = self.styles[id].clone();
                flow.push_text(text, &style);
                return;
            }
            Node::Element(_) => {}
        }

        let tag = self.dom.tag(id).to_string();
        let style = self.styles[id].clone();
        if style.display == Display::None {
            return;
        }

        match tag.as_str() {
            "br" => {
                flow.flush();
                return;
            }
            "hr" => {
                let line = Line::new(LineKind::Rule);
                flow.push_line(line);
                return;
            }
            "img" => {
                let mut line = Line::new(LineKind::Image);
                let alt = self.dom.attr(id, "alt").unwrap_or("image").to_string();
                line.spans.push(Span {
                    text: alt,
                    style: SpanStyle::default(),
                    link: flow.link.clone(),
                });
                flow.push_line(line);
                return;
            }
            "input" | "button" | "select" | "textarea" => {
                let mut line = Line::new(LineKind::Button);
                let label = self
                    .dom
                    .attr(id, "value")
                    .or_else(|| self.dom.attr(id, "placeholder"))
                    .unwrap_or("Button")
                    .to_string();
                line.spans.push(Span {
                    text: label,
                    style: SpanStyle::default(),
                    link: flow.link.clone(),
                });
                flow.push_line(line);
                return;
            }
            _ => {}
        }

        // Anchor targets are attached to their text runs so the window manager
        // can hit-test clicks and navigate. This only records the href; it does
        // not fetch anything (no scripts, no CSS, no images are loaded).
        let link_target = (tag == "a")
            .then(|| self.dom.attr(id, "href").unwrap_or("").to_string())
            .filter(|href| !href.is_empty());

        let block = matches!(style.display, Display::Block | Display::InlineBlock);
        if !block {
            if let Some(href) = link_target.as_deref() {
                flow.with_link(href, |f| self.layout_children(id, decor, f));
            } else {
                self.layout_children(id, decor, flow);
            }
            return;
        }

        // Finish the surrounding inline flow before starting a block box.
        flow.flush();

        let margin = style.margin;
        let padding = style.padding;
        self.spacer(margin[0]);
        let box_x = flow.width.min(padding[3] + margin[3]);
        let box_w = self
            .width
            .saturating_sub(margin[1] + margin[3] + padding[1] + padding[3]);
        let box_w = style.width.map_or(box_w, |w| w.min(box_w));
        let box_decor =
            (style.background != DEFAULT_BACKGROUND || tag == "body").then_some(BoxDecor {
                x: box_x,
                w: box_w,
                color: style.background,
            });

        let mut index_marker = None;
        match tag.as_str() {
            "ul" | "ol" => self.list_stack.push((tag == "ol", 0)),
            "li" => {
                if let Some(list) = self.list_stack.last_mut() {
                    list.1 += 1;
                    let ordered = list.0;
                    let counter = list.1;
                    let marker = match style.list_marker.as_deref() {
                        Some("none") => String::new(),
                        Some("decimal") => alloc::format!("{counter}. "),
                        Some("disc") => String::from("* "),
                        Some("square") => String::from("# "),
                        _ if ordered => alloc::format!("{counter}. "),
                        _ => String::from("* "),
                    };
                    index_marker = Some(marker);
                }
            }
            _ => {}
        }

        let line_kind = match tag.as_str() {
            "li" => LineKind::ListItem(self.list_stack.len()),
            "blockquote" => LineKind::Quote,
            "pre" => LineKind::Pre,
            _ => LineKind::Normal,
        };

        let mut child_flow = Flow::new(box_w);
        child_flow.link = flow.link.clone();
        if let Some(marker) = index_marker {
            if !marker.is_empty() {
                child_flow.push_text(&marker, &style);
                child_flow.push_span(" ", &style, style.scale());
            }
        }
        let child_decor = box_decor.or(decor);
        if let Some(href) = link_target.as_deref() {
            child_flow.with_link(href, |f| self.layout_children(id, child_decor, f));
        } else {
            self.layout_children(id, child_decor, &mut child_flow);
        }
        child_flow.flush();
        if box_decor.is_some() && child_flow.lines.is_empty() {
            child_flow.lines.push(Line::new(LineKind::Normal));
        }
        for mut line in child_flow.lines.drain(..) {
            if line.decor.is_none() {
                line.decor = child_decor;
            }
            if line_kind != LineKind::Normal && line.kind == LineKind::Normal {
                line.kind = line_kind;
            }
            if line.kind == LineKind::Pre && line.background.is_none() {
                line.background = Some(Color(0xF5_F5_F5));
            }
            line.indent += box_x;
            if style.align != Align::Left {
                line.align = style.align;
            }
            if self.page.lines.len() < 1200 {
                self.page.lines.push(line);
            }
        }

        match tag.as_str() {
            "ul" | "ol" => {
                self.list_stack.pop();
            }
            _ => {}
        }
        self.spacer(padding[2] + margin[2]);
    }
}

#[cfg(test)]
mod tests {
    use super::{Align, Color, LineKind, Page, render, title};

    fn page_text(page: &Page) -> alloc::string::String {
        let mut out = alloc::string::String::new();
        for line in &page.lines {
            for span in &line.spans {
                out.push_str(&span.text);
            }
        }
        out
    }

    fn line_with<'a>(page: &'a Page, needle: &str) -> &'a super::Line {
        page.lines
            .iter()
            .find(|line| line.spans.iter().any(|span| span.text.contains(needle)))
            .unwrap_or_else(|| panic!("no line containing {needle}"))
    }

    #[test_case]
    fn title_is_decoded_and_hidden_head_content_is_not_rendered() {
        let html = "<html><head><title>Hi &amp; bye</title>\
                    <style>p { color: red; }</style></head>\
                    <body><p>body text</p></body></html>";
        assert_eq!(title(html).as_deref(), Some("Hi & bye"));
        let page = render(html, 40);
        let text = page_text(&page);
        assert!(text.contains("body text"));
        assert!(!text.contains("Hi & bye"));
        assert!(!text.contains("color: red"));
    }

    #[test_case]
    fn anchor_hrefs_are_attached_to_their_text_runs() {
        let page = render(
            "<p><a href=\"/next\">Next page</a> and <b>bold</b>.</p>",
            40,
        );
        let spans: alloc::vec::Vec<&super::Span> = page
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .collect();
        let link = spans
            .iter()
            .find(|span| span.text.contains("Next"))
            .expect("link text was not rendered");
        assert_eq!(link.link.as_deref(), Some("/next"));
        let plain = spans
            .iter()
            .find(|span| span.text.contains("bold"))
            .expect("bold text was not rendered");
        assert_eq!(plain.link, None);
    }

    #[test_case]
    fn important_declarations_beat_inline_style() {
        let html = "<style>p { color: #123456; } .hot { color: red !important; }</style>\
                    <p class=\"hot\" style=\"color: green\">one</p><p>two</p>";
        let page = render(html, 40);
        let first = line_with(&page, "one");
        let second = line_with(&page, "two");
        assert_eq!(first.spans[0].style.color, Color(0xFF_00_00));
        assert_eq!(second.spans[0].style.color, Color(0x12_34_56));
    }

    #[test_case]
    fn descendant_selectors_set_alignment_and_weight() {
        let html = "<style>div p { text-align: center; font-weight: bold; }</style>\
                    <div><p>centered</p></div><p>plain</p>";
        let page = render(html, 40);
        let centered = line_with(&page, "centered");
        let plain = line_with(&page, "plain");
        assert_eq!(centered.align, Align::Center);
        assert!(centered.spans[0].style.bold);
        assert_eq!(plain.align, Align::Left);
        assert!(!plain.spans[0].style.bold);
    }

    #[test_case]
    fn lists_and_preformatted_blocks_get_distinct_lines() {
        let page = render("<ol><li>first</li><li>second</li></ol><pre>code</pre>", 40);
        let first = line_with(&page, "1.");
        let pre = line_with(&page, "code");
        assert!(matches!(first.kind, LineKind::ListItem(1)));
        assert!(first.spans[0].text.starts_with("1. "));
        assert_eq!(pre.kind, LineKind::Pre);
        assert!(pre.background.is_some());
    }
}
