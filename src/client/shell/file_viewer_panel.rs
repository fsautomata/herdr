//! Comment thread panel and composer for the file viewer (hpp fork). Pure rendering.

use super::*;
use crate::client::shell::file_viewer::{FileViewerDocument, FileViewerHits};
use crate::client::shell::file_viewer_comments::{CommentState, Composer, ComposerTarget};
use crate::hc;

/// Rows the composer occupies at the bottom of its area.
pub(super) const COMPOSER_ROWS: u16 = 4;

struct PanelLine {
    runs: Vec<(String, Style)>,
    /// Header row of this thread (clickable).
    thread: Option<String>,
}

pub(super) fn render_comment_panel(
    b: &mut Buffer,
    rect: Rect,
    document: &FileViewerDocument,
    comments: &CommentState,
    p: &Palette,
    hits: &mut FileViewerHits,
) -> Option<crate::protocol::CursorState> {
    let border = Style::default().fg(p.surface1).bg(p.panel_bg);
    for y in rect.y..rect.bottom() {
        put_text(b, rect.x, y, 1, "│", border);
    }
    let inner = Rect::new(
        rect.x + 2,
        rect.y,
        rect.width.saturating_sub(3),
        rect.height,
    );
    if inner.width < 10 || inner.height < 2 {
        return None;
    }
    let (list_area, cursor) = match comments.composer.as_ref() {
        Some(composer) if inner.height > COMPOSER_ROWS + 1 => {
            let composer_rect = Rect::new(
                inner.x,
                inner.bottom() - COMPOSER_ROWS,
                inner.width,
                COMPOSER_ROWS,
            );
            let cursor = render_composer(b, composer_rect, composer, p);
            (
                Rect::new(inner.x, inner.y, inner.width, inner.height - COMPOSER_ROWS),
                cursor,
            )
        }
        _ => (inner, None),
    };

    let Some(parsed) = document.comments.as_ref() else {
        return cursor;
    };
    let lines = panel_lines(
        parsed,
        comments.focused.as_deref(),
        usize::from(list_area.width),
        p,
    );
    if lines.is_empty() {
        put_text(
            b,
            list_area.x,
            list_area.y,
            list_area.width,
            "no comments yet",
            Style::default().fg(p.overlay0).bg(p.panel_bg),
        );
        return cursor;
    }
    let height = usize::from(list_area.height);
    let focused_line = comments.focused.as_deref().and_then(|id| {
        lines
            .iter()
            .position(|line| line.thread.as_deref() == Some(id))
    });
    let start = match focused_line {
        Some(line) if lines.len() > height => line.saturating_sub(1).min(lines.len() - height),
        _ => 0,
    };
    for (offset, line) in lines.iter().skip(start).take(height).enumerate() {
        let y = list_area.y + offset as u16;
        let mut x = list_area.x;
        for (text, style) in &line.runs {
            let remaining = list_area.right().saturating_sub(x);
            if remaining == 0 {
                break;
            }
            put_text(b, x, y, remaining, text, *style);
            x = x.saturating_add(display_width(text));
        }
        if let Some(id) = &line.thread {
            hits.panel_threads
                .push((Rect::new(list_area.x, y, list_area.width, 1), id.clone()));
        }
    }
    cursor
}

