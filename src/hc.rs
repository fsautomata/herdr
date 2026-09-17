//! `hc:` inline review comments stored in markdown files (hpp fork).
//!
//! A comment thread is an anchor (an invisible marker pair around the commented text, or a
//! table-cell / quote reference) plus one or more `hc:body` HTML comments placed on their own
//! lines right after the anchored block:
//!
//! ```markdown
//! The system MUST <!--hc:a id=c1-->retry on 5xx<!--hc:/ id=c1--> within 30s.
//! <!-- hc:body id=c1 author=elio ts=2026-09-15T10:12Z directive=reply
//!      quote="retry on 5xx"
//!      : Should this also cover 429? -->
//! ```
//!
//! Everything is HTML comments, so rendered markdown is unchanged and agents read the thread in
//! place. The file is the only store. This module is UI-independent: it parses threads,
//! re-anchors comments whose markers were removed, and produces edited source text.

use std::ops::Range;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

const OPEN_PREFIX: &str = "<!--hc:a ";
const CLOSE_PREFIX: &str = "<!--hc:/ ";
const BODY_PREFIXES: [&str; 2] = ["<!-- hc:body", "<!--hc:body"];
const COMMENT_END: &str = "-->";
/// Indentation of attribute and text continuation lines inside a body.
const BODY_INDENT: &str = "     ";
/// Characters of surrounding text stored in `ctx` on each side of the quote.
const CTX_CHARS: usize = 24;

/// What the agent should do with a comment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Directive {
    /// Answer in a reply; do not change the file.
    #[default]
    Reply,
    /// Change the file to address the comment, then remove the thread.
    Fix,
    /// Answer and propose an approach; change nothing until the human agrees.
    Discuss,
}

impl Directive {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Fix => "fix",
            Self::Discuss => "discuss",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "reply" => Some(Self::Reply),
            "fix" => Some(Self::Fix),
            "discuss" => Some(Self::Discuss),
            _ => None,
        }
    }

    pub(crate) fn next(self) -> Self {
        match self {
            Self::Reply => Self::Fix,
            Self::Fix => Self::Discuss,
            Self::Discuss => Self::Reply,
        }
    }
}

/// An `hc:a` / `hc:/` marker pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MarkerPair {
    pub(crate) id: String,
    /// Source range of the opening marker text.
    pub(crate) open: Range<usize>,
    /// Source range of the closing marker text.
    pub(crate) close: Range<usize>,
}

impl MarkerPair {
    /// The commented text between the markers.
    pub(crate) fn content(&self) -> Range<usize> {
        self.open.end..self.close.start
    }
}

/// One `hc:body` comment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Body {
    pub(crate) id: String,
    pub(crate) author: String,
    pub(crate) ts: String,
    /// `None` when the attribute is omitted (which means `reply`).
    pub(crate) directive: Option<Directive>,
    pub(crate) reply_to: Option<String>,
    pub(crate) quote: Option<String>,
    pub(crate) ctx: Option<String>,
    pub(crate) cell: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) text: String,
    /// Whole source lines occupied by the comment (including its trailing newline).
    pub(crate) range: Range<usize>,
}

impl Body {
    pub(crate) fn effective_directive(&self) -> Directive {
        self.directive.unwrap_or_default()
    }

    pub(crate) fn is_agent(&self) -> bool {
        self.author.eq_ignore_ascii_case("agent")
    }
}

/// How a thread's anchor was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Anchor {
    /// Marker pair present; range is the commented text.
    Marked(Range<usize>),
    /// Markers missing; the stored quote was found at this range.
    Quoted(Range<usize>),
    /// Table cell found by row key and column header.
    Cell(Range<usize>),
    /// Table row found but the column is gone.
    CellRow(Range<usize>),
    /// Nothing matched; the comment is kept but shown as orphaned.
    Orphan,
}

