//! Rendering for the file viewer overlay (hpp fork). Pure: reads overlay state, draws into the
//! buffer, and reports hit geometry plus clamped scroll offsets for the shell to apply.

use unicode_width::UnicodeWidthChar;

use super::*;
use crate::api::schema::FileEntryKind;
use crate::client::shell::file_viewer::{
    ClientFileViewerOverlay, FileViewerDocument, FileViewerHits, FileViewerMode, FileViewerRow,
};

/// Columns a tab advances to (multiples of this width).
const TAB_WIDTH: usize = 4;

pub(super) fn render_file_viewer_overlay(
    b: &mut Buffer,
    v: &ClientFileViewerOverlay,
    p: &Palette,
) -> Option<OverlayRender> {
    let a = b.area;
    let mx = (a.width / 24).max(1);
    let my = (a.height / 20).max(1);
    let q = Rect::new(
        a.x + mx,
        a.y + my,
        a.width.saturating_sub(mx * 2),
        a.height.saturating_sub(my * 2),
    )
    .intersection(a);
    let i = panel(b, q, p.accent, p.panel_bg)?;
    if i.width < 24 || i.height < 6 {
        return None;
    }
    let base = Style::default().fg(p.text).bg(p.panel_bg);
    let dim = Style::default().fg(p.overlay0).bg(p.panel_bg);

    // Header: title, path, close button.
    let close_label = match (v.mode, v.search_focused) {
        (FileViewerMode::View, _) | (FileViewerMode::Browse, true) => " esc back ",
        (FileViewerMode::Browse, false) => " esc close ",
    };
    let close_w = display_width(close_label);
    let close = Rect::new(i.right().saturating_sub(close_w), i.y, close_w, 1);
    button(
        b,
        close,
        close_label,
        Style::default()
            .fg(contrast(p))
            .bg(p.accent)
            .add_modifier(Modifier::BOLD),
    );
    let title = "files ";
    put_text(
        b,
        i.x,
        i.y,
        i.width,
        title,
        base.add_modifier(Modifier::BOLD),
    );
    let shown_path = match (v.mode, v.document.as_ref()) {
        (FileViewerMode::View, Some(document)) => document.content.path.as_str(),
        _ => v.dir.as_str(),
    };
    let path_x = i.x + display_width(title);
    let path_w = close.x.saturating_sub(path_x + 1);
    put_text(
        b,
        path_x,
        i.y,
        path_w,
        &truncate_start(shown_path, usize::from(path_w)),
        dim,
    );

    let body = Rect::new(i.x, i.y + 2, i.width, i.height.saturating_sub(3));
    let mut hits = FileViewerHits {
        popup: q,
        close,
        ..FileViewerHits::default()
    };

    let cursor = match v.mode {
        FileViewerMode::Browse => render_browser(b, v, i, body, p, &mut hits),
        FileViewerMode::View => {
            render_document(b, v, i, body, p, &mut hits);
            None
        }
    };

    let footer = match (v.mode, v.search_focused) {
        (FileViewerMode::View, _) => " scroll j/k/pgup/pgdn/g/G · reload r · back esc/h",
        (FileViewerMode::Browse, true) => " open enter · move ↑↓ · clear esc",
        (FileViewerMode::Browse, false) => {
            " open enter/l · up h/⌫ · filter / · hidden . · reload r · view tab · close esc"
        }
    };
    put_text(b, i.x, i.bottom() - 1, i.width, footer, dim);

    Some(OverlayRender {
        area: q,
        cancel: close,
        file_viewer: Some(hits),
        cursor,
        ..OverlayRender::default()
    })
}

