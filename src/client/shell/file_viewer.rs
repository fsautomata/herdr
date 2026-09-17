//! File viewer overlay (hpp fork): browse directories and read files on the server host.
//!
//! State and input live here; drawing lives in `file_viewer_overlay.rs`. All filesystem access
//! goes through the `file.list` / `file.read` endpoint methods, so a client attached over SSH
//! sees the remote host's files, exactly like its panes do.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::*;
use crate::api::schema::{
    FileContentInfo, FileEntryInfo, FileEntryKind, FileListParams, FileReadParams, Method,
    ResponseResult,
};

/// Rows scrolled by PageUp/PageDown when the viewport height is unknown.
const PAGE_ROWS: usize = 16;
/// Rows scrolled by one mouse wheel notch.
const WHEEL_ROWS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FileViewerMode {
    Browse,
    View,
}

/// One visible row of the directory browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileViewerRow {
    Parent,
    Entry(usize),
}

#[derive(Debug)]
pub(super) struct FileViewerDocument {
    pub(super) content: FileContentInfo,
    /// Byte ranges of each line of `content.text`, without the line terminator.
    pub(super) lines: Vec<std::ops::Range<usize>>,
    /// Line-numbered plain view.
    pub(super) plain: crate::ui::document::Doc,
    /// Rendered view for markdown files.
    pub(super) markdown: Option<crate::ui::document::Doc>,
    /// Show the markdown view (when available) instead of the plain one.
    pub(super) rendered: bool,
    /// Top wrapped row shown in the viewport.
    pub(super) scroll: usize,
    /// Row counts per line for the last (width, view) pair; layout is costly on big files.
    row_cache: std::cell::RefCell<Option<RowCache>>,
    /// Comment threads, for markdown files that can be edited.
    pub(super) comments: Option<crate::hc::Parsed>,
}

#[derive(Debug)]
struct RowCache {
    width: usize,
    rendered: bool,
    counts: std::rc::Rc<Vec<usize>>,
}

impl FileViewerDocument {
    pub(super) fn new(content: FileContentInfo) -> Self {
        let lines = content.text.as_deref().map(line_ranges).unwrap_or_default();
        let plain = crate::ui::document::Doc::plain(&lines);
        let markdown = content
            .text
            .as_deref()
            .filter(|_| crate::ui::markdown::is_markdown_path(&content.path))
            .map(crate::ui::markdown::parse);
        let rendered = markdown.is_some();
        let comments = markdown
            .as_ref()
            .filter(|_| !content.truncated && !content.lossy && !content.binary)
            .and(content.text.as_deref())
            .map(crate::hc::parse);
        Self {
            content,
            lines,
            plain,
            markdown,
            rendered,
            scroll: 0,
            row_cache: std::cell::RefCell::new(None),
            comments,
        }
    }

    /// Whether comments can be written: the whole file was read as valid UTF-8 text.
    pub(super) fn editable(&self) -> bool {
        let content = &self.content;
        !content.truncated && !content.lossy && !content.binary && content.text.is_some()
    }

    pub(super) fn active_doc(&self) -> &crate::ui::document::Doc {
        match (&self.markdown, self.rendered) {
            (Some(markdown), true) => markdown,
            _ => &self.plain,
        }
    }

    /// Screen rows of each line of the active view at `width`, cached per width and view.
    pub(super) fn row_counts(&self, width: usize) -> std::rc::Rc<Vec<usize>> {
        let rendered = self.rendered && self.markdown.is_some();
        if let Some(cache) = self.row_cache.borrow().as_ref() {
            if cache.width == width && cache.rendered == rendered {
                return cache.counts.clone();
            }
        }
        let counts = std::rc::Rc::new(self.active_doc().row_counts(self.text(), width));
        *self.row_cache.borrow_mut() = Some(RowCache {
            width,
            rendered,
            counts: counts.clone(),
        });
        counts
    }

    pub(super) fn toggle_rendered(&mut self) {
        if self.markdown.is_some() {
            self.rendered = !self.rendered;
            self.scroll = 0;
        }
    }

    pub(super) fn text(&self) -> &str {
        self.content.text.as_deref().unwrap_or("")
    }
}

