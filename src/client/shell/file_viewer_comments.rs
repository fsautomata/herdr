//! Inline comments in the file viewer (hpp fork): select rendered text, compose a comment with a
//! directive, and write it into the markdown file as `hc:` HTML comments.
//!
//! Edits are pure transforms of the file text (`crate::hc`). They are written with the file's
//! SHA-256 as a precondition; when an agent changed the file meanwhile the viewer reloads it,
//! re-applies the edit (re-anchoring a new comment by its quote), and retries once. Comment text
//! is never discarded on failure: the composer is reopened with it.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::file_viewer::{FileViewerDocument, FileViewerMode};
use super::*;
use crate::hc;

/// Leave headroom under the 1 MiB endpoint request limit for JSON escaping and the envelope.
const MAX_WRITE_REQUEST_BYTES: usize = 900 * 1024;
/// Two clicks on the same cell within this window select the word under it.
const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(400);

/// A position in the laid-out document: absolute wrapped row and display column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct DocPos {
    pub(super) row: usize,
    pub(super) col: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DocSelection {
    pub(super) anchor: DocPos,
    pub(super) cursor: DocPos,
}

impl DocSelection {
    pub(super) fn ordered(&self) -> (DocPos, DocPos) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// Columns selected on `row`, inclusive, or `None` when the row is outside the selection.
    pub(super) fn columns_on(&self, row: usize) -> Option<(usize, usize)> {
        let (start, end) = self.ordered();
        if row < start.row || row > end.row {
            return None;
        }
        let first = if row == start.row { start.col } else { 0 };
        let last = if row == end.row { end.col } else { usize::MAX };
        Some((first, last))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ComposerTarget {
    New {
        anchor: hc::NewAnchor,
        /// Text being commented on, for display and for re-anchoring after a reload.
        quote: String,
    },
    Reply {
        id: String,
    },
}

#[derive(Debug)]
pub(super) struct Composer {
    pub(super) target: ComposerTarget,
    pub(super) directive: hc::Directive,
    pub(super) input: TextEditor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CommentEdit {
    Add {
        anchor: hc::NewAnchor,
        quote: String,
        directive: hc::Directive,
        body: hc::NewBody,
    },
    Reply {
        id: String,
        body: hc::NewBody,
    },
    Delete {
        id: String,
    },
    Reattach {
        id: String,
        range: std::ops::Range<usize>,
    },
}

#[derive(Debug)]
pub(super) struct PendingEdit {
    pub(super) edit: CommentEdit,
    /// The full text sent with the write.
    pub(super) text: String,
    /// Thread to focus after a successful write.
    pub(super) focus: Option<String>,
    /// Already retried after a `stale_content` reload.
    pub(super) retried: bool,
    /// Waiting for the reload that precedes the retry.
    pub(super) reloading: bool,
}

#[derive(Debug, Default)]
pub(super) struct CommentState {
    pub(super) selection: Option<DocSelection>,
    pub(super) dragging: bool,
    pub(super) focused: Option<String>,
    pub(super) composer: Option<Composer>,
    pub(super) show_panel: bool,
    pub(super) confirm_delete: Option<String>,
    pub(super) pending: Option<PendingEdit>,
    pub(super) notice: Option<String>,
    last_click: Option<(std::time::Instant, DocPos)>,
}

impl CommentState {
    pub(super) fn new() -> Self {
        Self {
            show_panel: true,
            ..Self::default()
        }
    }
}

/// Author written into new comments: `HPP_AUTHOR`, then the login name.
pub(super) fn comment_author() -> String {
    ["HPP_AUTHOR", "USER", "USERNAME"]
        .iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "human".to_owned())
}

/// Apply an edit to `source`, returning the new text and the thread to focus.
pub(super) fn apply_edit(
    source: &str,
    edit: &CommentEdit,
) -> Result<(String, Option<String>), String> {
    let applied = match edit {
        CommentEdit::Add {
            anchor,
            directive,
            body,
            ..
        } => hc::add_comment(source, anchor.clone(), *directive, body)
            .map(|(text, id)| (text, Some(id)))
            .map_err(|err| err.to_string()),
        CommentEdit::Reply { id, body } => hc::append_reply(source, id, body, None)
            .map(|text| (text, Some(id.clone())))
            .map_err(|err| err.to_string()),
        CommentEdit::Delete { id } => hc::remove_thread(source, id)
            .map(|text| (text, None))
            .map_err(|err| err.to_string()),
        CommentEdit::Reattach { id, range } => hc::reattach(source, id, range.clone())
            .map(|text| (text, Some(id.clone())))
            .map_err(|err| err.to_string()),
    };
    // Every save also heals anchors that were found by quote after an agent rewrote text.
    applied.map(|(text, focus)| (hc::restore_markers(&text), focus))
}

/// After the file changed on disk, move a new comment's span anchor onto its quote again.
pub(super) fn reanchor_edit(source: &str, edit: &CommentEdit) -> Option<CommentEdit> {
    match edit {
        CommentEdit::Add {
            anchor: hc::NewAnchor::Span(_),
            quote,
            directive,
            body,
        } => {
            let range = hc::locate_quote(source, quote)?;
            Some(CommentEdit::Add {
                anchor: hc::NewAnchor::Span(range),
                quote: quote.clone(),
                directive: *directive,
                body: body.clone(),
            })
        }
        // A re-attach points at bytes the human selected in the old text; ask again.
        CommentEdit::Reattach { .. } => None,
        other => Some(other.clone()),
    }
}

impl FileViewerDocument {
    /// The laid-out row at `row` for a text width, if it exists.
    pub(super) fn laid_row(
        &self,
        width: usize,
        row: usize,
    ) -> Option<crate::ui::document::LaidRow> {
        let counts = self.row_counts(width);
        let mut first = 0usize;
        for (index, count) in counts.iter().enumerate() {
            if row < first + count {
                let line = self.active_doc().lines.get(index)?;
                return crate::ui::document::layout_line(self.text(), line, width)
                    .into_iter()
                    .nth(row - first);
            }
            first += count;
        }
        None
    }

    /// First wrapped row showing the source byte `offset` (or the nearest row after it).
    pub(super) fn row_of_offset(&self, width: usize, offset: usize) -> Option<usize> {
        let counts = self.row_counts(width);
        let doc = self.active_doc();
        let mut first = 0usize;
        for (line, count) in doc.lines.iter().zip(counts.iter()) {
            if line.src.end > offset || line.src.start >= offset {
                let rows = crate::ui::document::layout_line(self.text(), line, width);
                for (index, laid) in rows.iter().enumerate() {
                    if laid
                        .cells
                        .iter()
                        .any(|cell| cell.src.is_some_and(|src| src >= offset))
                    {
                        return Some(first + index);
                    }
                }
                if line.src.start >= offset {
                    return Some(first);
                }
            }
            first += count;
        }
        None
    }

    /// Resolve a selection to an anchor for a new comment, with the quoted text.
    pub(super) fn selection_anchor(
        &self,
        width: usize,
        selection: &DocSelection,
    ) -> Option<(hc::NewAnchor, String)> {
        let (start, end) = selection.ordered();
        let mut cells = Vec::new();
        for row in start.row..=end.row {
            let Some(laid) = self.laid_row(width, row) else {
                continue;
            };
            let (first, last) = selection.columns_on(row)?;
            let mut col = 0usize;
            for cell in laid.cells {
                let cell_end = col + usize::from(cell.width);
                if cell_end > first && col <= last {
                    cells.push(cell);
                }
                col = cell_end;
            }
        }
        let sourced: Vec<_> = cells.iter().filter(|cell| cell.src.is_some()).collect();
        let first = sourced.first()?;
        let text = self.text();
        if let Some((table, row, column)) = first.cell {
            let same_cell = sourced.iter().all(|cell| cell.cell == first.cell);
            if row > 0 && same_cell {
                let quote = sourced.iter().map(|cell| cell.ch).collect::<String>();
                return Some((
                    hc::NewAnchor::Cell {
                        table: table as usize,
                        row: row as usize - 1,
                        column: column as usize,
                    },
                    quote.trim().to_owned(),
                ));
            }
        }
        let start_byte = sourced.iter().filter_map(|cell| cell.src).min()?;
        let last_byte = sourced.iter().filter_map(|cell| cell.src).max()?;
        let end_byte = last_byte
            + text
                .get(last_byte..)
                .and_then(|rest| rest.chars().next())
                .map_or(1, char::len_utf8);
        let range = hc::snap_selection(text, start_byte..end_byte.min(text.len()));
        if range.is_empty() {
            return None;
        }
        let quote = hc::strip_markers(&text[range.clone()]);
        Some((hc::NewAnchor::Span(range), quote))
    }
}

impl ClientShellState {
    fn file_viewer_comments_mut(&mut self) -> Option<&mut CommentState> {
        match self.overlay.as_mut() {
            Some(ClientShellOverlay::FileViewer(viewer)) => Some(&mut viewer.comments),
            _ => None,
        }
    }

    /// Keys for comments while viewing a document. Returns true when consumed.
    pub(super) fn route_file_viewer_comment_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_mut() else {
            return false;
        };
        if viewer.mode != FileViewerMode::View {
            return false;
        }
        let Some(document) = viewer.document.as_ref() else {
            return false;
        };
        let commentable = document.comments.is_some();
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let comments = &mut viewer.comments;

        if let Some(composer) = comments.composer.as_mut() {
            outcome.repaint = true;
            match code {
                KeyCode::Esc => {
                    comments.composer = None;
                }
                KeyCode::Tab => {
                    if matches!(composer.target, ComposerTarget::New { .. }) {
                        composer.directive = composer.directive.next();
                    }
                }
                KeyCode::Enter => self.submit_file_viewer_composer(outcome),
                _ => {
                    let _ = composer.input.handle_key(key);
                }
            }
            return true;
        }

        if let Some(id) = comments.confirm_delete.take() {
            outcome.repaint = true;
            if matches!(code, KeyCode::Char('d') | KeyCode::Char('y')) {
                self.start_file_viewer_edit(CommentEdit::Delete { id }, outcome);
            } else {
                comments.notice = Some("delete cancelled".to_owned());
            }
            return true;
        }

        if !commentable {
            return false;
        }
        let shift = modifiers.contains(KeyModifiers::SHIFT);
        match code {
            KeyCode::Char('c') => {
                outcome.repaint = true;
                self.open_new_comment_composer();
                true
            }
            KeyCode::Char('n') => {
                outcome.repaint = true;
                self.focus_next_thread(1);
                true
            }
            KeyCode::Char('N') => {
                outcome.repaint = true;
                self.focus_next_thread(-1);
                true
            }
            KeyCode::Char('R') | KeyCode::Char('a') if shift || code == KeyCode::Char('a') => {
                outcome.repaint = true;
                match comments.focused.clone() {
                    Some(id) => {
                        comments.composer = Some(Composer {
                            target: ComposerTarget::Reply { id },
                            directive: hc::Directive::Reply,
                            input: TextEditor::default(),
                        });
                    }
                    None => comments.notice = Some("focus a thread with n first".to_owned()),
                }
                true
            }
            KeyCode::Char('d') => {
                outcome.repaint = true;
                match comments.focused.clone() {
                    Some(id) => {
                        comments.notice =
                            Some(format!("delete thread {id}? press d again to confirm"));
                        comments.confirm_delete = Some(id);
                    }
                    None => comments.notice = Some("focus a thread with n first".to_owned()),
                }
                true
            }
            KeyCode::Char('t') => {
                outcome.repaint = true;
                comments.show_panel = !comments.show_panel;
                true
            }
            KeyCode::Char('A') => {
                outcome.repaint = true;
                self.reattach_focused_thread(outcome);
                true
            }
            KeyCode::Esc if comments.selection.is_some() => {
                outcome.repaint = true;
                comments.selection = None;
                true
            }
            _ => false,
        }
    }

    pub(super) fn insert_file_viewer_comment_text(&mut self, text: &str) -> bool {
        let Some(comments) = self.file_viewer_comments_mut() else {
            return false;
        };
        let Some(composer) = comments.composer.as_mut() else {
            return false;
        };
        composer.input.insert(text);
        true
    }

    fn open_new_comment_composer(&mut self) {
        let width = self
            .hits
            .file_viewer
            .as_ref()
            .map_or(0, |hits| hits.text_width);
        let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_mut() else {
            return;
        };
        let Some(document) = viewer.document.as_ref() else {
            return;
        };
        let comments = &mut viewer.comments;
        let Some(selection) = comments.selection else {
            comments.notice = Some("select text with the mouse, then press c".to_owned());
            return;
        };
        if !document.editable() {
            comments.notice = Some(
                "comments need the whole file as valid UTF-8 (this view is truncated or lossy)"
                    .to_owned(),
            );
            return;
        }
        match document.selection_anchor(width.max(1), &selection) {
            Some((anchor, quote)) => {
                comments.composer = Some(Composer {
                    target: ComposerTarget::New { anchor, quote },
                    directive: hc::Directive::Reply,
                    input: TextEditor::default(),
                });
            }
            None => comments.notice = Some("the selection has no file text to anchor".to_owned()),
        }
    }

    fn reattach_focused_thread(&mut self, outcome: &mut ClientShellInput) {
        let width = self
            .hits
            .file_viewer
            .as_ref()
            .map_or(0, |hits| hits.text_width)
            .max(1);
        let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_mut() else {
            return;
        };
        let Some(document) = viewer.document.as_ref() else {
            return;
        };
        let comments = &mut viewer.comments;
        let (Some(id), Some(selection)) = (comments.focused.clone(), comments.selection) else {
            comments.notice =
                Some("focus a thread (n) and select its new text, then press A".to_owned());
            return;
        };
        match document.selection_anchor(width, &selection) {
            Some((hc::NewAnchor::Span(range), _)) => {
                self.start_file_viewer_edit(CommentEdit::Reattach { id, range }, outcome);
            }
            Some((hc::NewAnchor::Cell { .. }, _)) => {
                comments.notice = Some(
                    "table cells are anchored by cell; re-attach needs text outside tables"
                        .to_owned(),
                );
            }
            None => comments.notice = Some("the selection has no file text to anchor".to_owned()),
        }
    }

    fn submit_file_viewer_composer(&mut self, outcome: &mut ClientShellInput) {
        let Some(comments) = self.file_viewer_comments_mut() else {
            return;
        };
        let Some(composer) = comments.composer.as_ref() else {
            return;
        };
        let text = composer.input.as_str().trim().to_owned();
        if text.is_empty() {
            comments.notice = Some("type a comment first".to_owned());
            return;
        }
        let body = hc::NewBody {
            author: comment_author(),
            ts: hc::now_ts(),
            text,
        };
        let edit = match &composer.target {
            ComposerTarget::New { anchor, quote } => CommentEdit::Add {
                anchor: anchor.clone(),
                quote: quote.clone(),
                directive: composer.directive,
                body,
            },
            ComposerTarget::Reply { id } => CommentEdit::Reply {
                id: id.clone(),
                body,
            },
        };
        self.start_file_viewer_edit(edit, outcome);
    }

    /// Compute the edited text and send it with the current hash as a precondition.
    pub(super) fn start_file_viewer_edit(
        &mut self,
        edit: CommentEdit,
        outcome: &mut ClientShellInput,
    ) {
        let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_mut() else {
            return;
        };
        let Some(document) = viewer.document.as_ref() else {
            return;
        };
        let (text, focus) = match apply_edit(document.text(), &edit) {
            Ok(applied) => applied,
            Err(message) => {
                viewer.comments.notice = Some(message);
                return;
            }
        };
        if text.len() + text.len() / 8 > MAX_WRITE_REQUEST_BYTES {
            viewer.comments.notice = Some(
                "this file is too large to edit over the client connection (limit ~900 KiB)"
                    .to_owned(),
            );
            return;
        }
        let path = document.content.path.clone();
        let expected = document.content.sha256.clone();
        let retried = viewer
            .comments
            .pending
            .as_ref()
            .is_some_and(|pending| pending.retried);
        viewer.comments.pending = Some(PendingEdit {
            edit,
            text: text.clone(),
            focus,
            retried,
            reloading: false,
        });
        viewer.comments.composer = None;
        self.send_file_viewer_write(path, text, expected, outcome);
        outcome.repaint = true;
    }

    /// Handle the result of a comment write.
    pub(super) fn complete_file_viewer_write(
        &mut self,
        result: Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
        outcome: &mut ClientShellInput,
    ) {
        let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_mut() else {
            return;
        };
        let Some(pending) = viewer.comments.pending.take() else {
            return;
        };
        match result {
            Ok(crate::api::schema::ResponseResult::FileWritten { file }) => {
                let Some(document) = viewer.document.as_mut() else {
                    return;
                };
                let mut content = document.content.clone();
                content.size = file.size;
                content.sha256 = file.sha256;
                content.text = Some(pending.text);
                let scroll = document.scroll;
                let rendered = document.rendered;
                let mut replaced = FileViewerDocument::new(content);
                replaced.scroll = scroll;
                replaced.rendered = rendered && replaced.markdown.is_some();
                **document = replaced;
                viewer.comments.selection = None;
                viewer.comments.notice = Some(match (&pending.edit, &pending.focus) {
                    (CommentEdit::Delete { id }, _) => format!("thread {id} deleted"),
                    (CommentEdit::Reattach { id, .. }, _) => format!("thread {id} re-attached"),
                    (_, Some(id)) => format!("saved comment {id}"),
                    _ => "saved".to_owned(),
                });
                viewer.comments.focused = pending.focus;
            }
            Err(error) if error.code.as_deref() == Some("stale_content") && !pending.retried => {
                // The file changed underneath us: reload, then re-apply once.
                viewer.comments.notice =
                    Some("file changed on disk; reloading to re-apply".to_owned());
                let path = viewer
                    .document
                    .as_ref()
                    .map(|document| document.content.path.clone())
                    .unwrap_or_default();
                viewer.comments.pending = Some(PendingEdit {
                    retried: true,
                    reloading: true,
                    ..pending
                });
                self.send_file_viewer_read(path, outcome);
            }
            Err(error) => {
                viewer.comments.notice = Some(format!("comment not saved: {}", error.message));
                restore_composer(&mut viewer.comments, pending.edit);
            }
            Ok(_) => {
                viewer.comments.notice = Some("unexpected response to file.write".to_owned());
                restore_composer(&mut viewer.comments, pending.edit);
            }
        }
        outcome.repaint = true;
    }

    /// After a reload triggered by a stale write, re-apply the pending edit.
    pub(super) fn retry_file_viewer_edit_after_reload(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_mut() else {
            return;
        };
        let Some(pending) = viewer.comments.pending.take() else {
            return;
        };
        if !pending.reloading {
            viewer.comments.pending = Some(pending);
            return;
        }
        let source = viewer
            .document
            .as_ref()
            .map(|document| document.text().to_owned())
            .unwrap_or_default();
        match reanchor_edit(&source, &pending.edit) {
            Some(edit) => {
                viewer.comments.pending = Some(PendingEdit {
                    reloading: false,
                    ..pending
                });
                self.start_file_viewer_edit(edit, outcome);
            }
            None => {
                viewer.comments.notice =
                    Some("file changed and the commented text is gone; select it again".to_owned());
                restore_composer(&mut viewer.comments, pending.edit);
            }
        }
    }

    fn focus_next_thread(&mut self, delta: isize) {
        let width = self
            .hits
            .file_viewer
            .as_ref()
            .map_or(0, |hits| hits.text_width)
            .max(1);
        let viewport = self
            .hits
            .file_viewer
            .as_ref()
            .map_or(1, |hits| hits.viewport_rows);
        let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_mut() else {
            return;
        };
        let Some(document) = viewer.document.as_mut() else {
            return;
        };
        let Some(parsed) = document.comments.as_ref() else {
            return;
        };
        if parsed.threads.is_empty() {
            viewer.comments.notice = Some("no comments in this file".to_owned());
            return;
        }
        let ids: Vec<&str> = parsed
            .threads
            .iter()
            .map(|thread| thread.id.as_str())
            .collect();
        let current = viewer
            .comments
            .focused
            .as_deref()
            .and_then(|id| ids.iter().position(|candidate| *candidate == id));
        let len = ids.len() as isize;
        let next = match current {
            Some(index) => (index as isize + delta).rem_euclid(len) as usize,
            None if delta >= 0 => 0,
            None => ids.len() - 1,
        };
        let id = ids[next].to_owned();
        let target = parsed
            .thread(&id)
            .and_then(|thread| thread.anchor.range())
            .map(|range| range.start)
            .or_else(|| {
                parsed
                    .thread(&id)
                    .and_then(|thread| thread.bodies.first())
                    .map(|index| parsed.bodies[*index].range.start)
            });
        if let Some(row) = target.and_then(|offset| document.row_of_offset(width, offset)) {
            document.scroll = row.saturating_sub(viewport / 3);
        }
        viewer.comments.focused = Some(id);
        viewer.comments.notice = None;
    }

    /// Mouse handling for comments in view mode. Returns true when consumed.
    pub(super) fn route_file_viewer_comment_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(hits) = self.hits.file_viewer.clone() else {
            return false;
        };
        let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_mut() else {
            return false;
        };
        if viewer.mode != FileViewerMode::View {
            return false;
        }
        let point = (mouse.column, mouse.row);
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some((_, id)) = hits
                .panel_threads
                .iter()
                .find(|(rect, _)| super::contains(*rect, point))
            {
                viewer.comments.focused = Some(id.clone());
                outcome.repaint = true;
                return true;
            }
        }
        let area = hits.text_area;
        let inside = super::contains(area, point);
        let position = || DocPos {
            row: hits.doc_scroll + usize::from(mouse.row.saturating_sub(area.y)),
            col: usize::from(mouse.column.saturating_sub(area.x)),
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if inside => {
                let pos = position();
                let now = std::time::Instant::now();
                let double = viewer
                    .comments
                    .last_click
                    .is_some_and(|(at, last)| last == pos && now.duration_since(at) < DOUBLE_CLICK);
                viewer.comments.last_click = Some((now, pos));
                viewer.comments.selection = Some(DocSelection {
                    anchor: pos,
                    cursor: pos,
                });
                viewer.comments.dragging = !double;
                if double {
                    viewer.comments.notice = Some("word selected · c to comment".to_owned());
                }
                outcome.repaint = true;
                true
            }
            MouseEventKind::Drag(MouseButton::Left) if viewer.comments.dragging => {
                let mut pos = position();
                if mouse.row < area.y {
                    pos.row = hits.doc_scroll.saturating_sub(1);
                    viewer.scroll_document_by(-1);
                } else if mouse.row >= area.bottom() {
                    pos.row = hits.doc_scroll + hits.viewport_rows;
                    viewer.scroll_document_by(1);
                }
                if let Some(selection) = viewer.comments.selection.as_mut() {
                    selection.cursor = pos;
                }
                outcome.repaint = true;
                true
            }
            MouseEventKind::Up(MouseButton::Left) if viewer.comments.dragging => {
                viewer.comments.dragging = false;
                if let Some(selection) = viewer.comments.selection {
                    if selection.anchor == selection.cursor {
                        // A plain click: focus a thread whose anchor is under it, else clear.
                        let clicked = viewer.document.as_ref().and_then(|document| {
                            let laid =
                                document.laid_row(hits.text_width.max(1), selection.anchor.row)?;
                            let mut col = 0usize;
                            let offset = laid.cells.iter().find_map(|cell| {
                                let end = col + usize::from(cell.width);
                                let hit = (col..end).contains(&selection.anchor.col);
                                col = end;
                                hit.then_some(cell.src).flatten()
                            })?;
                            let parsed = document.comments.as_ref()?;
                            parsed
                                .threads
                                .iter()
                                .find(|thread| {
                                    thread
                                        .anchor
                                        .range()
                                        .is_some_and(|range| range.contains(&offset))
                                })
                                .map(|thread| thread.id.clone())
                        });
                        let double = viewer
                            .comments
                            .notice
                            .as_deref()
                            .is_some_and(|notice| notice.starts_with("word selected"));
                        if !double {
                            viewer.comments.selection = None;
                        }
                        if let Some(id) = clicked {
                            viewer.comments.focused = Some(id);
                        }
                    } else {
                        viewer.comments.notice = Some("c to comment on the selection".to_owned());
                    }
                }
                outcome.repaint = true;
                true
            }
            _ => false,
        }
    }
}

