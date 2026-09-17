//! Markdown → source-mapped [`Doc`] for the hpp file viewer.
//!
//! Built on `pulldown-cmark`'s offset iterator so every rendered span keeps the byte range it
//! came from. HTML comments are not rendered, which hides the `hc:` comment markers while
//! keeping their source offsets intact.

use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use unicode_width::UnicodeWidthStr;

use super::document::{Doc, DocLine, DocSpan, DocStyle, LineKind, WrapMode};

/// Whether a path looks like a markdown file.
pub(crate) fn is_markdown_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".md", ".markdown", ".mdown", ".mkd", ".mdx"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

pub(crate) fn parse(source: &str) -> Doc {
    let mut builder = Builder::new(source);
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        builder.event(event, range);
    }
    builder.finish()
}

struct ListLevel {
    /// Next number for ordered lists.
    number: Option<u64>,
    /// Display width of the current item's marker (continuation indent).
    marker_width: usize,
}

#[derive(Default)]
struct TableState {
    index: u32,
    alignments: Vec<Alignment>,
    rows: Vec<Vec<TableCell>>,
    header_rows: usize,
    in_head: bool,
    src: Range<usize>,
}

#[derive(Default)]
struct TableCell {
    spans: Vec<DocSpan>,
    src: Range<usize>,
}

struct Builder<'a> {
    source: &'a str,
    lines: Vec<DocLine>,
    current: Option<DocLine>,
    style: DocStyle,
    quote_depth: usize,
    lists: Vec<ListLevel>,
    /// Marker for the first line started in the current list item.
    item_marker: Option<String>,
    pending_blank: bool,
    in_code_block: bool,
    table: Option<TableState>,
    tables_seen: u32,
    in_html_comment: bool,
}