#[derive(Debug)]
pub(super) struct ClientFileViewerOverlay {
    pub(super) mode: FileViewerMode,
    /// Directory being browsed (absolute, canonical once the first listing arrives).
    pub(super) dir: String,
    pub(super) parent: Option<String>,
    pub(super) entries: Vec<FileEntryInfo>,
    pub(super) entries_truncated: bool,
    /// Index into `rows()`.
    pub(super) selected: usize,
    pub(super) list_scroll: usize,
    pub(super) query: TextEditor,
    pub(super) search_focused: bool,
    pub(super) show_hidden: bool,
    pub(super) loading: Option<String>,
    pub(super) error: Option<String>,
    pub(super) document: Option<Box<FileViewerDocument>>,
    /// Serial of the latest request; responses for older requests are ignored.
    pub(super) serial: u64,
    /// Entry name to select when the next listing arrives (e.g. the directory we came from).
    pub(super) select_after_list: Option<String>,
    pub(super) comments: super::file_viewer_comments::CommentState,
}

impl ClientFileViewerOverlay {
    pub(super) fn new(dir: String) -> Self {
        Self {
            mode: FileViewerMode::Browse,
            dir,
            parent: None,
            entries: Vec::new(),
            entries_truncated: false,
            selected: 0,
            list_scroll: 0,
            query: TextEditor::default(),
            search_focused: false,
            show_hidden: false,
            loading: None,
            error: None,
            document: None,
            serial: 0,
            select_after_list: None,
            comments: super::file_viewer_comments::CommentState::new(),
        }
    }

    /// Rows shown in the browser: `..` (when there is a parent and no filter), then the
    /// entries whose name contains the filter, case-insensitively.
    pub(crate) fn rows(&self) -> Vec<FileViewerRow> {
        let query = self.query.as_str().to_lowercase();
        let mut rows = Vec::with_capacity(self.entries.len() + 1);
        if query.is_empty() && self.parent.is_some() {
            rows.push(FileViewerRow::Parent);
        }
        rows.extend(
            self.entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| query.is_empty() || entry.name.to_lowercase().contains(&query))
                .map(|(index, _)| FileViewerRow::Entry(index)),
        );
        rows
    }

    fn selected_row(&self) -> Option<FileViewerRow> {
        self.rows().get(self.selected).copied()
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.rows().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let next = if delta.is_negative() {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected.saturating_add(delta.unsigned_abs())
        };
        self.selected = next.min(len - 1);
    }

    pub(super) fn scroll_document_by(&mut self, delta: isize) {
        self.scroll_document(delta);
    }

    fn scroll_document(&mut self, delta: isize) {
        if let Some(document) = self.document.as_mut() {
            document.scroll = if delta.is_negative() {
                document.scroll.saturating_sub(delta.unsigned_abs())
            } else {
                document.scroll.saturating_add(delta.unsigned_abs())
            };
        }
    }

    fn path_of(&self, name: &str) -> String {
        join_path(&self.dir, name)
    }
}

/// What a request should do once it has been sent.
enum FileViewerRequest {
    List {
        path: String,
        select: Option<String>,
    },
    Read {
        path: String,
    },
    Write {
        path: String,
        text: String,
        expected_sha256: String,
    },
}

/// Hit-test geometry produced by rendering, consumed by mouse input.
#[derive(Clone, Debug, Default)]
pub(crate) struct FileViewerHits {
    pub(crate) popup: Rect,
    pub(crate) close: Rect,
    pub(crate) search: Rect,
    /// Browser rows and their index into `rows()`.
    pub(crate) rows: Vec<(Rect, usize)>,
    pub(crate) scrollbar: Rect,
    pub(crate) scroll_metrics: Option<crate::pane::ScrollMetrics>,
    /// Largest valid scroll offset for the active mode.
    pub(crate) max_scroll: usize,
    /// Browser scroll offset after keeping the selection visible.
    pub(crate) list_scroll: usize,
    pub(crate) viewport_rows: usize,
    /// Document text area and its wrap width (view mode).
    pub(crate) text_area: Rect,
    pub(crate) text_width: usize,
    /// Effective top row of the document viewport.
    pub(crate) doc_scroll: usize,
    /// Thread headers in the comment panel.
    pub(crate) panel_threads: Vec<(Rect, String)>,
}