fn restore_composer(comments: &mut CommentState, edit: CommentEdit) {
    comments.composer = match edit {
        CommentEdit::Add {
            anchor,
            quote,
            directive,
            body,
        } => Some(Composer {
            target: ComposerTarget::New { anchor, quote },
            directive,
            input: TextEditor::from(body.text.as_str()),
        }),
        CommentEdit::Reply { id, body } => Some(Composer {
            target: ComposerTarget::Reply { id },
            directive: hc::Directive::Reply,
            input: TextEditor::from(body.text.as_str()),
        }),
        CommentEdit::Delete { .. } | CommentEdit::Reattach { .. } => None,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_orders_and_reports_columns() {
        let selection = DocSelection {
            anchor: DocPos { row: 3, col: 5 },
            cursor: DocPos { row: 1, col: 2 },
        };
        assert_eq!(selection.columns_on(0), None);
        assert_eq!(selection.columns_on(1), Some((2, usize::MAX)));
        assert_eq!(selection.columns_on(2), Some((0, usize::MAX)));
        assert_eq!(selection.columns_on(3), Some((0, 5)));
    }

    #[test]
    fn edits_apply_and_reanchor_after_external_changes() {
        let source = "Retry on 5xx within 30s.\n";
        let at = source.find("5xx").unwrap();
        let edit = CommentEdit::Add {
            anchor: hc::NewAnchor::Span(at..at + 3),
            quote: "5xx".into(),
            directive: hc::Directive::Fix,
            body: hc::NewBody {
                author: "elio".into(),
                ts: "t".into(),
                text: "include 429".into(),
            },
        };
        let (text, focus) = apply_edit(source, &edit).unwrap();
        assert_eq!(focus.as_deref(), Some("c1"));
        assert!(text.contains("directive=fix"));

        // An agent inserted a line above: the byte range moved, the quote did not.
        let changed = format!("New first line.\n{source}");
        let moved = reanchor_edit(&changed, &edit).unwrap();
        let (text, _) = apply_edit(&changed, &moved).unwrap();
        assert!(
            text.contains("<!--hc:a id=c1-->5xx<!--hc:/ id=c1-->"),
            "{text}"
        );

        assert!(reanchor_edit("unrelated\n", &edit).is_none());
    }
}