impl<'a> Builder<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            lines: Vec::new(),
            current: None,
            style: DocStyle::default(),
            quote_depth: 0,
            lists: Vec::new(),
            item_marker: None,
            pending_blank: false,
            in_code_block: false,
            table: None,
            tables_seen: 0,
            in_html_comment: false,
        }
    }

    fn finish(mut self) -> Doc {
        self.close_line();
        while self
            .lines
            .last()
            .is_some_and(|line| line.kind == LineKind::Blank)
        {
            self.lines.pop();
        }
        Doc { lines: self.lines }
    }

    fn marker_style() -> DocStyle {
        DocStyle {
            marker: true,
            ..DocStyle::default()
        }
    }

    /// Prefix and continuation spans for a new line at the current nesting.
    fn prefixes(&mut self, consume_marker: bool) -> (Vec<DocSpan>, Vec<DocSpan>) {
        let mut prefix = String::new();
        let mut continuation = String::new();
        for _ in 0..self.quote_depth {
            prefix.push_str("│ ");
            continuation.push_str("│ ");
        }
        let depth = self.lists.len();
        for (level, list) in self.lists.iter().enumerate() {
            let is_innermost = level + 1 == depth;
            if is_innermost && consume_marker {
                if let Some(marker) = self.item_marker.take() {
                    prefix.push_str(&marker);
                    continuation.push_str(&" ".repeat(marker.width()));
                    continue;
                }
            }
            let indent = " ".repeat(list.marker_width);
            prefix.push_str(&indent);
            continuation.push_str(&indent);
        }
        let style = Self::marker_style();
        let wrap = |text: String| {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![DocSpan::decoration(text, style)]
            }
        };
        (wrap(prefix), wrap(continuation))
    }

    fn open_line(&mut self, kind: LineKind, start: usize) {
        self.close_line();
        if self.pending_blank && !self.lines.is_empty() {
            let mut blank = DocLine::new(LineKind::Blank, start..start);
            let (prefix, continuation) = self.prefixes(false);
            if self.quote_depth > 0 {
                blank.prefix = prefix;
                blank.continuation = continuation;
            }
            self.lines.push(blank);
        }
        self.pending_blank = false;
        let (prefix, continuation) = self.prefixes(true);
        let mut line = DocLine::new(kind, start..start);
        line.prefix = prefix;
        line.continuation = continuation;
        if self.quote_depth > 0 && kind == LineKind::Text {
            line.kind = LineKind::Quote;
        }
        self.current = Some(line);
    }

    fn ensure_line(&mut self, start: usize) {
        if self.current.is_none() {
            self.open_line(LineKind::Text, start);
        }
    }

    fn close_line(&mut self) {
        if let Some(line) = self.current.take() {
            self.lines.push(line);
        }
    }

    fn push_span(&mut self, span: DocSpan, range: &Range<usize>) {
        if let Some(table) = self.table.as_mut() {
            if let Some(cell) = table.rows.last_mut().and_then(|row| row.last_mut()) {
                cell.spans.push(span);
                return;
            }
        }
        self.ensure_line(range.start);
        if let Some(line) = self.current.as_mut() {
            line.src.start = line.src.start.min(range.start);
            line.src.end = line.src.end.max(range.end);
            line.spans.push(span);
        }
    }

    /// A span for `text` whose event covered `range`: source-backed when the text is found
    /// verbatim inside the range, owned otherwise (escapes, entities).
    fn text_span(&self, text: &str, range: &Range<usize>, style: DocStyle) -> DocSpan {
        let slice = self.source.get(range.clone()).unwrap_or("");
        if let Some(offset) = slice.find(text) {
            let start = range.start + offset;
            return DocSpan::source(start..start + text.len(), style);
        }
        DocSpan::owned(text.to_owned(), style, Some(range.clone()))
    }

    fn event(&mut self, event: Event<'_>, range: Range<usize>) {
        match event {
            Event::Start(tag) => self.start(tag, range),
            Event::End(tag) => self.end(tag, range),
            Event::Text(text) => {
                if self.in_code_block {
                    self.code_text(&text, &range);
                } else {
                    let span = self.text_span(&text, &range, self.style);
                    self.push_span(span, &range);
                }
            }
            Event::Code(text) => {
                let style = DocStyle {
                    inline_code: true,
                    ..self.style
                };
                let span = self.text_span(&text, &range, style);
                self.push_span(span, &range);
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                let span = self.text_span(&text, &range, self.style);
                self.push_span(span, &range);
            }
            Event::Html(text) => self.html_block(&text, &range),
            Event::InlineHtml(text) => {
                if is_html_comment(&text) || self.in_html_comment {
                    self.track_comment(&text);
                    return;
                }
                let style = DocStyle {
                    dim: true,
                    ..self.style
                };
                let span = self.text_span(&text, &range, style);
                self.push_span(span, &range);
            }
            Event::FootnoteReference(name) => {
                let style = DocStyle {
                    dim: true,
                    ..self.style
                };
                self.push_span(
                    DocSpan::owned(format!("[^{name}]"), style, Some(range.clone())),
                    &range,
                );
            }
            Event::SoftBreak => {
                self.push_span(DocSpan::owned(" ", self.style, Some(range.clone())), &range);
            }
            Event::HardBreak => {
                let kind = self
                    .current
                    .as_ref()
                    .map_or(LineKind::Text, |line| line.kind);
                self.close_line();
                let (_, continuation) = self.prefixes(false);
                let mut line = DocLine::new(kind, range.end..range.end);
                line.prefix = continuation.clone();
                line.continuation = continuation;
                self.current = Some(line);
            }
            Event::Rule => {
                self.open_line(LineKind::Rule, range.start);
                if let Some(line) = self.current.as_mut() {
                    line.src = range;
                }
                self.close_line();
                self.pending_blank = true;
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "☑ " } else { "☐ " };
                self.push_span(
                    DocSpan::owned(marker, Self::marker_style(), Some(range.clone())),
                    &range,
                );
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>, range: Range<usize>) {
        match tag {
            Tag::Paragraph => {
                if self.table.is_none() {
                    self.open_line(LineKind::Text, range.start);
                }
            }
            Tag::Heading { level, .. } => {
                let level = heading_number(level);
                self.open_line(LineKind::Heading(level), range.start);
                self.style.heading = level;
                self.style.bold = true;
            }
            Tag::BlockQuote(_) => {
                self.close_line();
                // The separator above a quote belongs to the outer level, without a quote bar.
                if self.pending_blank && !self.lines.is_empty() {
                    let at = range.start;
                    self.lines.push(DocLine::new(LineKind::Blank, at..at));
                    self.pending_blank = false;
                }
                self.quote_depth += 1;
                self.style.quote = true;
            }
            Tag::CodeBlock(kind) => {
                self.close_line();
                self.in_code_block = true;
                if let CodeBlockKind::Fenced(lang) = kind {
                    if !lang.is_empty() {
                        self.open_line(LineKind::Code, range.start);
                        if let Some(line) = self.current.as_mut() {
                            line.fill = true;
                            line.wrap = WrapMode::Char;
                            line.spans.push(DocSpan::decoration(
                                lang.to_string(),
                                DocStyle {
                                    code_block: true,
                                    dim: true,
                                    ..DocStyle::default()
                                },
                            ));
                        }
                        self.close_line();
                    }
                }
            }
            Tag::HtmlBlock => self.close_line(),
            Tag::List(start) => {
                self.close_line();
                self.lists.push(ListLevel {
                    number: start,
                    marker_width: 2,
                });
            }
            Tag::Item => {
                self.close_line();
                let depth = self.lists.len();
                let marker = match self.lists.last_mut() {
                    Some(ListLevel {
                        number: Some(number),
                        marker_width,
                    }) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        *marker_width = marker.width();
                        marker
                    }
                    Some(level) => {
                        level.marker_width = 2;
                        match depth {
                            1 => "• ",
                            2 => "◦ ",
                            _ => "▪ ",
                        }
                        .to_owned()
                    }
                    None => String::new(),
                };
                self.item_marker = Some(marker);
            }
            Tag::FootnoteDefinition(name) => {
                self.open_line(LineKind::Text, range.start);
                self.push_span(
                    DocSpan::owned(
                        format!("[^{name}]: "),
                        DocStyle {
                            dim: true,
                            ..DocStyle::default()
                        },
                        None,
                    ),
                    &range,
                );
            }
            Tag::DefinitionList => self.close_line(),
            Tag::DefinitionListTitle => {
                self.open_line(LineKind::Text, range.start);
                self.style.bold = true;
            }
            Tag::DefinitionListDefinition => {
                self.open_line(LineKind::Text, range.start);
                self.push_span(DocSpan::decoration("  ", DocStyle::default()), &range);
            }
            Tag::Table(alignments) => {
                self.close_line();
                self.table = Some(TableState {
                    index: self.tables_seen,
                    alignments,
                    src: range,
                    ..TableState::default()
                });
                self.tables_seen += 1;
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_head = true;
                    table.rows.push(Vec::new());
                }
                self.style.table_header = true;
            }
            Tag::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(row) = self.table.as_mut().and_then(|table| table.rows.last_mut()) {
                    row.push(TableCell {
                        spans: Vec::new(),
                        src: range,
                    });
                }
            }
            Tag::Emphasis => self.style.italic = true,
            Tag::Strong => self.style.bold = true,
            Tag::Strikethrough => self.style.strike = true,
            Tag::Superscript | Tag::Subscript => {}
            Tag::Link { .. } => self.style.link = true,
            Tag::Image { .. } => {
                let style = DocStyle {
                    dim: true,
                    ..self.style
                };
                self.push_span(DocSpan::owned("[image: ", style, None), &range);
                self.style.dim = true;
            }
            Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd, range: Range<usize>) {
        match tag {
            TagEnd::Paragraph => {
                if self.table.is_none() {
                    self.close_line();
                    self.pending_blank = true;
                }
            }
            TagEnd::Heading(_) => {
                self.style.heading = 0;
                self.style.bold = false;
                self.close_line();
                self.pending_blank = true;
            }
            TagEnd::BlockQuote(_) => {
                self.close_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.style.quote = self.quote_depth > 0;
                self.pending_blank = true;
            }
            TagEnd::CodeBlock => {
                self.close_line();
                self.in_code_block = false;
                self.pending_blank = true;
            }
            TagEnd::HtmlBlock => {
                self.close_line();
                if self
                    .lines
                    .last()
                    .is_some_and(|line| line.kind == LineKind::Html)
                {
                    self.pending_blank = true;
                }
            }
            TagEnd::List(_) => {
                self.close_line();
                self.lists.pop();
                self.item_marker = None;
                if self.lists.is_empty() {
                    self.pending_blank = true;
                }
            }
            TagEnd::Item => {
                self.close_line();
                // An item with no content still shows its marker.
                if let Some(marker) = self.item_marker.take() {
                    self.item_marker = Some(marker);
                    self.open_line(LineKind::Text, range.start);
                    self.close_line();
                }
                self.pending_blank = false;
            }
            TagEnd::FootnoteDefinition => {
                self.close_line();
                self.pending_blank = true;
            }
            TagEnd::DefinitionList => self.pending_blank = true,
            TagEnd::DefinitionListTitle => {
                self.style.bold = false;
                self.close_line();
            }
            TagEnd::DefinitionListDefinition => self.close_line(),
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.emit_table(table);
                }
                self.pending_blank = true;
            }
            TagEnd::TableHead => {
                self.style.table_header = false;
                if let Some(table) = self.table.as_mut() {
                    table.in_head = false;
                    table.header_rows = table.rows.len();
                }
            }
            TagEnd::TableRow | TagEnd::TableCell => {}
            TagEnd::Emphasis => self.style.italic = false,
            TagEnd::Strong => self.style.bold = self.style.heading > 0,
            TagEnd::Strikethrough => self.style.strike = false,
            TagEnd::Superscript | TagEnd::Subscript => {}
            TagEnd::Link => self.style.link = false,
            TagEnd::Image => {
                self.style.dim = false;
                let style = DocStyle {
                    dim: true,
                    ..self.style
                };
                self.push_span(DocSpan::owned("]", style, None), &range);
            }
            TagEnd::MetadataBlock(_) => {}
        }
    }

    fn code_text(&mut self, text: &str, range: &Range<usize>) {
        let style = DocStyle {
            code_block: true,
            ..DocStyle::default()
        };
        let slice = self.source.get(range.clone()).unwrap_or("");
        let exact = slice == text;
        let mut offset = 0;
        for piece in text.split_inclusive('\n') {
            let content = piece.strip_suffix('\n').unwrap_or(piece);
            let content = content.strip_suffix('\r').unwrap_or(content);
            let start = if exact {
                range.start + offset
            } else {
                range.start
            };
            self.open_line(LineKind::Code, start);
            if let Some(line) = self.current.as_mut() {
                line.fill = true;
                line.wrap = WrapMode::Char;
                let span = if exact {
                    DocSpan::source(start..start + content.len(), style)
                } else {
                    DocSpan::owned(content.to_owned(), style, Some(range.clone()))
                };
                line.src = start..start + content.len();
                line.spans.push(span);
            }
            self.close_line();
            offset += piece.len();
        }
    }

    fn html_block(&mut self, text: &str, range: &Range<usize>) {
        if is_html_comment(text) || self.in_html_comment {
            self.track_comment(text);
            return;
        }
        let style = DocStyle {
            dim: true,
            ..DocStyle::default()
        };
        for (index, piece) in text.split_inclusive('\n').enumerate() {
            let content = piece.trim_end_matches(['\n', '\r']);
            if content.is_empty() {
                continue;
            }
            self.open_line(LineKind::Html, range.start);
            let span = if index == 0 {
                self.text_span(content, range, style)
            } else {
                DocSpan::owned(content.to_owned(), style, Some(range.clone()))
            };
            if let Some(line) = self.current.as_mut() {
                line.wrap = WrapMode::Char;
                line.src = range.clone();
                line.spans.push(span);
            }
            self.close_line();
        }
    }

    /// Follow multi-line HTML comments across events.
    fn track_comment(&mut self, text: &str) {
        let trimmed = text.trim();
        if self.in_html_comment {
            if trimmed.contains("-->") {
                self.in_html_comment = false;
            }
        } else if trimmed.starts_with("<!--") && !trimmed.contains("-->") {
            self.in_html_comment = true;
        }
    }

    fn emit_table(&mut self, table: TableState) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let mut widths = vec![1usize; columns];
        for row in &table.rows {
            for (column, cell) in row.iter().enumerate() {
                let width: usize = cell
                    .spans
                    .iter()
                    .map(|span| span.as_str(self.source).width())
                    .sum();
                widths[column] = widths[column].max(width);
            }
        }
        let border = Self::marker_style();
        for (row_index, row) in table.rows.iter().enumerate() {
            if row_index == table.header_rows && table.header_rows > 0 {
                let rule = widths
                    .iter()
                    .map(|width| "─".repeat(width + 2))
                    .collect::<Vec<_>>()
                    .join("┼");
                self.open_line(LineKind::TableRow, table.src.start);
                if let Some(line) = self.current.as_mut() {
                    line.wrap = WrapMode::Char;
                    line.spans.push(DocSpan::decoration(rule, border));
                }
                self.close_line();
            }
            let start = row.first().map_or(table.src.start, |cell| cell.src.start);
            self.open_line(LineKind::TableRow, start);
            let mut spans = Vec::new();
            for (column, width) in widths.iter().enumerate() {
                if column > 0 {
                    spans.push(DocSpan::decoration("│", border));
                }
                let cell = row.get(column);
                let content_width: usize = cell
                    .map(|cell| {
                        cell.spans
                            .iter()
                            .map(|span| span.as_str(self.source).width())
                            .sum()
                    })
                    .unwrap_or(0);
                let pad = width.saturating_sub(content_width);
                let (left, right) = match table.alignments.get(column) {
                    Some(Alignment::Right) => (pad, 0),
                    Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                spans.push(DocSpan::decoration(
                    " ".repeat(left + 1),
                    DocStyle::default(),
                ));
                if let Some(cell) = cell {
                    let coordinates = (table.index, row_index as u32, column as u32);
                    for span in &cell.spans {
                        let mut span = span.clone();
                        span.cell = Some(coordinates);
                        spans.push(span);
                    }
                }
                spans.push(DocSpan::decoration(
                    " ".repeat(right + 1),
                    DocStyle::default(),
                ));
            }
            if let Some(line) = self.current.as_mut() {
                line.wrap = WrapMode::Char;
                line.src = row
                    .first()
                    .zip(row.last())
                    .map_or(table.src.clone(), |(first, last)| {
                        first.src.start..last.src.end
                    });
                line.spans = spans;
            }
            self.close_line();
        }
    }
}