impl Anchor {
    pub(crate) fn range(&self) -> Option<Range<usize>> {
        match self {
            Self::Marked(range)
            | Self::Quoted(range)
            | Self::Cell(range)
            | Self::CellRow(range) => Some(range.clone()),
            Self::Orphan => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Thread {
    pub(crate) id: String,
    /// Indexes into [`Parsed::bodies`], in file order.
    pub(crate) bodies: Vec<usize>,
    pub(crate) anchor: Anchor,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Parsed {
    pub(crate) markers: Vec<MarkerPair>,
    pub(crate) bodies: Vec<Body>,
    pub(crate) threads: Vec<Thread>,
}

impl Parsed {
    pub(crate) fn thread(&self, id: &str) -> Option<&Thread> {
        self.threads.iter().find(|thread| thread.id == id)
    }

    /// The newest body decides whether a thread is waiting on the human.
    pub(crate) fn is_answered(&self, thread: &Thread) -> bool {
        thread
            .bodies
            .last()
            .is_some_and(|index| self.bodies[*index].is_agent())
    }

    pub(crate) fn is_resolved(&self, thread: &Thread) -> bool {
        thread.bodies.iter().any(|index| {
            self.bodies[*index]
                .status
                .as_deref()
                .is_some_and(|status| status.eq_ignore_ascii_case("resolved"))
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HcError {
    EmptySelection,
    UnknownThread(String),
    InsideComment,
}

impl std::fmt::Display for HcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptySelection => write!(f, "select some text to comment on"),
            Self::UnknownThread(id) => write!(f, "comment thread {id} is not in the file"),
            Self::InsideComment => write!(f, "the selection is inside a comment"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------------------

pub(crate) fn parse(source: &str) -> Parsed {
    let markers = parse_markers(source);
    let bodies = parse_bodies(source);
    let mut order: Vec<String> = Vec::new();
    for body in &bodies {
        if !order.contains(&body.id) {
            order.push(body.id.clone());
        }
    }
    let tables = table_models(source);
    let threads = order
        .into_iter()
        .map(|id| {
            let indexes: Vec<usize> = bodies
                .iter()
                .enumerate()
                .filter(|(_, body)| body.id == id)
                .map(|(index, _)| index)
                .collect();
            let root = &bodies[indexes[0]];
            let anchor = resolve_anchor(source, &markers, &tables, root);
            Thread {
                id,
                bodies: indexes,
                anchor,
            }
        })
        .collect();
    Parsed {
        markers,
        bodies,
        threads,
    }
}

fn parse_markers(source: &str) -> Vec<MarkerPair> {
    let opens = marker_occurrences(source, OPEN_PREFIX);
    let closes = marker_occurrences(source, CLOSE_PREFIX);
    let mut used = vec![false; closes.len()];
    let mut pairs = Vec::new();
    for (id, open) in opens {
        let close = closes
            .iter()
            .enumerate()
            .find(|(index, (close_id, close))| {
                !used[*index] && *close_id == id && close.start >= open.end
            })
            .map(|(index, (_, close))| (index, close.clone()));
        if let Some((index, close)) = close {
            used[index] = true;
            pairs.push(MarkerPair { id, open, close });
        }
    }
    pairs
}

fn marker_occurrences(source: &str, prefix: &str) -> Vec<(String, Range<usize>)> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(offset) = source[from..].find(prefix) {
        let start = from + offset;
        let Some(end_offset) = source[start..].find(COMMENT_END) else {
            break;
        };
        let end = start + end_offset + COMMENT_END.len();
        let inner = &source[start + prefix.len()..start + end_offset];
        let attrs = parse_attributes(inner).0;
        if let Some(id) = attr(&attrs, "id") {
            found.push((id.to_owned(), start..end));
        }
        from = end;
    }
    found
}

fn parse_bodies(source: &str) -> Vec<Body> {
    let mut bodies = Vec::new();
    let mut from = 0;
    loop {
        let next = BODY_PREFIXES
            .iter()
            .filter_map(|prefix| source[from..].find(prefix).map(|at| (from + at, *prefix)))
            .min_by_key(|(at, _)| *at);
        let Some((start, prefix)) = next else {
            break;
        };
        let Some(end_offset) = source[start..].find(COMMENT_END) else {
            break;
        };
        let comment_end = start + end_offset + COMMENT_END.len();
        from = comment_end;
        let inner = &source[start + prefix.len()..start + end_offset];
        let (attrs, text) = parse_attributes(inner);
        let Some(id) = attr(&attrs, "id").map(str::to_owned) else {
            continue;
        };
        bodies.push(Body {
            id,
            author: attr(&attrs, "author").unwrap_or("unknown").to_owned(),
            ts: attr(&attrs, "ts").unwrap_or_default().to_owned(),
            directive: attr(&attrs, "directive").and_then(Directive::parse),
            reply_to: attr(&attrs, "reply-to").map(str::to_owned),
            quote: attr(&attrs, "quote").map(str::to_owned),
            ctx: attr(&attrs, "ctx").map(str::to_owned),
            cell: attr(&attrs, "cell").map(str::to_owned),
            status: attr(&attrs, "status").map(str::to_owned),
            text: text
                .map(|text| unindent(&unescape_text(text)))
                .unwrap_or_default(),
            range: whole_lines(source, start..comment_end),
        });
    }
    bodies
}

/// Extend a range to whole lines when it stands alone on them.
fn whole_lines(source: &str, range: Range<usize>) -> Range<usize> {
    let line_start = source[..range.start].rfind('\n').map_or(0, |at| at + 1);
    let start = if source[line_start..range.start].trim().is_empty() {
        line_start
    } else {
        range.start
    };
    let rest = &source[range.end..];
    let trailing = rest.len() - rest.trim_start_matches([' ', '\t']).len();
    let end = if rest[trailing..].starts_with("\r\n") {
        range.end + trailing + 2
    } else if rest[trailing..].starts_with('\n') {
        range.end + trailing + 1
    } else if rest[trailing..].is_empty() {
        source.len()
    } else {
        range.end
    };
    start..end
}

fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

/// Parse `key=value key="quoted value" ... : free text`. Returns attributes and the text after
/// the first `:` that starts a token.
fn parse_attributes(inner: &str) -> (Vec<(String, String)>, Option<&str>) {
    let mut attrs = Vec::new();
    let bytes = inner.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        if bytes[index] == b':' {
            let text = &inner[index + 1..];
            return (attrs, Some(text));
        }
        let key_start = index;
        while index < bytes.len() && !bytes[index].is_ascii_whitespace() && bytes[index] != b'=' {
            index += 1;
        }
        let key = inner[key_start..index].to_owned();
        if index >= bytes.len() || bytes[index] != b'=' {
            continue;
        }
        index += 1;
        let value = if index < bytes.len() && bytes[index] == b'"' {
            index += 1;
            let mut value = String::new();
            while index < bytes.len() && bytes[index] != b'"' {
                if bytes[index] == b'\\' && index + 1 < bytes.len() {
                    let escaped = bytes[index + 1];
                    match escaped {
                        b'n' => value.push('\n'),
                        b'"' => value.push('"'),
                        b'\\' => value.push('\\'),
                        b'>' => value.push('>'),
                        _ => {
                            value.push('\\');
                            value.push(escaped as char);
                        }
                    }
                    index += 2;
                    continue;
                }
                let ch_len = utf8_len(bytes[index]);
                value.push_str(&inner[index..index + ch_len]);
                index += ch_len;
            }
            index = (index + 1).min(bytes.len());
            value
        } else {
            let value_start = index;
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            inner[value_start..index].to_owned()
        };
        attrs.push((key, value));
    }
    (attrs, None)
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn unindent(text: &str) -> String {
    let text = text.strip_prefix(' ').unwrap_or(text);
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("").to_owned();
    let rest: Vec<&str> = lines.collect();
    let indent = rest
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut out = first;
    for line in rest {
        out.push('\n');
        out.push_str(line.get(indent..).unwrap_or(line.trim_start()));
    }
    out.trim_end().to_owned()
}

fn escape_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace("-->", "--\\>")
}

fn escape_text(text: &str) -> String {
    text.replace("-->", "--\\>")
}

fn unescape_text(text: &str) -> String {
    text.replace("--\\>", "-->")
}

// ---------------------------------------------------------------------------------------------
// Anchoring
// ---------------------------------------------------------------------------------------------

fn resolve_anchor(
    source: &str,
    markers: &[MarkerPair],
    tables: &[TableModel],
    root: &Body,
) -> Anchor {
    if let Some(pair) = markers.iter().find(|pair| pair.id == root.id) {
        return Anchor::Marked(pair.content());
    }
    if let Some(cell) = root.cell.as_deref() {
        if let Some(anchor) = resolve_cell(tables, cell, root.range.start) {
            return anchor;
        }
    }
    if let Some(quote) = root
        .quote
        .as_deref()
        .filter(|quote| !quote.trim().is_empty())
    {
        if let Some(range) = find_quote(source, quote, root.ctx.as_deref(), root.range.start) {
            return Anchor::Quoted(range);
        }
    }
    Anchor::Orphan
}

/// Locate `quote` in `source` outside comments (exact, then whitespace-normalized).
pub(crate) fn locate_quote(source: &str, quote: &str) -> Option<Range<usize>> {
    if quote.trim().is_empty() {
        return None;
    }
    find_quote(source, quote, None, usize::MAX)
}

/// Find `quote` outside comments: exact first (preferring the occurrence whose context matches,
/// then the nearest one before the body), then with whitespace normalized.
fn find_quote(source: &str, quote: &str, ctx: Option<&str>, before: usize) -> Option<Range<usize>> {
    let comments = comment_ranges(source);
    let outside = |range: &Range<usize>| {
        !comments
            .iter()
            .any(|comment| comment.start < range.end && range.start < comment.end)
    };
    let mut exact: Vec<Range<usize>> = source
        .match_indices(quote)
        .map(|(at, _)| at..at + quote.len())
        .filter(outside)
        .collect();
    if exact.is_empty() {
        exact = normalized_matches(source, quote)
            .into_iter()
            .filter(outside)
            .collect();
    }
    if exact.is_empty() {
        return None;
    }
    if let Some((before_ctx, after_ctx)) = ctx.and_then(|ctx| ctx.split_once('|')) {
        if let Some(found) = exact.iter().find(|range| {
            source[..range.start]
                .trim_end()
                .ends_with(before_ctx.trim())
                && source[range.end..]
                    .trim_start()
                    .starts_with(after_ctx.trim())
        }) {
            return Some(found.clone());
        }
    }
    exact
        .iter()
        .filter(|range| range.start <= before)
        .max_by_key(|range| range.start)
        .or_else(|| exact.first())
        .cloned()
}

/// Match `quote` treating any run of whitespace as equal to any other.
fn normalized_matches(source: &str, quote: &str) -> Vec<Range<usize>> {
    let words: Vec<&str> = quote.split_whitespace().collect();
    let Some(first) = words.first() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for (start, _) in source.match_indices(first) {
        let mut at = start + first.len();
        let mut matched = true;
        for word in &words[1..] {
            let rest = &source[at..];
            let skipped = rest.len() - rest.trim_start().len();
            if skipped == 0 || !rest[skipped..].starts_with(word) {
                matched = false;
                break;
            }
            at += skipped + word.len();
        }
        if matched {
            found.push(start..at);
        }
    }
    found
}

fn comment_ranges(source: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut from = 0;
    while let Some(offset) = source[from..].find("<!--") {
        let start = from + offset;
        let end = source[start..]
            .find(COMMENT_END)
            .map_or(source.len(), |at| start + at + COMMENT_END.len());
        ranges.push(start..end);
        from = end;
    }
    ranges
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TableModel {
    pub(crate) range: Range<usize>,
    /// Header cell texts and ranges.
    pub(crate) header: Vec<(String, Range<usize>)>,
    /// Body rows: cell texts and ranges.
    pub(crate) rows: Vec<Vec<(String, Range<usize>)>>,
}

impl TableModel {
    /// `cell="<row key>|<header>"` for a body cell, with `#n` when the row key repeats.
    pub(crate) fn cell_key(&self, row: usize, column: usize) -> Option<String> {
        let key = self.rows.get(row)?.first()?.0.clone();
        let header = self.header.get(column)?.0.clone();
        let occurrence = self.rows[..=row]
            .iter()
            .filter(|candidate| candidate.first().is_some_and(|cell| cell.0 == key))
            .count();
        Some(if occurrence > 1 {
            format!("{key}#{occurrence}|{header}")
        } else {
            format!("{key}|{header}")
        })
    }
}

pub(crate) fn table_models(source: &str) -> Vec<TableModel> {
    let mut tables = Vec::new();
    let mut current: Option<TableModel> = None;
    let mut in_head = false;
    let mut cell: Option<(String, Range<usize>)> = None;
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        match event {
            Event::Start(Tag::Table(_)) => {
                current = Some(TableModel {
                    range,
                    ..TableModel::default()
                })
            }
            Event::End(TagEnd::Table) => tables.extend(current.take()),
            Event::Start(Tag::TableHead) => in_head = true,
            Event::End(TagEnd::TableHead) => in_head = false,
            Event::Start(Tag::TableRow) => {
                if let Some(table) = current.as_mut() {
                    table.rows.push(Vec::new());
                }
            }
            Event::Start(Tag::TableCell) => cell = Some((String::new(), range)),
            Event::End(TagEnd::TableCell) => {
                if let (Some(table), Some((text, range))) = (current.as_mut(), cell.take()) {
                    let entry = (text.trim().to_owned(), range);
                    if in_head {
                        table.header.push(entry);
                    } else if let Some(row) = table.rows.last_mut() {
                        row.push(entry);
                    }
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((content, _)) = cell.as_mut() {
                    content.push_str(&text);
                }
            }
            _ => {}
        }
    }
    tables
}

fn resolve_cell(tables: &[TableModel], cell: &str, before: usize) -> Option<Anchor> {
    let (row_key, header) = cell.split_once('|')?;
    let (row_key, occurrence) = match row_key.rsplit_once('#') {
        Some((key, n)) => match n.parse::<usize>() {
            Ok(n) => (key, n.max(1)),
            Err(_) => (row_key, 1),
        },
        None => (row_key, 1),
    };
    let table = tables
        .iter()
        .filter(|table| table.range.start <= before)
        .max_by_key(|table| table.range.start)?;
    let row = table
        .rows
        .iter()
        .filter(|row| row.first().is_some_and(|cell| cell.0 == row_key.trim()))
        .nth(occurrence - 1)?;
    let row_range = row.first()?.1.start..row.last()?.1.end;
    let column = table
        .header
        .iter()
        .position(|(text, _)| text == header.trim());
    Some(match column.and_then(|column| row.get(column)) {
        Some((_, range)) => Anchor::Cell(range.clone()),
        None => Anchor::CellRow(row_range),
    })
}

// ---------------------------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------------------------

/// Author, time and text of a new body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NewBody {
    pub(crate) author: String,
    pub(crate) ts: String,
    pub(crate) text: String,
}

/// Where a new thread is anchored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NewAnchor {
    /// Selected source bytes (snapped to words and inline constructs before use).
    Span(Range<usize>),
    /// A table body cell: table index in document order, body row index, column index.
    Cell {
        table: usize,
        row: usize,
        column: usize,
    },
}

/// Structure needed to place markers and bodies safely.
struct Structure {
    /// Top-level blocks in order.
    blocks: Vec<Range<usize>>,
    /// Regions where markers must not be inserted (code blocks, tables, raw HTML).
    no_marker: Vec<Range<usize>>,
    /// Inline constructs a marker must not cut (code spans, links, emphasis, comments).
    atomic: Vec<Range<usize>>,
}

fn structure(source: &str) -> Structure {
    let mut blocks = Vec::new();
    let mut no_marker = Vec::new();
    let mut atomic = Vec::new();
    let mut depth = 0usize;
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        match &event {
            Event::Start(tag) => {
                let block = is_block(tag);
                if block {
                    if depth == 0 {
                        blocks.push(range.clone());
                    }
                    depth += 1;
                }
                match tag {
                    Tag::CodeBlock(_) | Tag::Table(_) | Tag::HtmlBlock => {
                        no_marker.push(range.clone())
                    }
                    Tag::Link { .. }
                    | Tag::Image { .. }
                    | Tag::Emphasis
                    | Tag::Strong
                    | Tag::Strikethrough => atomic.push(range.clone()),
                    _ => {}
                }
            }
            Event::End(tag) => {
                if is_block_end(tag) {
                    depth = depth.saturating_sub(1);
                }
            }
            Event::Code(_) | Event::InlineHtml(_) => atomic.push(range.clone()),
            Event::Html(_) | Event::Rule if depth == 0 => {
                blocks.push(range.clone());
                no_marker.push(range.clone());
            }
            _ => {}
        }
    }
    for comment in comment_ranges(source) {
        atomic.push(comment);
    }
    Structure {
        blocks,
        no_marker,
        atomic,
    }
}

fn is_block(tag: &Tag<'_>) -> bool {
    matches!(
        tag,
        Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::BlockQuote(_)
            | Tag::CodeBlock(_)
            | Tag::HtmlBlock
            | Tag::List(_)
            | Tag::Item
            | Tag::FootnoteDefinition(_)
            | Tag::Table(_)
            | Tag::DefinitionList
            | Tag::MetadataBlock(_)
    )
}

fn is_block_end(tag: &TagEnd) -> bool {
    matches!(
        tag,
        TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::CodeBlock
            | TagEnd::HtmlBlock
            | TagEnd::List(_)
            | TagEnd::Item
            | TagEnd::FootnoteDefinition
            | TagEnd::Table
            | TagEnd::DefinitionList
            | TagEnd::MetadataBlock(_)
    )
}

/// Next free thread id (`c<n>`).
pub(crate) fn next_id(parsed: &Parsed) -> String {
    let highest = parsed
        .bodies
        .iter()
        .map(|body| body.id.as_str())
        .chain(parsed.markers.iter().map(|pair| pair.id.as_str()))
        .filter_map(|id| id.strip_prefix('c')?.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    format!("c{}", highest + 1)
}

/// Snap a selection outward to whole words and to any inline construct or comment it cuts.
pub(crate) fn snap_selection(source: &str, selection: Range<usize>) -> Range<usize> {
    let mut start = selection.start.min(source.len());
    let mut end = selection.end.clamp(start, source.len());
    while !source.is_char_boundary(start) {
        start -= 1;
    }
    while !source.is_char_boundary(end) {
        end += 1;
    }
    // Trim surrounding whitespace.
    while start < end && source[start..].starts_with(char::is_whitespace) {
        start += source[start..].chars().next().map_or(1, char::len_utf8);
    }
    while end > start && source[..end].ends_with(char::is_whitespace) {
        end -= source[..end].chars().next_back().map_or(1, char::len_utf8);
    }
    if start == end {
        return start..end;
    }
    let is_word = |ch: char| ch.is_alphanumeric() || ch == '_' || ch == '\'' || ch == '-';
    while let Some(ch) = source[..start]
        .chars()
        .next_back()
        .filter(|ch| is_word(*ch))
    {
        start -= ch.len_utf8();
    }
    while let Some(ch) = source[end..].chars().next().filter(|ch| is_word(*ch)) {
        end += ch.len_utf8();
    }
    let atomic = structure(source).atomic;
    loop {
        let mut changed = false;
        for range in &atomic {
            let cuts_start = range.start < start && start < range.end;
            let cuts_end = range.start < end && end < range.end;
            if cuts_start || cuts_end {
                start = start.min(range.start);
                end = end.max(range.end);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    start..end
}

fn format_body(
    id: &str,
    meta: &NewBody,
    attrs: &[(&str, String)],
    quoted: &[(&str, String)],
) -> String {
    let mut header = format!(
        "<!-- hc:body id={id} author={} ts={}",
        bare(&meta.author),
        bare(&meta.ts)
    );
    for (key, value) in attrs {
        header.push_str(&format!(" {key}={}", bare(value)));
    }
    let mut out = header;
    for (key, value) in quoted {
        out.push('\n');
        out.push_str(BODY_INDENT);
        out.push_str(&format!("{key}=\"{}\"", escape_value(value)));
    }
    out.push('\n');
    out.push_str(BODY_INDENT);
    out.push_str(": ");
    let text = escape_text(meta.text.trim());
    let continuation = format!("\n{BODY_INDENT}  ");
    out.push_str(&text.replace('\n', &continuation));
    out.push_str(" -->\n");
    out
}

/// Attribute values without spaces are written bare.
fn bare(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_whitespace() || ch == '"' {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>()
        .replace("-->", "--_")
}

/// Context around a quote, markers and newlines flattened: `before|after`.
fn context(source: &str, range: &Range<usize>) -> String {
    let before: String = strip_markers(&source[..range.start])
        .chars()
        .rev()
        .take(CTX_CHARS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let after: String = strip_markers(&source[range.end..])
        .chars()
        .take(CTX_CHARS)
        .collect();
    let flat = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(
        "{}|{}",
        flat(&before).replace('|', "/"),
        flat(&after).replace('|', "/")
    )
}

/// Text with all `hc:` markers and bodies removed (bodies with the lines they occupy).
pub(crate) fn strip_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    while let Some(offset) = text[from..].find("<!--") {
        let start = from + offset;
        let is_marker =
            text[start..].starts_with(OPEN_PREFIX) || text[start..].starts_with(CLOSE_PREFIX);
        let is_body = BODY_PREFIXES
            .iter()
            .any(|prefix| text[start..].starts_with(prefix));
        let end = text[start..]
            .find(COMMENT_END)
            .map_or(text.len(), |at| start + at + COMMENT_END.len());
        let cut = if is_body {
            whole_lines(text, start..end)
        } else {
            start..end
        };
        // `whole_lines` may start before `start` (leading indentation already copied).
        let keep_until = cut.start.max(from);
        out.push_str(&text[from..keep_until]);
        if !is_marker && !is_body {
            out.push_str(&text[start..end]);
        }
        from = if is_body { cut.end } else { end };
    }
    out.push_str(&text[from..]);
    out
}

/// Insertion point for bodies of a thread anchored at `position`: after the top-level block that
/// contains it, and after any comment bodies already following that block.
fn body_insertion_point(
    source: &str,
    blocks: &[Range<usize>],
    bodies: &[Body],
    position: usize,
) -> usize {
    let block_end = blocks
        .iter()
        .find(|block| block.start <= position && position < block.end.max(block.start + 1))
        .map_or_else(
            || {
                source[position..]
                    .find('\n')
                    .map_or(source.len(), |at| position + at + 1)
            },
            |block| block.end,
        );
    // Blocks end after their last line; make sure we insert at a line start.
    let mut at = block_end.min(source.len());
    if at > 0 && !source[..at].ends_with('\n') {
        at = source[at..]
            .find('\n')
            .map_or(source.len(), |offset| at + offset + 1);
    }
    loop {
        let rest = &source[at..];
        match bodies.iter().find(|body| body.range.start == at) {
            Some(body) => at = body.range.end,
            None => {
                let blank = rest.len() - rest.trim_start_matches([' ', '\t']).len();
                if let Some(body) = bodies.iter().find(|body| body.range.start == at + blank) {
                    at = body.range.end;
                } else {
                    break;
                }
            }
        }
    }
    at
}

/// Insert `text` at byte `at`, adding a newline first when `at` is not at a line start.
fn insert_lines(source: &str, at: usize, text: &str) -> String {
    let mut out = String::with_capacity(source.len() + text.len() + 1);
    out.push_str(&source[..at]);
    if at > 0 && !source[..at].ends_with('\n') {
        out.push('\n');
    }
    out.push_str(text);
    out.push_str(&source[at..]);
    out
}

/// Add a new comment thread. Returns the new source and the thread id.
pub(crate) fn add_comment(
    source: &str,
    anchor: NewAnchor,
    directive: Directive,
    meta: &NewBody,
) -> Result<(String, String), HcError> {
    let parsed = parse(source);
    let id = next_id(&parsed);
    let structure = structure(source);
    let directive_attr = ("directive", directive.as_str().to_owned());
    match anchor {
        NewAnchor::Cell { table, row, column } => {
            let tables = table_models(source);
            let model = tables.get(table).ok_or(HcError::EmptySelection)?;
            let key = model.cell_key(row, column).ok_or(HcError::EmptySelection)?;
            let at =
                body_insertion_point(source, &structure.blocks, &parsed.bodies, model.range.start);
            let body = format_body(&id, meta, &[directive_attr], &[("cell", key)]);
            Ok((insert_lines(source, at, &body), id))
        }
        NewAnchor::Span(selection) => {
            let range = snap_selection(source, selection);
            if range.is_empty() {
                return Err(HcError::EmptySelection);
            }
            if comment_ranges(source)
                .iter()
                .any(|comment| comment.start <= range.start && range.end <= comment.end)
            {
                return Err(HcError::InsideComment);
            }
            let quote = strip_markers(&source[range.clone()]);
            let ctx = context(source, &range);
            let at = body_insertion_point(source, &structure.blocks, &parsed.bodies, range.start);
            let body = format_body(
                &id,
                meta,
                &[directive_attr],
                &[("quote", quote), ("ctx", ctx)],
            );
            let markable = !structure
                .no_marker
                .iter()
                .any(|region| region.start < range.end && range.start < region.end);
            // Apply edits back to front so earlier offsets stay valid.
            let mut out = insert_lines(source, at, &body);
            if markable {
                let open = format!("{OPEN_PREFIX}id={id}{COMMENT_END}");
                let close = format!("{CLOSE_PREFIX}id={id}{COMMENT_END}");
                debug_assert!(range.end <= at);
                out.insert_str(range.end, &close);
                out.insert_str(range.start, &open);
            }
            Ok((out, id))
        }
    }
}

/// Append a reply to a thread, right after its last body.
pub(crate) fn append_reply(
    source: &str,
    id: &str,
    meta: &NewBody,
    status: Option<&str>,
) -> Result<String, HcError> {
    let parsed = parse(source);
    let thread = parsed
        .thread(id)
        .ok_or_else(|| HcError::UnknownThread(id.to_owned()))?;
    let last = &parsed.bodies[*thread.bodies.last().expect("threads have bodies")];
    let mut attrs = vec![("reply-to", id.to_owned())];
    if let Some(status) = status {
        attrs.push(("status", status.to_owned()));
    }
    let body = format_body(id, meta, &attrs, &[]);
    Ok(insert_lines(source, last.range.end, &body))
}

/// Remove a thread: its markers and every body.
pub(crate) fn remove_thread(source: &str, id: &str) -> Result<String, HcError> {
    let parsed = parse(source);
    let thread = parsed
        .thread(id)
        .ok_or_else(|| HcError::UnknownThread(id.to_owned()))?;
    let mut cuts: Vec<Range<usize>> = thread
        .bodies
        .iter()
        .map(|index| parsed.bodies[*index].range.clone())
        .collect();
    for pair in parsed.markers.iter().filter(|pair| pair.id == id) {
        cuts.push(pair.open.clone());
        cuts.push(pair.close.clone());
    }
    cuts.sort_by_key(|range| std::cmp::Reverse(range.start));
    let mut out = source.to_owned();
    for cut in cuts {
        out.replace_range(cut, "");
    }
    Ok(out)
}

/// Wrap `range` (snapped like a new selection) in marker pairs for `id`, first removing any
/// markers the thread already has. Used to re-attach an orphaned or moved thread.
pub(crate) fn reattach(source: &str, id: &str, selection: Range<usize>) -> Result<String, HcError> {
    let parsed = parse(source);
    if parsed.thread(id).is_none() {
        return Err(HcError::UnknownThread(id.to_owned()));
    }
    let mut without = source.to_owned();
    let mut cuts: Vec<Range<usize>> = parsed
        .markers
        .iter()
        .filter(|pair| pair.id == id)
        .flat_map(|pair| [pair.open.clone(), pair.close.clone()])
        .collect();
    cuts.sort_by_key(|range| std::cmp::Reverse(range.start));
    let mut selection = selection;
    for cut in &cuts {
        let len = cut.end - cut.start;
        if cut.end <= selection.start {
            selection.start -= len;
            selection.end -= len;
        } else if cut.start < selection.end {
            selection.end = selection.end.saturating_sub(len).max(selection.start);
        }
        without.replace_range(cut.clone(), "");
    }
    let range = snap_selection(&without, selection);
    if range.is_empty() {
        return Err(HcError::EmptySelection);
    }
    let structure = structure(&without);
    if structure
        .no_marker
        .iter()
        .any(|region| region.start < range.end && range.start < region.end)
    {
        // Tables and code blocks cannot hold markers; the quote anchor is all we can keep.
        return Ok(without);
    }
    let mut out = without;
    out.insert_str(range.end, &format!("{CLOSE_PREFIX}id={id}{COMMENT_END}"));
    out.insert_str(range.start, &format!("{OPEN_PREFIX}id={id}{COMMENT_END}"));
    Ok(out)
}

/// Put markers back around threads that were re-anchored by their quote, where markers are
/// allowed. Applied whenever the viewer saves, so anchors heal after an agent rewrote text.
pub(crate) fn restore_markers(source: &str) -> String {
    let parsed = parse(source);
    let structure = structure(source);
    let mut inserts: Vec<(usize, String)> = Vec::new();
    for thread in &parsed.threads {
        let Anchor::Quoted(range) = &thread.anchor else {
            continue;
        };
        let blocked = structure
            .no_marker
            .iter()
            .chain(structure.atomic.iter())
            .any(|region| region.start < range.end && range.start < region.end);
        if blocked {
            continue;
        }
        inserts.push((
            range.end,
            format!("{CLOSE_PREFIX}id={}{COMMENT_END}", thread.id),
        ));
        inserts.push((
            range.start,
            format!("{OPEN_PREFIX}id={}{COMMENT_END}", thread.id),
        ));
    }
    // Back to front, closing markers before opening markers at the same offset.
    inserts.sort_by_key(|(at, marker)| (std::cmp::Reverse(*at), marker.starts_with(CLOSE_PREFIX)));
    let mut out = source.to_owned();
    for (at, marker) in inserts {
        out.insert_str(at, &marker);
    }
    out
}

/// Current UTC time in the `YYYY-MM-DDTHH:MMZ` form used by `ts`.
pub(crate) fn now_ts() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(text: &str) -> NewBody {
        NewBody {
            author: "elio".into(),
            ts: "2026-09-17T10:12Z".into(),
            text: text.into(),
        }
    }

    fn span_of(source: &str, needle: &str) -> Range<usize> {
        let at = source.find(needle).expect("needle present");
        at..at + needle.len()
    }

    #[test]
    fn parses_the_spec_example() {
        let source = "The system MUST <!--hc:a id=c1-->retry on 5xx<!--hc:/ id=c1--> within 30s.\n\n<!-- hc:body id=c1 author=elio ts=2026-09-15T10:12Z\n     quote=\"retry on 5xx\"\n     : Should this also cover 429? And is 30s per-attempt or total? -->\n<!-- hc:body id=c1 author=agent ts=2026-09-15T10:20Z reply-to=c1\n     : 429 is covered by the rate-limit path; 30s is total. Updated the line. -->\n";
        let parsed = parse(source);
        assert_eq!(parsed.markers.len(), 1);
        assert_eq!(&source[parsed.markers[0].content()], "retry on 5xx");
        assert_eq!(parsed.bodies.len(), 2);
        let root = &parsed.bodies[0];
        assert_eq!(root.author, "elio");
        assert_eq!(root.ts, "2026-09-15T10:12Z");
        assert_eq!(root.directive, None);
        assert_eq!(root.effective_directive(), Directive::Reply);
        assert_eq!(root.quote.as_deref(), Some("retry on 5xx"));
        assert_eq!(
            root.text,
            "Should this also cover 429? And is 30s per-attempt or total?"
        );
        let reply = &parsed.bodies[1];
        assert!(reply.is_agent());
        assert_eq!(reply.reply_to.as_deref(), Some("c1"));
        assert_eq!(parsed.threads.len(), 1);
        assert!(matches!(parsed.threads[0].anchor, Anchor::Marked(_)));
        assert!(parsed.is_answered(&parsed.threads[0]));
        // Body ranges cover whole lines.
        assert!(source[root.range.clone()].starts_with("<!-- hc:body"));
        assert!(source[root.range.clone()].ends_with("-->\n"));
    }

    #[test]
    fn insert_then_remove_is_byte_identical() {
        let sources = [
            "# Plan\n\nThe server owns PTYs and pane state.\n\nNext paragraph.\n",
            "no trailing newline at all",
            "- item one\n- item two with words\n\nafter\n",
            "Line with `inline code` and **bold text** here.\n",
            "crlf line one\r\ncrlf line two\r\n",
            "Unicode ✓ café naïve text.\n",
        ];
        for source in sources {
            let target = source
                .find(|ch: char| ch.is_alphabetic())
                .map(|at| at..(at + 3).min(source.len()))
                .unwrap();
            let (with_comment, id) = add_comment(
                source,
                NewAnchor::Span(target),
                Directive::Fix,
                &meta("why?"),
            )
            .unwrap();
            assert_ne!(with_comment, source);
            let parsed = parse(&with_comment);
            assert_eq!(parsed.threads.len(), 1, "{with_comment}");
            let removed = remove_thread(&with_comment, &id).unwrap();
            // A comment appended at EOF adds a newline that removal cannot know about.
            if source.ends_with('\n') {
                assert_eq!(removed, source, "round trip for {source:?}");
            } else {
                assert_eq!(removed.trim_end(), source, "round trip for {source:?}");
            }
        }
    }

    #[test]
    fn new_comment_is_inserted_after_the_block_with_markers_and_attributes() {
        let source =
            "The server owns PTYs and pane state.\nSecond line of the paragraph.\n\nNext.\n";
        let (out, id) = add_comment(
            source,
            NewAnchor::Span(span_of(source, "PT")),
            Directive::Reply,
            &meta("which auth does the socket API use?"),
        )
        .unwrap();
        assert_eq!(id, "c1");
        assert!(out.starts_with("The server owns <!--hc:a id=c1-->PTYs<!--hc:/ id=c1--> and pane state.\nSecond line of the paragraph.\n<!-- hc:body id=c1 author=elio ts=2026-09-17T10:12Z directive=reply\n     quote=\"PTYs\"\n"), "{out}");
        assert!(
            out.contains("     : which auth does the socket API use? -->\n\nNext.\n"),
            "{out}"
        );
        let parsed = parse(&out);
        assert_eq!(parsed.bodies[0].quote.as_deref(), Some("PTYs"));
        assert_eq!(
            parsed.bodies[0].ctx.as_deref(),
            Some("The server owns|and pane state. Second")
        );
    }

    #[test]
    fn markers_never_split_inline_code_links_or_emphasis() {
        let source = "Use `cargo test --locked` or see [the docs](https://x.y) for **very important** notes.\n";
        for needle in ["test", "docs", "important"] {
            let (out, _) = add_comment(
                source,
                NewAnchor::Span(span_of(source, needle)),
                Directive::Reply,
                &meta("?"),
            )
            .unwrap();
            let parsed = parse(&out);
            let content = &out[parsed.markers[0].content()];
            assert!(
                matches!(
                    content,
                    "`cargo test --locked`" | "[the docs](https://x.y)" | "**very important**"
                ),
                "{needle}: {content}"
            );
            // Rendering is unchanged apart from the hidden comments.
            assert_eq!(strip_markers(&out), source, "{out}");
        }
    }

    #[test]
    fn code_block_selections_anchor_by_quote_without_markers() {
        let source = "Intro.\n\n```\nlet retries = 3;\n```\n\nOutro.\n";
        let (out, id) = add_comment(
            source,
            NewAnchor::Span(span_of(source, "retries")),
            Directive::Discuss,
            &meta("why three?"),
        )
        .unwrap();
        let parsed = parse(&out);
        assert!(parsed.markers.is_empty(), "{out}");
        let thread = parsed.thread(&id).unwrap();
        let Anchor::Quoted(range) = &thread.anchor else {
            panic!("expected quote anchor, got {:?}", thread.anchor);
        };
        assert_eq!(&out[range.clone()], "retries");
        // The body sits after the fenced block, never inside it.
        let fence_close = out.find("= 3;\n```").unwrap() + "= 3;\n```".len();
        assert!(parsed.bodies[0].range.start > fence_close, "{out}");
        assert_eq!(parsed.bodies[0].effective_directive(), Directive::Discuss);
    }

    #[test]
    fn table_cells_anchor_by_row_key_and_header() {
        let source = "| Container | SSH Port | Purpose |\n|-----------|----------|---------|\n| claude-dev | 2222 | Coding projects |\n| claude-general | 2223 | Everything |\n\nAfter.\n";
        let (out, id) = add_comment(
            source,
            NewAnchor::Cell {
                table: 0,
                row: 1,
                column: 1,
            },
            Directive::Fix,
            &meta("document why this isn't 2224"),
        )
        .unwrap();
        assert!(out.contains("cell=\"claude-general|SSH Port\""), "{out}");
        assert!(!out.contains("hc:a"), "no markers inside tables: {out}");
        // Body goes after the whole table.
        let table_end = out.find("| Everything |\n").unwrap() + "| Everything |\n".len();
        let parsed = parse(&out);
        assert_eq!(parsed.bodies[0].range.start, table_end, "{out}");
        let Anchor::Cell(range) = &parsed.thread(&id).unwrap().anchor else {
            panic!("expected a cell anchor");
        };
        assert!(out[range.clone()].contains("2223"));

        // Reordering rows keeps the anchor on the same cell.
        let reordered = out.replace(
            "| claude-dev | 2222 | Coding projects |\n| claude-general | 2223 | Everything |\n",
            "| claude-general | 2223 | Everything |\n| claude-dev | 2222 | Coding projects |\n",
        );
        let parsed = parse(&reordered);
        let Anchor::Cell(range) = &parsed.threads[0].anchor else {
            panic!("expected a cell anchor after reorder");
        };
        assert!(reordered[range.clone()].contains("2223"));

        // Removing the column degrades to the row.
        let no_port = "| Container | Purpose |\n|---|---|\n| claude-dev | Coding |\n| claude-general | Everything |\n<!-- hc:body id=c1 author=elio ts=x cell=\"claude-general|SSH Port\"\n     : gone -->\n";
        assert!(matches!(
            parse(no_port).threads[0].anchor,
            Anchor::CellRow(_)
        ));
    }

    #[test]
    fn duplicate_row_keys_use_occurrence_numbers() {
        let source = "| Status | Task |\n|---|---|\n| done | a |\n| done | b |\n";
        let tables = table_models(source);
        assert_eq!(tables[0].cell_key(0, 1).as_deref(), Some("done|Task"));
        assert_eq!(tables[0].cell_key(1, 1).as_deref(), Some("done#2|Task"));
        let (out, id) = add_comment(
            source,
            NewAnchor::Cell {
                table: 0,
                row: 1,
                column: 1,
            },
            Directive::Reply,
            &meta("b?"),
        )
        .unwrap();
        let parsed = parse(&out);
        let Anchor::Cell(range) = &parsed.thread(&id).unwrap().anchor else {
            panic!("cell anchor");
        };
        assert_eq!(out[range.clone()].trim(), "b");
    }

    #[test]
    fn removed_markers_reanchor_by_quote_then_orphan() {
        let source = "Retry on 5xx within 30s.\n\n<!-- hc:body id=c1 author=elio ts=x quote=\"on 5xx\" ctx=\"Retry|within 30s.\"\n     : 429? -->\n";
        let parsed = parse(source);
        let Anchor::Quoted(range) = &parsed.threads[0].anchor else {
            panic!("expected quote anchor");
        };
        assert_eq!(&source[range.clone()], "on 5xx");

        let reflowed = source.replace("Retry on 5xx", "Retry on\n5xx");
        assert!(matches!(
            parse(&reflowed).threads[0].anchor,
            Anchor::Quoted(_)
        ));

        let rewritten = source.replace("Retry on 5xx within 30s.", "Totally different text.");
        let parsed = parse(&rewritten);
        assert_eq!(parsed.threads[0].anchor, Anchor::Orphan);
        assert_eq!(parsed.bodies[0].text, "429?");
    }

    #[test]
    fn replies_append_after_the_thread_and_keep_order() {
        let source = "Text here.\n";
        let (out, id) = add_comment(
            source,
            NewAnchor::Span(span_of(source, "Text")),
            Directive::Reply,
            &meta("first"),
        )
        .unwrap();
        let reply = NewBody {
            author: "agent".into(),
            ts: "2026-09-17T10:20Z".into(),
            text: "an answer\nover two lines --> tricky".into(),
        };
        let out = append_reply(&out, &id, &reply, Some("resolved")).unwrap();
        let (out, second) = add_comment(
            &out,
            NewAnchor::Span(span_of(&out, "here")),
            Directive::Fix,
            &meta("second thread"),
        )
        .unwrap();
        assert_eq!(second, "c2");
        let parsed = parse(&out);
        assert_eq!(parsed.threads.len(), 2);
        let first = parsed.thread(&id).unwrap();
        assert_eq!(first.bodies.len(), 2);
        let answer = &parsed.bodies[first.bodies[1]];
        assert_eq!(answer.text, "an answer\nover two lines --> tricky");
        assert!(parsed.is_answered(first) && parsed.is_resolved(first));
        // The second thread's body comes after the first thread's bodies.
        let c2 = parsed.thread("c2").unwrap();
        assert!(parsed.bodies[c2.bodies[0]].range.start >= answer.range.end);
        assert!(!out.replace("--\\>", "").contains("tricky -->\n -->"));
    }

    #[test]
    fn attribute_parsing_handles_quotes_escapes_and_inline_text() {
        let (attrs, text) = parse_attributes(
            " id=c3 author=agent ts=2026-09-15T10:20Z quote=\"say \\\"hi\\\" \\\\ now\" : one-line reply ",
        );
        assert_eq!(attr(&attrs, "ts"), Some("2026-09-15T10:20Z"));
        assert_eq!(attr(&attrs, "quote"), Some("say \"hi\" \\ now"));
        assert_eq!(text, Some(" one-line reply "));
        let source = "Word.\n<!-- hc:body id=c9 author=agent ts=t reply-to=c9 : inline form -->\n";
        let parsed = parse(source);
        assert_eq!(parsed.bodies[0].text, "inline form");
    }

    #[test]
    fn selections_snap_to_words_and_reject_empty() {
        let source = "alpha beta gamma";
        assert_eq!(&source[snap_selection(source, 7..8)], "beta");
        assert_eq!(&source[snap_selection(source, 5..7)], "beta");
        assert!(snap_selection(source, 5..6).is_empty());
        assert_eq!(
            add_comment(source, NewAnchor::Span(5..6), Directive::Reply, &meta("x")),
            Err(HcError::EmptySelection)
        );
    }

    #[test]
    fn reattach_moves_markers_onto_the_new_selection() {
        let source =
            "Alpha beta gamma.\n<!-- hc:body id=c1 author=elio ts=t quote=\"gone\" : note -->\n";
        assert_eq!(parse(source).threads[0].anchor, Anchor::Orphan);
        let at = source.find("beta").unwrap();
        let out = reattach(source, "c1", at..at + 4).unwrap();
        assert!(
            out.starts_with("Alpha <!--hc:a id=c1-->beta<!--hc:/ id=c1--> gamma."),
            "{out}"
        );
        assert!(matches!(parse(&out).threads[0].anchor, Anchor::Marked(_)));

        // Re-attaching again replaces the old pair instead of nesting a second one.
        let at = out.find("gamma").unwrap();
        let again = reattach(&out, "c1", at..at + 5).unwrap();
        assert_eq!(parse(&again).markers.len(), 1, "{again}");
        assert!(
            again.contains("beta <!--hc:a id=c1-->gamma<!--hc:/ id=c1-->."),
            "{again}"
        );
    }

    #[test]
    fn quoted_anchors_regain_markers_on_save() {
        let source = "Retry on 5xx within 30s.\n<!-- hc:body id=c1 author=elio ts=t quote=\"on 5xx\" : 429? -->\n";
        let restored = restore_markers(source);
        assert!(
            restored.starts_with("Retry <!--hc:a id=c1-->on 5xx<!--hc:/ id=c1--> within"),
            "{restored}"
        );
        assert!(matches!(
            parse(&restored).threads[0].anchor,
            Anchor::Marked(_)
        ));
        // Already-marked threads are untouched.
        assert_eq!(restore_markers(&restored), restored);
    }

    #[test]
    fn next_id_skips_existing_ids() {
        let source =
            "a <!--hc:a id=c7-->b<!--hc:/ id=c7-->\n<!-- hc:body id=c7 author=elio ts=t : x -->\n";
        assert_eq!(next_id(&parse(source)), "c8");
    }

    #[test]
    fn timestamps_use_minute_precision_utc() {
        let ts = now_ts();
        assert_eq!(ts.len(), "2026-09-17T10:12Z".len(), "{ts}");
        assert!(ts.ends_with('Z'));
    }
}
