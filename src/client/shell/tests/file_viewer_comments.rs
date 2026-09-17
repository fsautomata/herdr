//! Inline comments in the file viewer (hpp fork): select, compose, write, conflict retry,
//! reply, delete, and error recovery — driven through real input events.

use super::*;
use crate::api::schema::{
    FileContentInfo, FileEntryInfo, FileEntryKind, FileWriteInfo, Method, Request, ResponseResult,
};
use sha2::{Digest, Sha256};

const DOC: &str =
    "# Spec\n\nThe system MUST retry on errors within thirty seconds.\n\nSecond paragraph.\n";

fn sha(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn state_with_document(text: &str) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let mut open = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::OpenFileViewer),
        &mut open,
    );
    let list = only_request(&open);
    state.handle_endpoint_result(
        "boot-1",
        &list.id,
        Ok(ResponseResult::FileList {
            path: "/repo".into(),
            parent: None,
            entries: vec![FileEntryInfo {
                name: "SPEC.md".into(),
                kind: FileEntryKind::File,
                size: text.len() as u64,
                target_is_dir: false,
            }],
            truncated: false,
        }),
    );
    let read = only_request(&state.handle_input_bytes(b"\r"));
    state.handle_endpoint_result("boot-1", &read.id, Ok(content(text)));
    state
}

fn content(text: &str) -> ResponseResult {
    ResponseResult::FileContent {
        file: FileContentInfo {
            path: "/repo/SPEC.md".into(),
            size: text.len() as u64,
            truncated: false,
            binary: false,
            lossy: false,
            text: Some(text.into()),
            sha256: sha(text),
        },
    }
}

fn only_request(outcome: &ClientShellInput) -> Request {
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!(
            "expected one endpoint request, got {}",
            outcome.actions.len()
        );
    };
    (**request).clone()
}

fn only_action_request(actions: &[ClientShellAction]) -> Request {
    let [ClientShellAction::Endpoint { request, .. }] = actions else {
        panic!("expected one follow-up request, got {}", actions.len());
    };
    (**request).clone()
}

fn screen(state: &mut ClientShellState) -> Vec<String> {
    let frame = state.compose(120, 34).expect("frame");
    frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect()
}

/// Screen position (column, row) of `needle`, counting cells (ASCII needles only).
fn position_of(state: &mut ClientShellState, needle: &str) -> (u16, u16) {
    for (row, line) in screen(state).iter().enumerate() {
        if let Some(byte) = line.find(needle) {
            let column = line[..byte].chars().count();
            return (column as u16, row as u16);
        }
    }
    panic!("{needle:?} not on screen:\n{}", screen(state).join("\n"));
}

fn mouse(
    state: &mut ClientShellState,
    kind: MouseEventKind,
    column: u16,
    row: u16,
) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })])
}

fn drag_select(state: &mut ClientShellState, needle: &str) {
    let (column, row) = position_of(state, needle);
    mouse(state, MouseEventKind::Down(MouseButton::Left), column, row);
    let last = column + needle.len() as u16 - 1;
    mouse(state, MouseEventKind::Drag(MouseButton::Left), last, row);
    mouse(state, MouseEventKind::Up(MouseButton::Left), last, row);
}

fn written(request: &Request) -> (String, Option<String>) {
    let Method::FileWrite(params) = &request.method else {
        panic!("expected file.write, got {:?}", request.method);
    };
    assert_eq!(params.path, "/repo/SPEC.md");
    assert!(!params.create);
    (params.text.clone(), params.expected_sha256.clone())
}

fn ack_write(state: &mut ClientShellState, request: &Request) -> Vec<ClientShellAction> {
    let (text, _) = written(request);
    let (_, actions) = state.handle_endpoint_result(
        "boot-1",
        &request.id,
        Ok(ResponseResult::FileWritten {
            file: FileWriteInfo {
                path: "/repo/SPEC.md".into(),
                size: text.len() as u64,
                sha256: sha(&text),
            },
        }),
    );
    actions
}