fn render_browser(
    b: &mut Buffer,
    v: &ClientFileViewerOverlay,
    i: Rect,
    body: Rect,
    p: &Palette,
    hits: &mut FileViewerHits,
) -> Option<crate::protocol::CursorState> {
    let base = Style::default().fg(p.text).bg(p.panel_bg);
    let dim = Style::default().fg(p.overlay0).bg(p.panel_bg);
    let search_y = i.y + 1;
    hits.search = Rect::new(i.x, search_y, i.width, 1);
    let filter_hint = if v.search_focused {
        " / ".to_owned()
    } else if v.query.is_empty() {
        " / filter".to_owned()
    } else {
        format!(" / {}", v.query.as_str())
    };
    put_text(
        b,
        i.x,
        search_y,
        i.width,
        &filter_hint,
        if v.search_focused { base } else { dim },
    );
    let mut flags = Vec::new();
    if v.show_hidden {
        flags.push("hidden shown".to_owned());
    }
    if v.entries_truncated {
        flags.push("list truncated".to_owned());
    }
    flags.push(format!("{} entries", v.entries.len()));
    let flags = flags.join(" · ");
    put_right_text(b, i, search_y, &flags, dim);
    let cursor = if v.search_focused {
        text_editor::render(
            b,
            Rect::new(
                i.x + 3,
                search_y,
                i.width.saturating_sub(4 + display_width(&flags)),
                1,
            ),
            &v.query,
            base,
        )
    } else {
        None
    };

    let mut list = body;
    if let Some(message) = status_message(v) {
        let (text, style) = message;
        put_text(b, list.x, list.y, list.width, &text, style.bg(p.panel_bg));
        list = Rect::new(
            list.x,
            list.y + 1,
            list.width,
            list.height.saturating_sub(1),
        );
    }
    let rows = v.rows();
    let viewport = usize::from(list.height);
    hits.viewport_rows = viewport.max(1);
    if viewport == 0 {
        return cursor;
    }
    let selected = v.selected.min(rows.len().saturating_sub(1));
    let mut scroll = v.list_scroll;
    if selected < scroll {
        scroll = selected;
    } else if selected >= scroll + viewport {
        scroll = selected + 1 - viewport;
    }
    let max_scroll = rows.len().saturating_sub(viewport);
    scroll = scroll.min(max_scroll);
    hits.list_scroll = scroll;
    hits.max_scroll = max_scroll;

    let needs_scrollbar = rows.len() > viewport;
    let row_w = if needs_scrollbar {
        list.width.saturating_sub(1)
    } else {
        list.width
    };
    for (offset, row) in rows.iter().enumerate().skip(scroll).take(viewport) {
        let y = list.y + (offset - scroll) as u16;
        let rect = Rect::new(list.x, y, row_w, 1);
        let is_selected = offset == selected;
        let (label, size, is_dir) = match row {
            FileViewerRow::Parent => ("▴ ..".to_owned(), String::new(), true),
            FileViewerRow::Entry(index) => {
                let entry = &v.entries[*index];
                let is_dir = crate::file_access::entry_is_dir(entry);
                let marker = if is_dir { "▸ " } else { "  " };
                let suffix = match (entry.kind, is_dir) {
                    (_, true) => "/",
                    (FileEntryKind::Symlink, false) => " ↪",
                    _ => "",
                };
                let size = if is_dir {
                    String::new()
                } else {
                    human_size(entry.size)
                };
                (format!("{marker}{}{suffix}", entry.name), size, is_dir)
            }
        };
        let fg = if is_dir { p.accent } else { p.text };
        let style = if is_selected {
            Style::default()
                .fg(fg)
                .bg(p.surface1)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(fg).bg(p.panel_bg)
        };
        b.set_style(rect, style);
        let size_w = display_width(&size);
        put_text(
            b,
            rect.x + 1,
            y,
            rect.width.saturating_sub(size_w + 3),
            &label,
            style,
        );
        if !size.is_empty() {
            put_right_text(
                b,
                Rect::new(rect.x, y, rect.width.saturating_sub(1), 1),
                y,
                &size,
                if is_selected {
                    style.fg(p.subtext0)
                } else {
                    Style::default().fg(p.overlay1).bg(p.panel_bg)
                },
            );
        }
        hits.rows.push((rect, offset));
    }
    if needs_scrollbar {
        let track = Rect::new(list.right().saturating_sub(1), list.y, 1, list.height);
        let metrics = crate::pane::ScrollMetrics {
            offset_from_bottom: max_scroll.saturating_sub(scroll),
            max_offset_from_bottom: max_scroll,
            viewport_rows: viewport,
        };
        draw_scrollbar(b, track, metrics, p);
        hits.scrollbar = track;
        hits.scroll_metrics = Some(metrics);
    }
    cursor
}