impl ClientShellState {
    /// Open the viewer at the focused pane's directory (falling back to the workspace default).
    pub(super) fn open_file_viewer(&mut self, outcome: &mut ClientShellInput) {
        let dir = self.file_viewer_start_dir();
        self.overlay = Some(ClientShellOverlay::FileViewer(Box::new(
            ClientFileViewerOverlay::new(dir.clone()),
        )));
        self.send_file_viewer_request(
            FileViewerRequest::List {
                path: dir,
                select: None,
            },
            outcome,
        );
        outcome.repaint = true;
    }

    fn file_viewer_start_dir(&self) -> String {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return "/".to_owned();
        };
        let focused = snapshot
            .focused_pane_id
            .as_deref()
            .and_then(|pane_id| snapshot.panes.iter().find(|pane| pane.pane_id == pane_id));
        focused
            .and_then(|pane| pane.foreground_cwd.clone().or_else(|| pane.cwd.clone()))
            .or_else(|| {
                let workspace_id = focused.map(|pane| pane.workspace_id.as_str())?;
                snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == workspace_id)
                    .map(|workspace| workspace.new_workspace_cwd.clone())
            })
            .filter(|dir| dir.starts_with('/'))
            .unwrap_or_else(|| "/".to_owned())
    }

    fn send_file_viewer_request(
        &mut self,
        request: FileViewerRequest,
        outcome: &mut ClientShellInput,
    ) {
        let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_mut() else {
            return;
        };
        viewer.serial = viewer.serial.wrapping_add(1);
        let serial = viewer.serial;
        viewer.error = None;
        let (method, kind) = match request {
            FileViewerRequest::List { path, select } => {
                viewer.loading = Some(format!("listing {path}"));
                viewer.select_after_list = select;
                (
                    Method::FileList(FileListParams {
                        path,
                        show_hidden: viewer.show_hidden,
                    }),
                    PendingEndpointKind::FileViewerList { serial },
                )
            }
            FileViewerRequest::Read { path } => {
                viewer.loading = Some(format!("reading {path}"));
                (
                    Method::FileRead(FileReadParams {
                        path,
                        max_bytes: None,
                    }),
                    PendingEndpointKind::FileViewerRead { serial },
                )
            }
            FileViewerRequest::Write {
                path,
                text,
                expected_sha256,
            } => {
                viewer.loading = Some("saving".to_owned());
                (
                    Method::FileWrite(crate::api::schema::FileWriteParams {
                        path,
                        text,
                        expected_sha256: Some(expected_sha256),
                        create: false,
                    }),
                    PendingEndpointKind::FileViewerWrite { serial },
                )
            }
        };
        let method_name = crate::api::api_method_name(&method).to_owned();
        let sent = self.push_endpoint_method_with_kind(method, kind, outcome);
        if !sent {
            if let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_mut() {
                viewer.loading = None;
                viewer.error = Some(format!(
                    "this server cannot {method_name}; restart it with an hpp build"
                ));
            }
        }
    }

    pub(super) fn send_file_viewer_write(
        &mut self,
        path: String,
        text: String,
        expected_sha256: String,
        outcome: &mut ClientShellInput,
    ) {
        self.send_file_viewer_request(
            FileViewerRequest::Write {
                path,
                text,
                expected_sha256,
            },
            outcome,
        );
    }

    pub(super) fn send_file_viewer_read(&mut self, path: String, outcome: &mut ClientShellInput) {
        self.send_file_viewer_request(FileViewerRequest::Read { path }, outcome);
    }

    /// Apply a file viewer response. Returns whether to repaint and follow-up actions.
    pub(super) fn complete_file_viewer_request(
        &mut self,
        kind: PendingEndpointKind,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> (bool, Vec<ClientShellAction>) {
        let mut outcome = ClientShellInput::default();
        let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_mut() else {
            return (false, Vec::new());
        };
        let (serial, is_write) = match kind {
            PendingEndpointKind::FileViewerList { serial }
            | PendingEndpointKind::FileViewerRead { serial } => (serial, false),
            PendingEndpointKind::FileViewerWrite { serial } => (serial, true),
            _ => return (false, Vec::new()),
        };
        if serial != viewer.serial {
            return (false, Vec::new());
        }
        viewer.loading = None;
        if is_write {
            self.complete_file_viewer_write(result, &mut outcome);
            return (true, outcome.actions);
        }
        let mut retry_after_reload = false;
        match result {
            Ok(ResponseResult::FileList {
                path,
                parent,
                entries,
                truncated,
            }) => {
                viewer.dir = path;
                viewer.parent = parent;
                viewer.entries = entries;
                viewer.entries_truncated = truncated;
                viewer.query.clear();
                viewer.search_focused = false;
                viewer.list_scroll = 0;
                viewer.mode = FileViewerMode::Browse;
                let select = viewer.select_after_list.take();
                viewer.selected = select
                    .and_then(|name| {
                        viewer.rows().iter().position(|row| {
                            matches!(row, FileViewerRow::Entry(index)
                                if viewer.entries[*index].name == name)
                        })
                    })
                    .unwrap_or(0);
            }
            Ok(ResponseResult::FileContent { file }) => {
                let mut document = FileViewerDocument::new(file);
                // Reloading the same file keeps the reader's place and view.
                if let Some(previous) = viewer
                    .document
                    .as_ref()
                    .filter(|previous| previous.content.path == document.content.path)
                {
                    document.scroll = previous.scroll;
                    document.rendered = previous.rendered && document.markdown.is_some();
                } else {
                    viewer.comments = super::file_viewer_comments::CommentState::new();
                }
                viewer.document = Some(Box::new(document));
                viewer.mode = FileViewerMode::View;
                retry_after_reload = viewer
                    .comments
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.reloading);
            }
            Ok(_) => viewer.error = Some("unexpected response from server".to_owned()),
            Err(error) => viewer.error = Some(error.message),
        }
        if retry_after_reload {
            self.retry_file_viewer_edit_after_reload(&mut outcome);
        }
        (true, outcome.actions)
    }

    fn file_viewer_open_selected(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_ref() else {
            return;
        };
        let request = match viewer.selected_row() {
            Some(FileViewerRow::Parent) => match viewer.parent.clone() {
                Some(parent) => FileViewerRequest::List {
                    path: parent,
                    select: current_dir_name(&viewer.dir),
                },
                None => return,
            },
            Some(FileViewerRow::Entry(index)) => {
                let entry = &viewer.entries[index];
                let path = viewer.path_of(&entry.name);
                if crate::file_access::entry_is_dir(entry) {
                    FileViewerRequest::List { path, select: None }
                } else if matches!(entry.kind, FileEntryKind::Other | FileEntryKind::Unknown) {
                    return;
                } else {
                    FileViewerRequest::Read { path }
                }
            }
            None => return,
        };
        self.send_file_viewer_request(request, outcome);
        outcome.repaint = true;
    }

    fn file_viewer_go_parent(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_ref() else {
            return;
        };
        let Some(parent) = viewer.parent.clone() else {
            return;
        };
        let select = current_dir_name(&viewer.dir);
        self.send_file_viewer_request(
            FileViewerRequest::List {
                path: parent,
                select,
            },
            outcome,
        );
        outcome.repaint = true;
    }

    fn file_viewer_reload(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_ref() else {
            return;
        };
        let request = match (viewer.mode, viewer.document.as_ref()) {
            (FileViewerMode::View, Some(document)) => FileViewerRequest::Read {
                path: document.content.path.clone(),
            },
            _ => FileViewerRequest::List {
                path: viewer.dir.clone(),
                select: viewer.selected_row().and_then(|row| match row {
                    FileViewerRow::Entry(index) => Some(viewer.entries[index].name.clone()),
                    FileViewerRow::Parent => None,
                }),
            },
        };
        self.send_file_viewer_request(request, outcome);
        outcome.repaint = true;
    }

    /// Insert typed or pasted text into the browser filter. Returns whether it was consumed.
    pub(super) fn insert_file_viewer_text(&mut self, text: &str) -> bool {
        if self.insert_file_viewer_comment_text(text) {
            return true;
        }
        let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_mut() else {
            return false;
        };
        if !viewer.search_focused {
            return false;
        }
        if viewer.query.insert(text) {
            viewer.selected = 0;
            viewer.list_scroll = 0;
        }
        true
    }

    /// Route a key to the file viewer. Returns false when the viewer is not open.
    pub(super) fn route_file_viewer_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if matches!(self.overlay, Some(ClientShellOverlay::FileViewer(_)))
            && self.route_file_viewer_comment_key(key, outcome)
        {
            return true;
        }
        let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_mut() else {
            return false;
        };
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        let page = self
            .hits
            .file_viewer
            .as_ref()
            .map_or(PAGE_ROWS, |hits| hits.viewport_rows.max(1));
        outcome.repaint = true;
        match viewer.mode {
            FileViewerMode::View => match code {
                KeyCode::Esc
                | KeyCode::Char('q')
                | KeyCode::Char('h')
                | KeyCode::Left
                | KeyCode::Backspace => viewer.mode = FileViewerMode::Browse,
                KeyCode::Down | KeyCode::Char('j') => viewer.scroll_document(1),
                KeyCode::Up | KeyCode::Char('k') => viewer.scroll_document(-1),
                KeyCode::PageDown | KeyCode::Char(' ') => viewer.scroll_document(page as isize),
                KeyCode::PageUp => viewer.scroll_document(-(page as isize)),
                KeyCode::Char('d') if ctrl => viewer.scroll_document((page / 2) as isize),
                KeyCode::Char('u') if ctrl => viewer.scroll_document(-((page / 2) as isize)),
                KeyCode::Home | KeyCode::Char('g') => {
                    if let Some(document) = viewer.document.as_mut() {
                        document.scroll = 0;
                    }
                }
                KeyCode::End | KeyCode::Char('G') => {
                    if let Some(document) = viewer.document.as_mut() {
                        document.scroll = usize::MAX;
                    }
                }
                KeyCode::Char('r') => self.file_viewer_reload(outcome),
                KeyCode::Char('m') => {
                    if let Some(document) = viewer.document.as_mut() {
                        document.toggle_rendered();
                    }
                }
                _ => outcome.repaint = false,
            },
            FileViewerMode::Browse if viewer.search_focused => match code {
                KeyCode::Esc => {
                    viewer.search_focused = false;
                    viewer.query.clear();
                    viewer.selected = 0;
                }
                KeyCode::Enter => self.file_viewer_open_selected(outcome),
                KeyCode::Down => viewer.move_selection(1),
                KeyCode::Up => viewer.move_selection(-1),
                KeyCode::Char('n') if ctrl => viewer.move_selection(1),
                KeyCode::Char('p') if ctrl => viewer.move_selection(-1),
                _ => match viewer.query.handle_key(key) {
                    Some(true) => {
                        viewer.selected = 0;
                        viewer.list_scroll = 0;
                    }
                    Some(false) => {}
                    None => outcome.repaint = false,
                },
            },
            FileViewerMode::Browse => match code {
                KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                KeyCode::Down | KeyCode::Char('j') => viewer.move_selection(1),
                KeyCode::Up | KeyCode::Char('k') => viewer.move_selection(-1),
                KeyCode::PageDown => viewer.move_selection(page as isize),
                KeyCode::PageUp => viewer.move_selection(-(page as isize)),
                KeyCode::Char('d') if ctrl => viewer.move_selection((page / 2) as isize),
                KeyCode::Char('u') if ctrl => viewer.move_selection(-((page / 2) as isize)),
                KeyCode::Home | KeyCode::Char('g') => viewer.selected = 0,
                KeyCode::End | KeyCode::Char('G') => viewer.move_selection(isize::MAX),
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                    self.file_viewer_open_selected(outcome)
                }
                KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('-') => {
                    self.file_viewer_go_parent(outcome)
                }
                KeyCode::Char('/') => viewer.search_focused = true,
                KeyCode::Char('.') => {
                    viewer.show_hidden = !viewer.show_hidden;
                    self.file_viewer_reload(outcome);
                }
                KeyCode::Char('r') => self.file_viewer_reload(outcome),
                KeyCode::Tab => {
                    if viewer.document.is_some() {
                        viewer.mode = FileViewerMode::View;
                    }
                }
                _ => outcome.repaint = false,
            },
        }
        true
    }

    /// Route a mouse event to the file viewer. Returns false when the viewer is not open.
    pub(super) fn route_file_viewer_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if matches!(self.overlay, Some(ClientShellOverlay::FileViewer(_)))
            && self.route_file_viewer_comment_mouse(mouse, outcome)
        {
            return true;
        }
        let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_mut() else {
            return false;
        };
        let hits = self.hits.file_viewer.clone().unwrap_or_default();
        let point = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let delta = if mouse.kind == MouseEventKind::ScrollDown {
                    WHEEL_ROWS as isize
                } else {
                    -(WHEEL_ROWS as isize)
                };
                match viewer.mode {
                    FileViewerMode::View => viewer.scroll_document(delta),
                    FileViewerMode::Browse => viewer.move_selection(delta),
                }
                outcome.repaint = true;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if super::contains(hits.close, point) {
                    if viewer.mode == FileViewerMode::View {
                        viewer.mode = FileViewerMode::Browse;
                    } else if viewer.search_focused {
                        viewer.search_focused = false;
                        viewer.query.clear();
                        viewer.selected = 0;
                    } else {
                        self.overlay = None;
                    }
                    outcome.repaint = true;
                } else if !super::contains(hits.popup, point) {
                    self.overlay = None;
                    outcome.repaint = true;
                } else if super::contains(hits.scrollbar, point) {
                    if let Some(metrics) = hits.scroll_metrics {
                        let offset = crate::ui::scrollbar_offset_from_row(
                            metrics,
                            hits.scrollbar,
                            mouse.row,
                        );
                        let target = metrics.max_offset_from_bottom.saturating_sub(offset);
                        match viewer.mode {
                            FileViewerMode::View => {
                                if let Some(document) = viewer.document.as_mut() {
                                    document.scroll = target;
                                }
                            }
                            FileViewerMode::Browse => {
                                viewer.selected = target;
                                viewer.move_selection(0);
                            }
                        }
                        outcome.repaint = true;
                    }
                } else if super::contains(hits.search, point)
                    && viewer.mode == FileViewerMode::Browse
                {
                    viewer.search_focused = true;
                    outcome.repaint = true;
                } else if let Some(index) = hits
                    .rows
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                    .map(|(_, index)| *index)
                {
                    // First click selects, a click on the selected row opens it.
                    if viewer.selected == index {
                        self.file_viewer_open_selected(outcome);
                    } else {
                        viewer.selected = index;
                    }
                    outcome.repaint = true;
                }
            }
            _ => {}
        }
        true
    }

    /// Keep scroll offsets inside the ranges measured by the last render.
    pub(super) fn clamp_file_viewer_scroll(&mut self) {
        let Some(hits) = self.hits.file_viewer.as_ref() else {
            return;
        };
        if let Some(ClientShellOverlay::FileViewer(viewer)) = self.overlay.as_mut() {
            match viewer.mode {
                FileViewerMode::Browse => viewer.list_scroll = hits.list_scroll,
                FileViewerMode::View => {
                    if let Some(document) = viewer.document.as_mut() {
                        document.scroll = document.scroll.min(hits.max_scroll);
                    }
                }
            }
        }
    }
}

