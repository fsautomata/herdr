//! Rendering for the file viewer overlay (hpp fork). Pure: reads overlay state, draws into the
//! buffer, and reports hit geometry plus clamped scroll offsets for the shell to apply.

use unicode_width::UnicodeWidthChar;

use super::*;
use crate::api::schema::FileEntryKind;
use crate::client::shell::file_viewer::{
    ClientFileViewerOverlay, FileViewerHits, FileViewerMode, FileViewerRow,
};

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
        (FileViewerMode::View | FileViewerMode::Diff, _) | (FileViewerMode::Browse, true) => {
            " esc back "
        }
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
        (FileViewerMode::Diff, _) => v
            .diff
            .as_ref()
            .map_or(v.dir.as_str(), |diff| diff.info.path.as_str()),
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
        FileViewerMode::View => render_document(b, v, i, body, p, &mut hits),
        FileViewerMode::Diff => {
            render_diff(b, v, i, body, p, &mut hits);
            None
        }
    };

    let footer = match (v.mode, v.search_focused) {
        (FileViewerMode::Diff, _) => " scroll j/k/pgup/pgdn/g/G · staged s · reload r · back esc",
        (FileViewerMode::View, _) if has_comments(v) => {
            " select+c comment · n/N thread · a reply · d delete · A re-attach · t panel · m raw · D diff · esc back"
        }
        (FileViewerMode::View, _) => {
            " scroll j/k/pgup/pgdn/g/G · raw/rendered m · diff D · reload r · back esc/h"
        }
        (FileViewerMode::Browse, true) => " open enter · move ↑↓ · clear esc",
        (FileViewerMode::Browse, false) => {
            " open enter/l · up h/⌫ · filter / · hidden . · diff D · reload r · close esc"
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
) -> Option<crate::protocol::CursorState> {
    let dim = Style::default().fg(p.overlay0).bg(p.panel_bg);
    let document = v.document.as_ref()?;
    let content = &document.content;
    let mut meta = vec![human_size(content.size)];
    if content.truncated {
        meta.push("truncated".to_owned());
    }
    if content.lossy {
        meta.push("not utf-8".to_owned());
    }
    meta.push(format!("{} lines", document.lines.len()));
    if let Some(parsed) = document.comments.as_ref() {
        let open = parsed
            .threads
            .iter()
            .filter(|thread| !parsed.is_resolved(thread))
            .count();
        meta.push(format!("{} comments ({open} open)", parsed.threads.len()));
    }
    if document.markdown.is_some() {
        meta.push(if document.rendered {
            "markdown".to_owned()
        } else {
            "raw".to_owned()
        });
    }
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
    if content.binary {
        hits.viewport_rows = usize::from(area.height).max(1);
        put_text(
            b,
            area.x + 1,
            area.y,
            area.width.saturating_sub(1),
            &format!("binary file, {} — not shown", human_size(content.size)),
            dim,
        );
        return None;
    }

    // Comment panel on the right when there is room; otherwise the composer takes the bottom.
    let comments = &v.comments;
    let has_threads = document
        .comments
        .as_ref()
        .is_some_and(|parsed| !parsed.threads.is_empty());
    let wants_panel = document.comments.is_some()
        && comments.show_panel
        && (has_threads || comments.composer.is_some());
    let mut cursor = None;
    if wants_panel && area.width >= 90 {
        let panel_width = (area.width / 3).clamp(32, 56);
        let panel = Rect::new(area.right() - panel_width, area.y, panel_width, area.height);
        cursor = file_viewer_panel::render_comment_panel(b, panel, document, comments, p, hits);
        area = Rect::new(area.x, area.y, area.width - panel_width, area.height);
    } else if let Some(composer) = comments.composer.as_ref() {
        if area.height > file_viewer_panel::COMPOSER_ROWS + 2 {
            let rows = file_viewer_panel::COMPOSER_ROWS;
            let rect = Rect::new(area.x, area.bottom() - rows, area.width, rows);
            cursor = file_viewer_panel::render_composer(b, rect, composer, p);
            area = Rect::new(area.x, area.y, area.width, area.height - rows);
        }
    }

    let viewport = usize::from(area.height);
    hits.viewport_rows = viewport.max(1);
    if viewport == 0 || area.width < 4 {
        return cursor;
    }

    // Leave one column of margin on the right; reserve one more for a scrollbar if needed.
    let mut width = usize::from(area.width) - 1;
    let mut counts = document.row_counts(width);
    let mut total_rows: usize = counts.iter().sum();
    let needs_scrollbar = total_rows > viewport;
    if needs_scrollbar {
        width -= 1;
        counts = document.row_counts(width);
        total_rows = counts.iter().sum();
    }
    let max_scroll = total_rows.saturating_sub(viewport);
    let scroll = document.scroll.min(max_scroll);
    hits.max_scroll = max_scroll;
    hits.text_area = Rect::new(area.x, area.y, width as u16, area.height);
    hits.text_width = width;
    hits.doc_scroll = scroll;

    let decorations = Decorations {
        anchors: document
            .comments
            .as_ref()
            .map(|parsed| {
                parsed
                    .threads
                    .iter()
                    .filter_map(|thread| {
                        let focused = comments.focused.as_deref() == Some(thread.id.as_str());
                        thread.anchor.range().map(|range| (range, focused))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        selection: comments.selection,
    };
    let source = document.text();
    let doc = document.active_doc();
    let mut row_index = 0usize;
    let mut y = area.y;
    'lines: for (line, count) in doc.lines.iter().zip(counts.iter()) {
        if row_index + count <= scroll {
            row_index += count;
            continue;
        }
        for row in crate::ui::document::layout_line(source, line, width) {
            if row_index < scroll {
                row_index += 1;
                continue;
            }
            if y >= area.bottom() {
                break 'lines;
            }
            draw_row(
                b,
                Rect::new(area.x, y, width as u16, 1),
                &row,
                p,
                row_index,
                &decorations,
            );
            y += 1;
            row_index += 1;
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
    cursor
}

fn render_diff(
    b: &mut Buffer,
    v: &ClientFileViewerOverlay,
    i: Rect,
    body: Rect,
    p: &Palette,
    hits: &mut FileViewerHits,
) {
    let dim = Style::default().fg(p.overlay0).bg(p.panel_bg);
    let Some(diff) = v.diff.as_ref() else {
        return;
    };
    let mut meta = vec![
        if diff.staged {
            "staged changes vs HEAD".to_owned()
        } else {
            "work tree vs HEAD".to_owned()
        },
        format!("repo {}", diff.info.repo_root),
    ];
    if diff.info.truncated {
        meta.push("truncated".to_owned());
    }
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
    if diff.info.text.is_empty() {
        put_text(
            b,
            area.x + 1,
            area.y,
            area.width.saturating_sub(1),
            if diff.staged {
                "no staged changes"
            } else {
                "no changes"
            },
            dim,
        );
        return;
    }
    if viewport == 0 || area.width < 4 {
        return;
    }
    let source = diff.info.text.as_str();
    let mut width = usize::from(area.width) - 1;
    let mut counts = diff.doc.row_counts(source, width);
    let mut total_rows: usize = counts.iter().sum();
    let needs_scrollbar = total_rows > viewport;
    if needs_scrollbar {
        width -= 1;
        counts = diff.doc.row_counts(source, width);
        total_rows = counts.iter().sum();
    }
    let max_scroll = total_rows.saturating_sub(viewport);
    let scroll = diff.scroll.min(max_scroll);
    hits.max_scroll = max_scroll;
    let decorations = Decorations {
        anchors: Vec::new(),
        selection: None,
    };
    let mut row_index = 0usize;
    let mut y = area.y;
    'lines: for (line, count) in diff.doc.lines.iter().zip(counts.iter()) {
        if row_index + count <= scroll {
            row_index += count;
            continue;
        }
        for row in crate::ui::document::layout_line(source, line, width) {
            if row_index < scroll {
                row_index += 1;
                continue;
            }
            if y >= area.bottom() {
                break 'lines;
            }
            draw_row(
                b,
                Rect::new(area.x, y, width as u16, 1),
                &row,
                p,
                row_index,
                &decorations,
            );
            y += 1;
            row_index += 1;
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

/// Comment anchors (with focus) and the mouse selection to paint over document text.
struct Decorations {
    anchors: Vec<(std::ops::Range<usize>, bool)>,
    selection: Option<crate::client::shell::file_viewer_comments::DocSelection>,
}

/// Draw one laid-out row, grouping cells of the same style into text runs.
fn draw_row(
    b: &mut Buffer,
    rect: Rect,
    row: &crate::ui::document::LaidRow,
    p: &Palette,
    row_index: usize,
    decorations: &Decorations,
) {
    if let Some(fill) = row.fill {
        b.set_style(rect, doc_style(fill, p));
    }
    let selected = decorations
        .selection
        .and_then(|selection| selection.columns_on(row_index));
    let mut x = rect.x;
    let mut col = 0usize;
    let mut run = String::new();
    let mut run_style = None;
    let mut run_x = x;
    for cell in &row.cells {
        let mut style = doc_style(cell.style, p);
        if selected.is_some_and(|(first, last)| col >= first && col <= last) {
            style = style.bg(p.selection_bg).fg(p.text);
        } else if let Some(src) = cell.src {
            if let Some((_, focused)) = decorations
                .anchors
                .iter()
                .find(|(range, _)| range.contains(&src))
            {
                style = if *focused {
                    style
                        .bg(p.surface1)
                        .fg(p.yellow)
                        .add_modifier(Modifier::UNDERLINED)
                } else {
                    style.bg(p.surface0).add_modifier(Modifier::UNDERLINED)
                };
            }
        }
        if run_style != Some(style) {
            if let Some(previous) = run_style {
                put_text(
                    b,
                    run_x,
                    rect.y,
                    rect.right().saturating_sub(run_x),
                    &run,
                    previous,
                );
            }
            run.clear();
            run_style = Some(style);
            run_x = x;
        }
        run.push(cell.ch);
        x = x.saturating_add(u16::from(cell.width));
        col += usize::from(cell.width);
    }
    if let Some(style) = run_style {
        put_text(
            b,
            run_x,
            rect.y,
            rect.right().saturating_sub(run_x),
            &run,
            style,
        );
    }
    if row.rule && x < rect.right() {
        let marker = doc_style(
            crate::ui::document::DocStyle {
                marker: true,
                ..Default::default()
            },
            p,
        );
        let rule = "─".repeat(usize::from(rect.right() - x));
        put_text(b, x, rect.y, rect.right() - x, &rule, marker);
    }
}

/// Map a semantic document style to terminal colors.
fn doc_style(style: crate::ui::document::DocStyle, p: &Palette) -> Style {
    let mut out = Style::default().fg(p.text).bg(p.panel_bg);
    if style.code_block {
        out = out.bg(p.surface0);
    }
    if style.quote {
        out = out.fg(p.subtext0);
    }
    match style.heading {
        0 => {}
        1 => {
            out = out
                .fg(p.accent)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        }
        2 => out = out.fg(p.accent).add_modifier(Modifier::BOLD),
        _ => out = out.fg(p.mauve).add_modifier(Modifier::BOLD),
    }
    if style.bold || style.table_header {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.italic {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.strike {
        out = out.add_modifier(Modifier::CROSSED_OUT);
    }
    if style.inline_code {
        out = out.fg(p.peach).bg(p.surface0);
    }
    if style.link {
        out = out.fg(p.blue).add_modifier(Modifier::UNDERLINED);
    }
    if style.marker {
        out = out.fg(p.accent);
    }
    if style.dim {
        out = out.fg(p.overlay0);
    }
    if style.diff_add {
        out = out.fg(p.green);
    }
    if style.diff_del {
        out = out.fg(p.red);
    }
    if style.diff_hunk {
        out = out.fg(p.mauve).add_modifier(Modifier::BOLD);
    }
    out
}

fn status_message(v: &ClientFileViewerOverlay) -> Option<(String, Style)> {
    if let Some(error) = &v.error {
        return Some((
            format!(" ⚠ {error}"),
            Style::default().fg(ratatui::style::Color::Red),
        ));
    }
    if let Some(loading) = &v.loading {
        return Some((
            format!(" {loading}…"),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    if v.mode == FileViewerMode::View {
        if let Some(notice) = &v.comments.notice {
            return Some((
                format!(" {notice}"),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
    }
    None
}

fn has_comments(v: &ClientFileViewerOverlay) -> bool {
    v.document
        .as_ref()
        .is_some_and(|document| document.comments.is_some())
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
    fn sizes_and_paths_are_compact() {
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(2048), "2.0K");
        assert_eq!(truncate_start("/workspace/plans/claude", 10), "…ns/claude");
        assert_eq!(truncate_start("/root", 10), "/root");
    }
}
