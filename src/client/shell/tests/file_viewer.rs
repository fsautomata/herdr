//! File viewer overlay (hpp fork): open, browse, read, navigate back, and degrade on old servers.

use super::*;
use crate::api::schema::{
    FileContentInfo, FileEntryInfo, FileEntryKind, Method, Request, ResponseResult,
};

fn open_state() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state
}

fn only_request(outcome: &ClientShellInput) -> Request {
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!(
            "expected exactly one endpoint request, got {:?}",
            outcome.actions.len()
        );
    };
    (**request).clone()
}

fn entry(name: &str, kind: FileEntryKind, size: u64) -> FileEntryInfo {
    FileEntryInfo {
        name: name.into(),
        kind,
        size,
        target_is_dir: false,
    }
}

fn listing(path: &str, parent: Option<&str>, entries: Vec<FileEntryInfo>) -> ResponseResult {
    ResponseResult::FileList {
        path: path.into(),
        parent: parent.map(str::to_owned),
        entries,
        truncated: false,
    }
}

fn content(path: &str, text: &str) -> ResponseResult {
    ResponseResult::FileContent {
        file: FileContentInfo {
            path: path.into(),
            size: text.len() as u64,
            truncated: false,
            binary: false,
            lossy: false,
            text: Some(text.into()),
            sha256: "0".repeat(64),
        },
    }
}

fn screen(state: &mut ClientShellState) -> String {
    let frame = state.compose(100, 30).expect("frame");
    frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn open_viewer(state: &mut ClientShellState) -> Request {
    let mut open = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::OpenFileViewer),
        &mut open,
    );
    assert!(open.repaint);
    only_request(&open)
}

#[test]
fn file_viewer_opens_at_the_focused_pane_directory() {
    let mut state = open_state();
    let request = open_viewer(&mut state);
    let Method::FileList(params) = &request.method else {
        panic!(
            "viewer should list a directory first, got {:?}",
            request.method
        );
    };
    assert_eq!(params.path, "/repo");
    assert!(!params.show_hidden);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::FileViewer(_))
    ));
    assert!(screen(&mut state).contains("listing /repo"));
}

#[test]
fn file_viewer_lists_entries_opens_a_file_and_returns_to_the_list() {
    let mut state = open_state();
    let list = open_viewer(&mut state);
    let (repaint, actions) = state.handle_endpoint_result(
        "boot-1",
        &list.id,
        Ok(listing(
            "/repo",
            Some("/"),
            vec![
                entry("src", FileEntryKind::Directory, 0),
                entry("README.md", FileEntryKind::File, 2048),
            ],
        )),
    );
    assert!(repaint && actions.is_empty());
    let text = screen(&mut state);
    assert!(text.contains("▴ .."), "{text}");
    assert!(text.contains("▸ src/"), "{text}");
    assert!(text.contains("README.md"), "{text}");
    assert!(text.contains("2.0K"), "{text}");

    // `..` is selected first; move to README.md and open it.
    state.handle_input_bytes(b"jj");
    let read = only_request(&state.handle_input_bytes(b"\r"));
    let Method::FileRead(params) = &read.method else {
        panic!("enter on a file should read it, got {:?}", read.method);
    };
    assert_eq!(params.path, "/repo/README.md");

    state.handle_endpoint_result(
        "boot-1",
        &read.id,
        Ok(content("/repo/README.md", "# Title\nfirst line\n")),
    );
    let text = screen(&mut state);
    assert!(text.contains("/repo/README.md"), "{text}");
    assert!(text.contains("# Title"), "{text}");
    assert!(text.contains("2  first line"), "{text}");

    // Esc goes back to the list, a second Esc closes the viewer.
    state.handle_input_bytes(b"\x1b");
    assert!(screen(&mut state).contains("▸ src/"));
    state.handle_input_bytes(b"\x1b");
    assert!(state.overlay.is_none());
}