fn render_document(
    b: &mut Buffer,
    v: &ClientFileViewerOverlay,
    i: Rect,
    body: Rect,
    p: &Palette,
    hits: &mut FileViewerHits,
) {
    let dim = Style::default().fg(p.overlay0).bg(p.panel_bg);
    let Some(document) = v.document.as_ref() else {
        return;
    };
    let content = &document.content;
    let mut meta = vec![human_size(content.size)];
    if content.truncated {
        meta.push("truncated".to_owned());
    }
    if content.lossy {
        meta.push("not utf-8".to_owned());
    }
    meta.push(format!("{} lines", document.lines.len()));
    put_text(
        b,
        i.x,
        i.y + 1,
        i.width,
        &format!(" {}", meta.join(" · ")),
        dim,
    );

    let mut area = body;
    if let Some((text, style)) = status_message(v) {
        put_text(b, area.x, area.y, area.width, &text, style.bg(p.panel_bg));
        area = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(1),
        );
    }
    let viewport = usize::from(area.height);
    hits.viewport_rows = viewport.max(1);
    if content.binary {
        put_text(
            b,
            area.x + 1,
            area.y,
            area.width.saturating_sub(1),
            &format!("binary file, {} — not shown", human_size(content.size)),
            dim,
        );
        return;
    }
    if viewport == 0 {
        return;
    }

    let gutter = (document.lines.len().max(1).ilog10() as u16 + 1).max(3) + 1;
    let text_width = |scrollbar: bool| {
        usize::from(
            area.width
                .saturating_sub(gutter + 1 + u16::from(scrollbar))
                .max(1),
        )
    };
    let mut width = text_width(false);
    let mut total_rows = total_wrapped_rows(document, width);
    let needs_scrollbar = total_rows > viewport;
    if needs_scrollbar {
        width = text_width(true);
        total_rows = total_wrapped_rows(document, width);
    }
    let max_scroll = total_rows.saturating_sub(viewport);
    let scroll = document.scroll.min(max_scroll);
    hits.max_scroll = max_scroll;

    let text_style = Style::default().fg(p.text).bg(p.panel_bg);
    let mut row = 0usize;
    let mut y = area.y;
    'lines: for (line_index, _) in document.lines.iter().enumerate() {
        let line = document.line(line_index);
        let line_rows = wrapped_rows(line, width);
        if row + line_rows <= scroll {
            row += line_rows;
            continue;
        }
        for (segment_index, segment) in wrap_segments(line, width).into_iter().enumerate() {
            if row < scroll {
                row += 1;
                continue;
            }
            if y >= area.bottom() {
                break 'lines;
            }
            if segment_index == 0 {
                let number = format!("{:>w$} ", line_index + 1, w = usize::from(gutter - 1));
                put_text(b, area.x, y, gutter, &number, dim);
            }
            put_text(
                b,
                area.x + gutter + 1,
                y,
                width as u16,
                &segment,
                text_style,
            );
            y += 1;
            row += 1;
        }
    }
    if needs_scrollbar {
        let track = Rect::new(area.right().saturating_sub(1), area.y, 1, area.height);
        let metrics = crate::pane::ScrollMetrics {
            offset_from_bottom: max_scroll.saturating_sub(scroll),
            max_offset_from_bottom: max_scroll,
            viewport_rows: viewport,
        };
        draw_scrollbar(b, track, metrics, p);
        hits.scrollbar = track;
        hits.scroll_metrics = Some(metrics);
    }
}

