//! Source-mapped document model for the hpp file viewer.
//!
//! A [`Doc`] is a list of logical lines made of styled spans. Every span that shows file content
//! remembers which source bytes it came from, so a rendered cell can be mapped back to a byte
//! offset in the file (needed to anchor inline comments to a mouse selection). Layout wraps a
//! line into screen rows for a given width; rows carry that per-cell source mapping.

use std::ops::Range;

use unicode_width::UnicodeWidthChar;

/// Columns a tab advances to (multiples of this width).
const TAB_WIDTH: usize = 4;

/// Semantic style of a span; the renderer maps it to palette colors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DocStyle {
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) strike: bool,
    pub(crate) inline_code: bool,
    pub(crate) code_block: bool,
    pub(crate) link: bool,
    /// 1..=6 for heading text, 0 otherwise.
    pub(crate) heading: u8,
    pub(crate) quote: bool,
    /// List bullets, quote bars, table borders, rules.
    pub(crate) marker: bool,
    /// De-emphasized text (raw HTML, line numbers, image labels).
    pub(crate) dim: bool,
    pub(crate) table_header: bool,
    /// Unified diff line kinds.
    pub(crate) diff_add: bool,
    pub(crate) diff_del: bool,
    pub(crate) diff_hunk: bool,
}

/// Where a span's text lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SpanText {
    /// Exactly `source[range]`: every character maps to its own byte offset.
    Source(Range<usize>),
    /// Generated or transformed text. `src` (if any) is the region it stands for.
    Owned(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DocSpan {
    pub(crate) text: SpanText,
    pub(crate) style: DocStyle,
    /// Source region this span represents (for owned text; derived for source text).
    pub(crate) src: Option<Range<usize>>,
    /// Table cell this span belongs to: (table index, row, column); row 0 is the header.
    pub(crate) cell: Option<(u32, u32, u32)>,
}

impl DocSpan {
    pub(crate) fn source(range: Range<usize>, style: DocStyle) -> Self {
        Self {
            src: Some(range.clone()),
            text: SpanText::Source(range),
            style,
            cell: None,
        }
    }

    pub(crate) fn owned(
        text: impl Into<String>,
        style: DocStyle,
        src: Option<Range<usize>>,
    ) -> Self {
        Self {
            text: SpanText::Owned(text.into()),
            style,
            src,
            cell: None,
        }
    }

    pub(crate) fn decoration(text: impl Into<String>, style: DocStyle) -> Self {
        Self::owned(text, style, None)
    }

    pub(crate) fn as_str<'a>(&'a self, source: &'a str) -> &'a str {
        match &self.text {
            SpanText::Source(range) => source.get(range.clone()).unwrap_or(""),
            SpanText::Owned(text) => text,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineKind {
    Text,
    Heading(u8),
    Code,
    Quote,
    TableRow,
    Rule,
    Html,
    Blank,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WrapMode {
    Word,
    Char,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DocLine {
    pub(crate) kind: LineKind,
    /// Shown before the first row (list bullets, quote bars, line numbers).
    pub(crate) prefix: Vec<DocSpan>,
    /// Shown before wrapped rows; same display width as `prefix`.
    pub(crate) continuation: Vec<DocSpan>,
    pub(crate) spans: Vec<DocSpan>,
    /// Source bytes of the whole block line.
    pub(crate) src: Range<usize>,
    pub(crate) wrap: WrapMode,
    /// Paint the style background across the full row (code blocks).
    pub(crate) fill: bool,
}

impl DocLine {
    pub(crate) fn new(kind: LineKind, src: Range<usize>) -> Self {
        Self {
            kind,
            prefix: Vec::new(),
            continuation: Vec::new(),
            spans: Vec::new(),
            src,
            wrap: WrapMode::Word,
            fill: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Doc {
    pub(crate) lines: Vec<DocLine>,
}

/// One laid-out screen cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LaidCell {
    pub(crate) ch: char,
    pub(crate) width: u8,
    pub(crate) style: DocStyle,
    /// Byte offset in the source of the character shown here (None for decorations).
    pub(crate) src: Option<usize>,
    pub(crate) cell: Option<(u32, u32, u32)>,
}

/// A laid-out screen row of a logical line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LaidRow {
    pub(crate) cells: Vec<LaidCell>,
    /// Paint the row background (code blocks).
    pub(crate) fill: Option<DocStyle>,
    /// Horizontal rule: draw across the row.
    pub(crate) rule: bool,
}

impl LaidRow {
    #[cfg(test)]
    pub(crate) fn width(&self) -> usize {
        self.cells.iter().map(|cell| usize::from(cell.width)).sum()
    }
}

impl Doc {
    /// Plain text: one line per source line, character-wrapped, with a line-number gutter.
    pub(crate) fn plain(line_ranges: &[Range<usize>]) -> Self {
        let digits = line_ranges.len().max(1).ilog10() as usize + 1;
        let gutter = digits.max(3);
        let number_style = DocStyle {
            dim: true,
            ..DocStyle::default()
        };
        let lines = line_ranges
            .iter()
            .enumerate()
            .map(|(index, range)| {
                let mut line = DocLine::new(LineKind::Text, range.clone());
                line.wrap = WrapMode::Char;
                line.prefix = vec![DocSpan::decoration(
                    format!("{:>gutter$}  ", index + 1),
                    number_style,
                )];
                line.continuation = vec![DocSpan::decoration(" ".repeat(gutter + 2), number_style)];
                line.spans = vec![DocSpan::source(range.clone(), DocStyle::default())];
                line
            })
            .collect();
        Self { lines }
    }

    /// A unified diff: one character-wrapped line per diff line, styled by its prefix.
    pub(crate) fn diff(text: &str) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        for piece in text.split_inclusive('\n') {
            let content = piece.trim_end_matches(['\n', '\r']);
            let range = start..start + content.len();
            start += piece.len();
            let header = content.starts_with("diff --git")
                || content.starts_with("index ")
                || content.starts_with("--- ")
                || content.starts_with("+++ ")
                || content.starts_with("new file mode")
                || content.starts_with("deleted file mode");
            let style = DocStyle {
                diff_hunk: content.starts_with("@@"),
                diff_add: !header && content.starts_with('+'),
                diff_del: !header && content.starts_with('-'),
                dim: header,
                bold: content.starts_with("diff --git"),
                ..DocStyle::default()
            };
            let mut line = DocLine::new(LineKind::Code, range.clone());
            line.wrap = WrapMode::Char;
            line.spans = vec![DocSpan::source(range, style)];
            lines.push(line);
        }
        Self { lines }
    }

    /// Rows each line occupies at `width` cells.
    pub(crate) fn row_counts(&self, source: &str, width: usize) -> Vec<usize> {
        self.lines
            .iter()
            .map(|line| layout_line(source, line, width).len())
            .collect()
    }
}

/// Wrap one logical line into screen rows of at most `width` cells.
pub(crate) fn layout_line(source: &str, line: &DocLine, width: usize) -> Vec<LaidRow> {
    let width = width.max(1);
    if line.kind == LineKind::Rule {
        return vec![LaidRow {
            cells: flatten(source, &line.prefix),
            fill: None,
            rule: true,
        }];
    }
    let prefix = flatten(source, &line.prefix);
    let continuation = flatten(source, &line.continuation);
    let content = flatten(source, &line.spans);
    let fill = line.fill.then(|| {
        line.spans
            .first()
            .map(|span| span.style)
            .unwrap_or_default()
    });
    let prefix_w = cells_width(&prefix);
    let cont_w = cells_width(&continuation);
    // A prefix wider than the row would never leave room for content.
    let (prefix, continuation) = if prefix_w >= width || cont_w >= width {
        (Vec::new(), Vec::new())
    } else {
        (prefix, continuation)
    };

    let mut rows: Vec<LaidRow> = Vec::new();
    let mut current: Vec<LaidCell> = Vec::new();
    let mut available = width.saturating_sub(cells_width(&prefix)).max(1);
    let mut column = 0usize;
    // Index in `current` just after the last space (a word-wrap break opportunity).
    let mut last_break: Option<usize> = None;

    let push_row = |rows: &mut Vec<LaidRow>, cells: Vec<LaidCell>, first: bool| {
        let mut row_cells = if first {
            prefix.clone()
        } else {
            continuation.clone()
        };
        row_cells.extend(cells);
        rows.push(LaidRow {
            cells: row_cells,
            fill,
            rule: false,
        });
    };

    for mut cell in content {
        if cell.ch == '\t' {
            let w = TAB_WIDTH - column % TAB_WIDTH;
            cell.ch = ' ';
            cell.width = w as u8;
        }
        let w = usize::from(cell.width).min(available);
        if column + w > available {
            let first = rows.is_empty();
            match (line.wrap, cell.ch == ' ', last_break) {
                (WrapMode::Word, true, _) => {
                    // Break at this space and drop it.
                    push_row(&mut rows, std::mem::take(&mut current), first);
                    column = 0;
                    available = width.saturating_sub(cells_width(&continuation)).max(1);
                    last_break = None;
                    continue;
                }
                (WrapMode::Word, false, Some(split)) if split > 0 && split <= current.len() => {
                    let carry = current.split_off(split);
                    while current.last().is_some_and(|cell| cell.ch == ' ') {
                        current.pop();
                    }
                    push_row(&mut rows, std::mem::take(&mut current), first);
                    current = carry;
                    column = cells_width(&current);
                }
                _ => {
                    push_row(&mut rows, std::mem::take(&mut current), first);
                    column = 0;
                }
            }
            available = width.saturating_sub(cells_width(&continuation)).max(1);
            last_break = None;
        }
        cell.width = cell.width.min(available as u8);
        column += usize::from(cell.width);
        let is_space = cell.ch == ' ';
        current.push(cell);
        if is_space && line.wrap == WrapMode::Word {
            last_break = Some(current.len());
        }
    }
    let first = rows.is_empty();
    push_row(&mut rows, current, first);
    rows
}

fn cells_width(cells: &[LaidCell]) -> usize {
    cells.iter().map(|cell| usize::from(cell.width)).sum()
}

/// Expand spans into cells, mapping each character of source-backed text to its byte offset.
fn flatten(source: &str, spans: &[DocSpan]) -> Vec<LaidCell> {
    let mut cells = Vec::new();
    for span in spans {
        let text = span.as_str(source);
        let base = match &span.text {
            SpanText::Source(range) => Some(range.start),
            SpanText::Owned(_) => None,
        };
        let fallback = span.src.as_ref().map(|range| range.start);
        for (offset, ch) in text.char_indices() {
            if ch == '\n' || ch == '\r' {
                continue;
            }
            let (ch, width) = if ch == '\t' {
                ('\t', 1)
            } else if ch.is_control() {
                ('\u{FFFD}', 1)
            } else {
                (ch, ch.width().unwrap_or(1).min(2) as u8)
            };
            if width == 0 {
                // Zero-width characters (combining marks) would desync columns; skip them.
                continue;
            }
            cells.push(LaidCell {
                ch,
                width,
                style: span.style,
                src: base.map(|base| base + offset).or(fallback),
                cell: span.cell,
            });
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_of(source: &str, wrap: WrapMode) -> DocLine {
        let mut line = DocLine::new(LineKind::Text, 0..source.len());
        line.wrap = wrap;
        line.spans = vec![DocSpan::source(0..source.len(), DocStyle::default())];
        line
    }

    fn row_texts(rows: &[LaidRow]) -> Vec<String> {
        rows.iter()
            .map(|row| row.cells.iter().map(|cell| cell.ch).collect())
            .collect()
    }

    #[test]
    fn word_wrap_breaks_at_spaces_and_keeps_offsets() {
        let source = "alpha beta gamma";
        let rows = layout_line(source, &line_of(source, WrapMode::Word), 11);
        assert_eq!(row_texts(&rows), ["alpha beta", "gamma"]);
        // Every content cell maps to the byte that holds its character.
        for row in &rows {
            for cell in &row.cells {
                let offset = cell.src.expect("source-backed");
                assert_eq!(source[offset..].chars().next(), Some(cell.ch));
            }
        }
    }

    #[test]
    fn long_words_fall_back_to_character_wrap() {
        let source = "abcdefghij kl";
        let rows = layout_line(source, &line_of(source, WrapMode::Word), 4);
        assert_eq!(row_texts(&rows), ["abcd", "efgh", "ij", "kl"]);
    }

    #[test]
    fn char_wrap_keeps_spaces_and_wide_characters_whole() {
        let source = "ab 日本";
        let rows = layout_line(source, &line_of(source, WrapMode::Char), 4);
        assert_eq!(row_texts(&rows), ["ab ", "日本"]);
        assert!(rows.iter().all(|row| row.width() <= 4));
    }

    #[test]
    fn prefix_on_first_row_and_continuation_after() {
        let source = "one two three four";
        let mut line = line_of(source, WrapMode::Word);
        line.prefix = vec![DocSpan::decoration("• ", DocStyle::default())];
        line.continuation = vec![DocSpan::decoration("  ", DocStyle::default())];
        let rows = layout_line(source, &line, 10);
        assert_eq!(row_texts(&rows), ["• one two", "  three", "  four"]);
        assert_eq!(rows[0].cells[0].src, None);
    }

    #[test]
    fn plain_doc_numbers_lines_and_counts_rows() {
        let source = "first\nsecond line that wraps";
        let ranges = vec![0..5, 6..source.len()];
        let doc = Doc::plain(&ranges);
        let counts = doc.row_counts(source, 15);
        assert_eq!(counts, [1, 3]);
        let rows = layout_line(source, &doc.lines[1], 15);
        assert!(row_texts(&rows)[0].starts_with("  2  second"));
    }

    #[test]
    fn diff_lines_are_styled_by_prefix() {
        let text = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n context\n";
        let doc = Doc::diff(text);
        let style = |index: usize| doc.lines[index].spans[0].style;
        assert!(style(0).bold && style(0).dim);
        assert!(style(1).dim && !style(1).diff_del);
        assert!(style(2).dim && !style(2).diff_add);
        assert!(style(3).diff_hunk);
        assert!(style(4).diff_del);
        assert!(style(5).diff_add);
        assert_eq!(style(6), DocStyle::default());
        assert_eq!(doc.lines[5].spans[0].as_str(text), "+new");
    }

    #[test]
    fn empty_lines_still_take_a_row() {
        let rows = layout_line("", &line_of("", WrapMode::Word), 10);
        assert_eq!(rows.len(), 1);
    }
}