#[test]
fn file_viewer_enters_directories_and_reselects_them_on_the_way_up() {
    let mut state = open_state();
    let list = open_viewer(&mut state);
    state.handle_endpoint_result(
        "boot-1",
        &list.id,
        Ok(listing(
            "/repo",
            Some("/"),
            vec![
                entry("docs", FileEntryKind::Directory, 0),
                entry("src", FileEntryKind::Directory, 0),
            ],
        )),
    );
    state.handle_input_bytes(b"jj");
    let into_src = only_request(&state.handle_input_bytes(b"l"));
    assert!(matches!(&into_src.method, Method::FileList(params) if params.path == "/repo/src"));
    state.handle_endpoint_result(
        "boot-1",
        &into_src.id,
        Ok(listing("/repo/src", Some("/repo"), vec![])),
    );

    let up = only_request(&state.handle_input_bytes(b"h"));
    assert!(matches!(&up.method, Method::FileList(params) if params.path == "/repo"));
    state.handle_endpoint_result(
        "boot-1",
        &up.id,
        Ok(listing(
            "/repo",
            Some("/"),
            vec![
                entry("docs", FileEntryKind::Directory, 0),
                entry("src", FileEntryKind::Directory, 0),
            ],
        )),
    );
    let Some(ClientShellOverlay::FileViewer(viewer)) = &state.overlay else {
        panic!("viewer should stay open");
    };
    // Rows are [.., docs, src]; the directory we came from is selected again.
    assert_eq!(viewer.selected, 2);
}

#[test]
fn file_viewer_filter_narrows_rows_and_stale_responses_are_ignored() {
    let mut state = open_state();
    let first = open_viewer(&mut state);
    // Reload before the first listing arrives: the first response is now stale.
    let reload = only_request(&state.handle_input_bytes(b"r"));
    let (repaint, _) = state.handle_endpoint_result(
        "boot-1",
        &first.id,
        Ok(listing(
            "/stale",
            None,
            vec![entry("old", FileEntryKind::File, 1)],
        )),
    );
    assert!(!repaint);
    state.handle_endpoint_result(
        "boot-1",
        &reload.id,
        Ok(listing(
            "/repo",
            Some("/"),
            vec![
                entry("alpha.md", FileEntryKind::File, 1),
                entry("beta.rs", FileEntryKind::File, 1),
            ],
        )),
    );
    state.handle_input_bytes(b"/");
    state.handle_input_bytes(b"BETA");
    let text = screen(&mut state);
    assert!(text.contains("beta.rs"), "{text}");
    assert!(!text.contains("alpha.md"), "{text}");
    assert!(!text.contains("old"), "{text}");
}

#[test]
fn file_viewer_shows_errors_inline_and_on_servers_without_file_methods() {
    let mut state = open_state();
    let list = open_viewer(&mut state);
    state.handle_endpoint_result(
        "boot-1",
        &list.id,
        Err(ClientShellEndpointError {
            code: Some("permission_denied".into()),
            message: "/repo: Permission denied".into(),
        }),
    );
    assert!(screen(&mut state).contains("⚠ /repo: Permission denied"));
    assert!(state.visible_endpoint_notice.is_none());
    state.handle_input_bytes(b"\x1b");

    state.set_endpoint_methods(Some(vec!["worktree.list".into()]));
    let mut open = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::OpenFileViewer),
        &mut open,
    );
    assert!(open.actions.is_empty());
    assert!(screen(&mut state).contains("this server cannot file.list"));
}

#[test]
fn file_viewer_scrolls_long_documents_and_clamps_at_the_end() {
    let mut state = open_state();
    let list = open_viewer(&mut state);
    state.handle_endpoint_result(
        "boot-1",
        &list.id,
        Ok(listing(
            "/repo",
            None,
            vec![entry("long.txt", FileEntryKind::File, 1)],
        )),
    );
    let read = only_request(&state.handle_input_bytes(b"\r"));
    let text: String = (1..=200).map(|n| format!("line {n}\n")).collect();
    state.handle_endpoint_result("boot-1", &read.id, Ok(content("/repo/long.txt", &text)));
    assert!(screen(&mut state).contains("line 1 "));

    state.handle_input_bytes(b"G");
    let bottom = screen(&mut state);
    assert!(bottom.contains("line 200"), "{bottom}");
    let Some(ClientShellOverlay::FileViewer(viewer)) = &state.overlay else {
        panic!("viewer open");
    };
    let scroll = viewer.document.as_ref().unwrap().scroll;
    assert!(
        scroll < 200,
        "scroll must be clamped to the last page, got {scroll}"
    );

    state.handle_input_bytes(b"g");
    assert!(screen(&mut state).contains("line 1 "));
}