#[test]
fn selecting_text_and_commenting_writes_an_anchored_thread() {
    let mut state = state_with_document(DOC);
    drag_select(&mut state, "retry");
    let composer = state.handle_input_bytes(b"c");
    assert!(composer.actions.is_empty());
    assert!(screen(&mut state).join("\n").contains("comment on “retry”"));

    state.handle_input_bytes(b"also cover 429?");
    state.handle_input_bytes(b"\t"); // reply -> fix
    let save = state.handle_input_bytes(b"\r");
    let request = only_request(&save);
    let (text, expected) = written(&request);
    assert_eq!(expected.as_deref(), Some(sha(DOC).as_str()));
    assert!(
        text.contains("The system MUST <!--hc:a id=c1-->retry<!--hc:/ id=c1--> on errors"),
        "{text}"
    );
    assert!(text.contains("directive=fix"), "{text}");
    assert!(text.contains(": also cover 429? -->"), "{text}");
    // Rendered text is unchanged apart from the hidden comments.
    assert_eq!(crate::hc::strip_markers(&text), DOC);

    assert!(ack_write(&mut state, &request).is_empty());
    let shown = screen(&mut state).join("\n");
    assert!(shown.contains("saved comment c1"), "{shown}");
    assert!(shown.contains("▶ c1 fix"), "{shown}");
    assert!(shown.contains("also cover 429?"), "{shown}");
    assert!(!shown.contains("hc:a"), "markers must stay hidden: {shown}");
}

#[test]
fn a_concurrent_agent_edit_reloads_reanchors_and_retries_once() {
    let mut state = state_with_document(DOC);
    drag_select(&mut state, "retry");
    state.handle_input_bytes(b"c");
    state.handle_input_bytes(b"why?");
    let first = only_request(&state.handle_input_bytes(b"\r"));

    // The agent inserted a paragraph meanwhile: the write is refused as stale.
    let (_, actions) = state.handle_endpoint_result(
        "boot-1",
        &first.id,
        Err(ClientShellEndpointError {
            code: Some("stale_content".into()),
            message: "/repo/SPEC.md changed since it was read".into(),
        }),
    );
    let reload = only_action_request(&actions);
    assert!(matches!(&reload.method, Method::FileRead(params) if params.path == "/repo/SPEC.md"));

    let changed = DOC.replace("# Spec\n\n", "# Spec\n\nAgent added this intro.\n\n");
    let (_, actions) = state.handle_endpoint_result("boot-1", &reload.id, Ok(content(&changed)));
    let retry = only_action_request(&actions);
    let (text, expected) = written(&retry);
    assert_eq!(expected.as_deref(), Some(sha(&changed).as_str()));
    assert!(text.contains("Agent added this intro."), "{text}");
    assert!(
        text.contains("<!--hc:a id=c1-->retry<!--hc:/ id=c1-->"),
        "{text}"
    );

    // A second conflict is not retried again: the comment text returns to the composer.
    let (_, actions) = state.handle_endpoint_result(
        "boot-1",
        &retry.id,
        Err(ClientShellEndpointError {
            code: Some("stale_content".into()),
            message: "changed again".into(),
        }),
    );
    assert!(actions.is_empty());
    let shown = screen(&mut state).join("\n");
    assert!(shown.contains("comment not saved"), "{shown}");
    let Some(ClientShellOverlay::FileViewer(viewer)) = &state.overlay else {
        panic!("viewer open");
    };
    let composer = viewer
        .comments
        .composer
        .as_ref()
        .expect("composer restored");
    assert_eq!(composer.input.as_str(), "why?");
}