/// Split text into line byte ranges, dropping `\n` / `\r\n` terminators and a final empty line.
pub(super) fn line_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            let end = if index > start && text.as_bytes()[index - 1] == b'\r' {
                index - 1
            } else {
                index
            };
            lines.push(start..end);
            start = index + 1;
        }
    }
    if start < text.len() {
        lines.push(start..text.len());
    }
    lines
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn current_dir_name(dir: &str) -> Option<String> {
    dir.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_ranges_handle_crlf_and_trailing_newline() {
        let text = "a\r\nbb\n\nc";
        let ranges = line_ranges(text);
        let lines: Vec<_> = ranges.iter().map(|range| &text[range.clone()]).collect();
        assert_eq!(lines, ["a", "bb", "", "c"]);
        assert_eq!(line_ranges("x\n").len(), 1);
        assert!(line_ranges("").is_empty());
    }

    #[test]
    fn paths_join_and_name_directories() {
        assert_eq!(join_path("/", "etc"), "/etc");
        assert_eq!(join_path("/root", "a.md"), "/root/a.md");
        assert_eq!(current_dir_name("/workspace/plans/"), Some("plans".into()));
        assert_eq!(current_dir_name("/"), None);
    }

    #[test]
    fn rows_include_parent_only_without_a_filter() {
        let mut viewer = ClientFileViewerOverlay::new("/root".into());
        viewer.parent = Some("/".into());
        viewer.entries = vec![
            FileEntryInfo {
                name: "docs".into(),
                kind: FileEntryKind::Directory,
                size: 0,
                target_is_dir: false,
            },
            FileEntryInfo {
                name: "README.md".into(),
                kind: FileEntryKind::File,
                size: 10,
                target_is_dir: false,
            },
        ];
        assert_eq!(
            viewer.rows(),
            [
                FileViewerRow::Parent,
                FileViewerRow::Entry(0),
                FileViewerRow::Entry(1)
            ]
        );
        viewer.query = TextEditor::from("read");
        assert_eq!(viewer.rows(), [FileViewerRow::Entry(1)]);
    }
}