fn status_message(v: &ClientFileViewerOverlay) -> Option<(String, Style)> {
    if let Some(error) = &v.error {
        return Some((
            format!(" ⚠ {error}"),
            Style::default().fg(ratatui::style::Color::Red),
        ));
    }
    v.loading.as_ref().map(|loading| {
        (
            format!(" {loading}…"),
            Style::default().add_modifier(Modifier::DIM),
        )
    })
}

fn draw_scrollbar(b: &mut Buffer, track: Rect, metrics: crate::pane::ScrollMetrics, p: &Palette) {
    for y in track.y..track.bottom() {
        b[(track.x, y)]
            .set_symbol("▐")
            .set_style(Style::default().fg(p.overlay0).bg(p.panel_bg));
    }
    if let Some(thumb) = crate::ui::scrollbar_thumb(metrics, track) {
        for y in thumb.top..thumb.top.saturating_add(thumb.len) {
            b[(track.x, y)]
                .set_symbol("▐")
                .set_style(Style::default().fg(p.overlay1).bg(p.panel_bg));
        }
    }
}

fn total_wrapped_rows(document: &FileViewerDocument, width: usize) -> usize {
    (0..document.lines.len())
        .map(|index| wrapped_rows(document.line(index), width))
        .sum()
}

/// Display width of a character in the viewer: tabs expand, control characters show as one cell.
fn cell_width(ch: char, column: usize) -> usize {
    if ch == '\t' {
        TAB_WIDTH - column % TAB_WIDTH
    } else {
        ch.width().unwrap_or(1)
    }
}

/// Number of screen rows a line occupies when wrapped at `width` cells (at least one).
pub(crate) fn wrapped_rows(line: &str, width: usize) -> usize {
    let width = width.max(1);
    let mut rows = 1;
    let mut column = 0;
    for ch in line.chars() {
        let w = cell_width(ch, column).min(width);
        if column + w > width {
            rows += 1;
            column = 0;
        }
        column += cell_width(ch, column).min(width);
    }
    rows
}

/// Wrap a line into display rows of at most `width` cells, expanding tabs and replacing control
/// characters so every row renders predictably.
pub(crate) fn wrap_segments(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut segments = vec![String::new()];
    let mut column = 0;
    for ch in line.chars() {
        let w = cell_width(ch, column).min(width);
        if column + w > width {
            segments.push(String::new());
            column = 0;
        }
        let w = cell_width(ch, column).min(width);
        let current = segments.last_mut().expect("segments always has a row");
        if ch == '\t' {
            current.extend(std::iter::repeat_n(' ', w));
        } else if ch.is_control() {
            current.push('\u{FFFD}');
        } else {
            current.push(ch);
        }
        column += w;
    }
    segments
}

fn truncate_start(text: &str, width: usize) -> String {
    if display_width(text) <= width as u16 {
        return text.to_owned();
    }
    let mut kept = String::new();
    let mut used = 1; // room for the ellipsis
    for ch in text.chars().rev() {
        let w = ch.width().unwrap_or(1);
        if used + w > width {
            break;
        }
        used += w;
        kept.insert(0, ch);
    }
    format!("…{kept}")
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "K", "M", "G"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_counts_match_segments() {
        for (line, width) in [
            ("", 5),
            ("hello", 5),
            ("hello!", 5),
            ("a\tb", 3),
            ("日本語テキスト", 5),
            ("x".repeat(23).as_str(), 7),
        ] {
            assert_eq!(
                wrapped_rows(line, width),
                wrap_segments(line, width).len(),
                "{line:?} at {width}"
            );
        }
        assert_eq!(wrap_segments("hello!", 5), ["hello", "!"]);
        assert_eq!(wrap_segments("a\tb", 8), ["a   b"]);
    }

    #[test]
    fn wide_characters_never_split_across_the_edge() {
        let rows = wrap_segments("ab日本", 3);
        assert_eq!(rows, ["ab", "日", "本"]);
    }

    #[test]
    fn sizes_and_paths_are_compact() {
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(2048), "2.0K");
        assert_eq!(truncate_start("/workspace/plans/claude", 10), "…ns/claude");
        assert_eq!(truncate_start("/root", 10), "/root");
    }
}