#[test]
fn replies_and_deletes_target_the_focused_thread() {
    let existing = "Retry <!--hc:a id=c1-->here<!--hc:/ id=c1--> please.\n<!-- hc:body id=c1 author=agent ts=2026-09-17T10:00Z directive=reply\n     quote=\"here\"\n     : done, see line 3 -->\n\nOther text.\n";
    let mut state = state_with_document(existing);
    let shown = screen(&mut state).join("\n");
    assert!(shown.contains("● c1 reply · answered"), "{shown}");
    assert!(shown.contains("agent: done, see line 3"), "{shown}");

    state.handle_input_bytes(b"n");
    assert!(screen(&mut state).join("\n").contains("▶ c1"));
    state.handle_input_bytes(b"a");
    state.handle_input_bytes(b"thanks");
    let reply = only_request(&state.handle_input_bytes(b"\r"));
    let (text, _) = written(&reply);
    assert!(text.contains("reply-to=c1"), "{text}");
    assert!(text.contains(": thanks -->"), "{text}");
    ack_write(&mut state, &reply);

    state.handle_input_bytes(b"d");
    assert!(screen(&mut state)
        .join("\n")
        .contains("delete thread c1? press d again"));
    let delete = only_request(&state.handle_input_bytes(b"d"));
    let (text, _) = written(&delete);
    assert_eq!(text, "Retry here please.\n\nOther text.\n");
    ack_write(&mut state, &delete);
    assert!(screen(&mut state).join("\n").contains("thread c1 deleted"));
}

#[test]
fn a_plain_click_on_an_anchor_focuses_its_thread() {
    let existing = "Retry <!--hc:a id=c1-->here<!--hc:/ id=c1--> please.\n<!-- hc:body id=c1 author=elio ts=t : why? -->\n";
    let mut state = state_with_document(existing);
    let (column, row) = position_of(&mut state, "here please");
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        column + 1,
        row,
    );
    mouse(
        &mut state,
        MouseEventKind::Up(MouseButton::Left),
        column + 1,
        row,
    );
    let Some(ClientShellOverlay::FileViewer(viewer)) = &state.overlay else {
        panic!("viewer open");
    };
    assert_eq!(viewer.comments.focused.as_deref(), Some("c1"));
    assert!(viewer.comments.selection.is_none());
}

#[test]
fn commenting_needs_a_selection_and_escape_cancels_the_composer() {
    let mut state = state_with_document(DOC);
    assert!(state.handle_input_bytes(b"c").actions.is_empty());
    assert!(screen(&mut state)
        .join("\n")
        .contains("select text with the mouse, then press c"));
    drag_select(&mut state, "Second");
    state.handle_input_bytes(b"c");
    state.handle_input_bytes(b"draft");
    state.handle_input_bytes(b"\x1b");
    let Some(ClientShellOverlay::FileViewer(viewer)) = &state.overlay else {
        panic!("viewer open");
    };
    assert!(viewer.comments.composer.is_none());
    // Esc with only a selection clears it; the next Esc leaves the document.
    state.handle_input_bytes(b"\x1b");
    state.handle_input_bytes(b"\x1b");
    let Some(ClientShellOverlay::FileViewer(viewer)) = &state.overlay else {
        panic!("viewer open");
    };
    assert_eq!(
        viewer.mode,
        super::super::file_viewer::FileViewerMode::Browse
    );
}

#[test]
fn an_orphaned_thread_can_be_reattached_to_a_new_selection() {
    let orphan = "The limit is now forty seconds.\n<!-- hc:body id=c1 author=elio ts=t quote=\"thirty seconds\" : per attempt? -->\n";
    let mut state = state_with_document(orphan);
    assert!(screen(&mut state).join("\n").contains("⚠ orphaned"));
    state.handle_input_bytes(b"n");
    drag_select(&mut state, "forty");
    let request = only_request(&state.handle_input_bytes(b"A"));
    let (text, _) = written(&request);
    assert!(
        text.starts_with("The limit is now <!--hc:a id=c1-->forty<!--hc:/ id=c1--> seconds."),
        "{text}"
    );
    ack_write(&mut state, &request);
    let shown = screen(&mut state).join("\n");
    assert!(shown.contains("thread c1 re-attached"), "{shown}");
    assert!(!shown.contains("orphaned"), "{shown}");
}