fn heading_number(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn is_html_comment(text: &str) -> bool {
    text.trim_start().starts_with("<!--")
}

/// Text of a doc line with decorations (for tests and debugging).
#[cfg(test)]
fn line_text(source: &str, line: &DocLine) -> String {
    line.prefix
        .iter()
        .chain(line.spans.iter())
        .map(|span| span.as_str(source))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::document::SpanText;
    use super::*;

    fn rendered(source: &str) -> Vec<String> {
        parse(source)
            .lines
            .iter()
            .map(|line| line_text(source, line))
            .collect()
    }

    /// Every source-backed span must show exactly the bytes it claims.
    fn assert_offsets_consistent(source: &str) {
        for line in parse(source).lines {
            for span in line.prefix.iter().chain(&line.spans) {
                if let SpanText::Source(range) = &span.text {
                    assert!(
                        source.get(range.clone()).is_some(),
                        "span range {range:?} out of bounds"
                    );
                }
            }
        }
    }

    #[test]
    fn headings_paragraphs_and_inline_styles() {
        let source = "# Title\n\nSome **bold** and *it* with `code`.\n";
        let doc = parse(source);
        assert_eq!(
            rendered(source),
            ["Title", "", "Some bold and it with code."]
        );
        assert_eq!(doc.lines[0].kind, LineKind::Heading(1));
        let bold = doc.lines[2]
            .spans
            .iter()
            .find(|span| span.as_str(source) == "bold")
            .unwrap();
        assert!(bold.style.bold);
        let code = doc.lines[2]
            .spans
            .iter()
            .find(|span| span.as_str(source) == "code")
            .unwrap();
        assert!(code.style.inline_code);
        // Inline code maps to the text between the backticks.
        let SpanText::Source(range) = &code.text else {
            panic!("inline code should be source-backed");
        };
        assert_eq!(&source[range.clone()], "code");
        assert_offsets_consistent(source);
    }

    #[test]
    fn lists_nest_with_markers_and_numbers() {
        let source = "- one\n- two\n  - inner\n\n1. first\n2. second\n";
        assert_eq!(
            rendered(source),
            ["• one", "• two", "  ◦ inner", "", "1. first", "2. second"]
        );
        let doc = parse(source);
        assert_eq!(doc.lines[2].continuation[0].as_str(source), "    ");
    }

    #[test]
    fn task_lists_quotes_and_rules() {
        let source = "- [ ] todo\n- [x] done\n\n> quoted line\n\n---\n";
        let lines = rendered(source);
        assert_eq!(lines[0], "• ☐ todo");
        assert_eq!(lines[1], "• ☑ done");
        assert_eq!(lines[3], "│ quoted line");
        assert_eq!(parse(source).lines.last().unwrap().kind, LineKind::Rule);
    }

    #[test]
    fn code_blocks_keep_exact_line_offsets() {
        let source = "text\n\n```rust\nfn main() {}\nlet x = 1;\n```\n";
        let doc = parse(source);
        let code: Vec<_> = doc
            .lines
            .iter()
            .filter(|line| line.kind == LineKind::Code)
            .collect();
        assert_eq!(code.len(), 3, "language label plus two code lines");
        assert_eq!(line_text(source, code[1]), "fn main() {}");
        let SpanText::Source(range) = &code[2].spans[0].text else {
            panic!("fenced code should be source-backed");
        };
        assert_eq!(&source[range.clone()], "let x = 1;");
        assert!(code[1].fill);
    }

    #[test]
    fn html_comments_are_hidden_but_other_html_shows() {
        let source = "Keep <!--hc:a id=c1-->this<!--hc:/ id=c1--> text.\n\n<!-- hc:body id=c1 author=elio\n     : note -->\n\n<div>raw</div>\n";
        let lines = rendered(source);
        assert_eq!(lines[0], "Keep this text.");
        assert!(lines.iter().all(|line| !line.contains("hc:")), "{lines:?}");
        assert!(lines.iter().any(|line| line.contains("<div>raw</div>")));
        // The anchored word still maps to its own bytes.
        let doc = parse(source);
        let anchored = doc.lines[0]
            .spans
            .iter()
            .find(|span| span.as_str(source) == "this")
            .unwrap();
        let SpanText::Source(range) = &anchored.text else {
            panic!("paragraph text should be source-backed");
        };
        assert_eq!(&source[range.clone()], "this");
    }

    #[test]
    fn tables_align_columns_and_tag_cells() {
        let source = "| Name | Port |\n|------|-----:|\n| dev | 2222 |\n| general | 2223 |\n";
        let doc = parse(source);
        let lines = rendered(source);
        assert_eq!(lines[0], " Name    │ Port ");
        assert!(lines[1].contains('┼'));
        assert_eq!(lines[2], " dev     │ 2222 ");
        let port = doc.lines[3]
            .spans
            .iter()
            .find(|span| span.as_str(source) == "2223")
            .unwrap();
        assert_eq!(port.cell, Some((0, 2, 1)));
        assert_offsets_consistent(source);
    }

    #[test]
    fn escapes_and_entities_fall_back_to_owned_text() {
        let source = "a \\*literal\\* &amp; b\n";
        let lines = rendered(source);
        assert_eq!(lines[0], "a *literal* & b");
        assert_offsets_consistent(source);
    }

    #[test]
    fn markdown_paths_are_detected() {
        assert!(is_markdown_path("/workspace/PLAN.md"));
        assert!(is_markdown_path("notes.MARKDOWN"));
        assert!(!is_markdown_path("main.rs"));
    }
}

#[cfg(test)]
mod perf {
    /// Manual check: `cargo nextest run --run-ignored only -E 'test(markdown_2mib_timing)'`.
    #[test]
    #[ignore = "timing smoke test, run manually"]
    fn markdown_2mib_timing() {
        let block = "## Section heading\n\nSome **bold** text with `code` and a [link](x) that wraps across the width of the viewer.\n\n- item one\n- item two\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```\nfn code() {}\n```\n\n";
        let source = block.repeat(2 * 1024 * 1024 / block.len());
        let started = std::time::Instant::now();
        let doc = super::parse(&source);
        let parsed = started.elapsed();
        let counts = doc.row_counts(&source, 100);
        let counted = started.elapsed() - parsed;
        eprintln!(
            "bytes={} lines={} rows={} parse={parsed:?} row_counts={counted:?}",
            source.len(),
            doc.lines.len(),
            counts.iter().sum::<usize>()
        );
    }
}