fn panel_lines(
    parsed: &hc::Parsed,
    focused: Option<&str>,
    width: usize,
    p: &Palette,
) -> Vec<PanelLine> {
    let base = Style::default().fg(p.text).bg(p.panel_bg);
    let dim = Style::default().fg(p.overlay0).bg(p.panel_bg);
    let mut lines = Vec::new();
    for thread in &parsed.threads {
        let is_focused = focused == Some(thread.id.as_str());
        let root = &parsed.bodies[thread.bodies[0]];
        let directive = root.effective_directive();
        let chip_color = match directive {
            hc::Directive::Fix => p.red,
            hc::Directive::Discuss => p.mauve,
            hc::Directive::Reply => p.blue,
        };
        let mut header = vec![
            (
                if is_focused { "▶ " } else { "● " }.to_owned(),
                base.fg(if is_focused { p.accent } else { p.overlay1 }),
            ),
            (
                thread.id.clone(),
                base.add_modifier(Modifier::BOLD)
                    .fg(if is_focused { p.accent } else { p.text }),
            ),
            (" ".to_owned(), base),
            (
                directive.as_str().to_owned(),
                base.fg(chip_color).add_modifier(Modifier::BOLD),
            ),
        ];
        if parsed.is_resolved(thread) {
            header.push((" · resolved".to_owned(), base.fg(p.green)));
        } else if parsed.is_answered(thread) {
            header.push((" · answered".to_owned(), base.fg(p.green)));
        }
        match thread.anchor {
            hc::Anchor::Orphan => header.push((" · ⚠ orphaned".to_owned(), base.fg(p.red))),
            hc::Anchor::Quoted(_) | hc::Anchor::CellRow(_) => {
                header.push((" · ⚠ moved".to_owned(), base.fg(p.peach)))
            }
            _ => {}
        }
        lines.push(PanelLine {
            runs: header,
            thread: Some(thread.id.clone()),
        });
        let quote = root
            .cell
            .as_deref()
            .map(|cell| format!("cell {cell}"))
            .or_else(|| root.quote.clone().map(|quote| format!("“{quote}”")));
        if let Some(quote) = quote {
            let quote = truncate_chars(&quote.replace('\n', " "), width.saturating_sub(2));
            lines.push(PanelLine {
                runs: vec![
                    ("  ".to_owned(), dim),
                    (quote, dim.add_modifier(Modifier::ITALIC)),
                ],
                thread: None,
            });
        }
        for index in &thread.bodies {
            let body = &parsed.bodies[*index];
            let author_style = if body.is_agent() {
                base.fg(p.teal).add_modifier(Modifier::BOLD)
            } else {
                base.fg(p.accent).add_modifier(Modifier::BOLD)
            };
            let label = format!("{}: ", body.author);
            let wrapped = wrap_words(&body.text, width.saturating_sub(4).max(8));
            for (row, text) in wrapped.into_iter().enumerate() {
                let mut runs = vec![("  ".to_owned(), base)];
                if row == 0 {
                    runs.push((label.clone(), author_style));
                } else {
                    runs.push(("  ".to_owned(), base));
                }
                runs.push((text, base));
                lines.push(PanelLine { runs, thread: None });
            }
        }
        lines.push(PanelLine {
            runs: Vec::new(),
            thread: None,
        });
    }
    lines
}

pub(super) fn render_composer(
    b: &mut Buffer,
    rect: Rect,
    composer: &Composer,
    p: &Palette,
) -> Option<crate::protocol::CursorState> {
    let base = Style::default().fg(p.text).bg(p.panel_bg);
    let dim = Style::default().fg(p.overlay0).bg(p.panel_bg);
    put_text(
        b,
        rect.x,
        rect.y,
        rect.width,
        &"─".repeat(usize::from(rect.width)),
        Style::default().fg(p.accent).bg(p.panel_bg),
    );
    let (title, chip) = match &composer.target {
        ComposerTarget::New { quote, .. } => (
            format!(
                "comment on “{}”",
                truncate_chars(
                    &quote.replace('\n', " "),
                    usize::from(rect.width).saturating_sub(24)
                )
            ),
            Some(composer.directive),
        ),
        ComposerTarget::Reply { id } => (format!("reply to {id}"), None),
    };
    put_text(
        b,
        rect.x,
        rect.y + 1,
        rect.width,
        &title,
        base.add_modifier(Modifier::BOLD),
    );
    if let Some(directive) = chip {
        let label = format!(" {} ", directive.as_str());
        let color = match directive {
            hc::Directive::Fix => p.red,
            hc::Directive::Discuss => p.mauve,
            hc::Directive::Reply => p.blue,
        };
        put_right_text(
            b,
            rect,
            rect.y + 1,
            &label,
            Style::default()
                .fg(contrast(p))
                .bg(color)
                .add_modifier(Modifier::BOLD),
        );
    }
    let input = Rect::new(rect.x, rect.y + 2, rect.width, 1);
    let input_style = Style::default().fg(p.text).bg(p.surface0);
    b.set_style(input, input_style);
    let cursor = text_editor::render(
        b,
        Rect::new(input.x + 1, input.y, input.width.saturating_sub(2), 1),
        &composer.input,
        input_style,
    );
    let hint = if chip.is_some() {
        "enter save · tab reply/fix/discuss · esc cancel"
    } else {
        "enter save · esc cancel"
    };
    put_text(b, rect.x, rect.y + 3, rect.width, hint, dim);
    cursor
}

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for paragraph in text.split('\n') {
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            let needed = if current.is_empty() {
                display_width(word) as usize
            } else {
                display_width(&current) as usize + 1 + display_width(word) as usize
            };
            if needed > width && !current.is_empty() {
                rows.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        rows.push(current);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

fn truncate_chars(text: &str, width: usize) -> String {
    if display_width(text) as usize <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    for ch in text.chars() {
        if display_width(&out) as usize + 2 > width {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_wrap_and_keep_paragraphs() {
        assert_eq!(wrap_words("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap_words("a\nb", 10), ["a", "b"]);
        assert_eq!(truncate_chars("abcdefgh", 5), "abcd…");
    }
}
