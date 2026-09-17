//! Server-side handlers for the file access API methods.
//!
//! Filesystem work can block for a long time (network mounts, FUSE), so requests are served
//! on a short-lived worker thread and answered through the API responder channel, keeping the
//! server's main loop free.

use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::sync::mpsc::Sender;

use sha2::{Digest, Sha256};

use crate::api::schema::{
    ErrorBody, ErrorResponse, FileContentInfo, FileEntryInfo, FileEntryKind, FileListParams,
    FileReadParams, FileWriteInfo, FileWriteParams, GitDiffInfo, GitDiffParams, Method, Request,
    ResponseResult, SuccessResponse, FILE_LIST_MAX_ENTRIES, FILE_READ_DEFAULT_MAX_BYTES,
    FILE_READ_MAX_BYTES_LIMIT, FILE_WRITE_MAX_BYTES, GIT_DIFF_MAX_BYTES,
};

/// Bytes inspected for a NUL byte when deciding whether a file is binary.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// Whether `method` is served by this module.
pub(crate) fn is_file_method(method: &Method) -> bool {
    matches!(
        method,
        Method::FileList(_) | Method::FileRead(_) | Method::FileWrite(_) | Method::GitDiff(_)
    )
}

/// Serve a file access request on a worker thread and send the JSON response to `respond_to`.
pub(crate) fn spawn_request(request: Request, respond_to: Sender<String>) {
    let fallback = respond_to.clone();
    let id = request.id.clone();
    let spawned = std::thread::Builder::new()
        .name("file-access".into())
        .spawn(move || {
            let _ = respond_to.send(handle_request(request));
        });
    if let Err(err) = spawned {
        tracing::warn!(err = %err, "failed to spawn file access worker");
        let _ = fallback.send(encode_error(
            id,
            "io_error",
            format!("file access failed: {err}"),
        ));
    }
}

/// Handle a file access request synchronously and return the JSON response.
pub(crate) fn handle_request(request: Request) -> String {
    let id = request.id;
    let result = match request.method {
        Method::FileList(params) => list_dir(&params),
        Method::FileRead(params) => read_file(&params),
        Method::FileWrite(params) => write_file(&params),
        Method::GitDiff(params) => git_diff(&params),
        _ => Err(FileError::new(
            "invalid_request",
            "not a file access method".to_owned(),
        )),
    };
    match result {
        Ok(result) => encode_success(id, result),
        Err(err) => encode_error(id, err.code, err.message),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FileError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl FileError {
    fn new(code: &'static str, message: String) -> Self {
        Self { code, message }
    }

    fn io(path: &str, err: &io::Error) -> Self {
        let code = match err.kind() {
            io::ErrorKind::NotFound => "not_found",
            io::ErrorKind::PermissionDenied => "permission_denied",
            _ => "io_error",
        };
        Self::new(code, format!("{path}: {err}"))
    }
}

fn require_absolute(path: &str) -> Result<&Path, FileError> {
    let candidate = Path::new(path);
    if path.is_empty() || !candidate.is_absolute() {
        return Err(FileError::new(
            "invalid_path",
            format!("path must be absolute: {path:?}"),
        ));
    }
    Ok(candidate)
}

pub(crate) fn list_dir(params: &FileListParams) -> Result<ResponseResult, FileError> {
    let requested = require_absolute(&params.path)?;
    let dir = fs::canonicalize(requested).map_err(|err| FileError::io(&params.path, &err))?;
    let metadata = fs::metadata(&dir).map_err(|err| FileError::io(&params.path, &err))?;
    if !metadata.is_dir() {
        return Err(FileError::new(
            "not_a_directory",
            format!("{}: not a directory", params.path),
        ));
    }
    let reader = fs::read_dir(&dir).map_err(|err| FileError::io(&params.path, &err))?;
    let mut entries = Vec::new();
    for entry in reader.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !params.show_hidden && name.starts_with('.') {
            continue;
        }
        entries.push(entry_info(&entry, name));
    }
    entries.sort_by(|a, b| {
        let a_dir = entry_is_dir(a);
        let b_dir = entry_is_dir(b);
        b_dir
            .cmp(&a_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    let truncated = entries.len() > FILE_LIST_MAX_ENTRIES;
    entries.truncate(FILE_LIST_MAX_ENTRIES);
    Ok(ResponseResult::FileList {
        path: dir.to_string_lossy().into_owned(),
        parent: dir
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned()),
        entries,
        truncated,
    })
}

/// Directories and symlinks to directories sort first and open as directories.
pub(crate) fn entry_is_dir(entry: &FileEntryInfo) -> bool {
    entry.kind == FileEntryKind::Directory
        || (entry.kind == FileEntryKind::Symlink && entry.target_is_dir)
}

fn entry_info(entry: &fs::DirEntry, name: String) -> FileEntryInfo {
    let Ok(file_type) = entry.file_type() else {
        return FileEntryInfo {
            name,
            kind: FileEntryKind::Unknown,
            size: 0,
            target_is_dir: false,
        };
    };
    if file_type.is_symlink() {
        let target = fs::metadata(entry.path()).ok();
        return FileEntryInfo {
            name,
            kind: FileEntryKind::Symlink,
            size: target
                .as_ref()
                .filter(|meta| meta.is_file())
                .map_or(0, fs::Metadata::len),
            target_is_dir: target.is_some_and(|meta| meta.is_dir()),
        };
    }
    let (kind, size) = if file_type.is_dir() {
        (FileEntryKind::Directory, 0)
    } else if file_type.is_file() {
        (
            FileEntryKind::File,
            entry.metadata().map_or(0, |meta| meta.len()),
        )
    } else {
        (FileEntryKind::Other, 0)
    };
    FileEntryInfo {
        name,
        kind,
        size,
        target_is_dir: false,
    }
}

pub(crate) fn read_file(params: &FileReadParams) -> Result<ResponseResult, FileError> {
    let requested = require_absolute(&params.path)?;
    let path = fs::canonicalize(requested).map_err(|err| FileError::io(&params.path, &err))?;
    let metadata = fs::metadata(&path).map_err(|err| FileError::io(&params.path, &err))?;
    if metadata.is_dir() {
        return Err(FileError::new(
            "is_a_directory",
            format!("{}: is a directory", params.path),
        ));
    }
    if !metadata.is_file() {
        return Err(FileError::new(
            "not_a_file",
            format!("{}: not a regular file", params.path),
        ));
    }
    let max_bytes = params
        .max_bytes
        .unwrap_or(FILE_READ_DEFAULT_MAX_BYTES)
        .min(FILE_READ_MAX_BYTES_LIMIT);
    let file = fs::File::open(&path).map_err(|err| FileError::io(&params.path, &err))?;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|err| FileError::io(&params.path, &err))?;
    let limit = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    let size = metadata.len().max(bytes.len() as u64);
    Ok(ResponseResult::FileContent {
        file: content_info(path.to_string_lossy().into_owned(), size, bytes, truncated),
    })
}

fn content_info(path: String, size: u64, mut bytes: Vec<u8>, truncated: bool) -> FileContentInfo {
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let sniff = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
    if sniff.contains(&0) {
        return FileContentInfo {
            path,
            size,
            truncated,
            binary: true,
            lossy: false,
            text: None,
            sha256,
        };
    }
    if truncated {
        // A cut can split a multi-byte character; drop the partial tail rather than
        // reporting the whole file as lossy.
        if let Err(err) = std::str::from_utf8(&bytes) {
            if err.error_len().is_none() {
                bytes.truncate(err.valid_up_to());
            }
        }
    }
    let (text, lossy) = match String::from_utf8(bytes) {
        Ok(text) => (text, false),
        Err(err) => (String::from_utf8_lossy(err.as_bytes()).into_owned(), true),
    };
    FileContentInfo {
        path,
        size,
        truncated,
        binary: false,
        lossy,
        text: Some(text),
        sha256,
    }
}

pub(crate) fn write_file(params: &FileWriteParams) -> Result<ResponseResult, FileError> {
    let requested = require_absolute(&params.path)?;
    if params.text.len() > FILE_WRITE_MAX_BYTES {
        return Err(FileError::new(
            "too_large",
            format!(
                "{}: {} bytes exceeds the {} byte write limit",
                params.path,
                params.text.len(),
                FILE_WRITE_MAX_BYTES
            ),
        ));
    }
    let existing = match fs::canonicalize(requested) {
        Ok(path) => Some(path),
        Err(err) if err.kind() == io::ErrorKind::NotFound && params.create => None,
        Err(err) => return Err(FileError::io(&params.path, &err)),
    };
    let target = match &existing {
        Some(path) => {
            let metadata = fs::metadata(path).map_err(|err| FileError::io(&params.path, &err))?;
            if !metadata.is_file() {
                return Err(FileError::new(
                    "not_a_file",
                    format!("{}: not a regular file", params.path),
                ));
            }
            if let Some(expected) = &params.expected_sha256 {
                let current = fs::read(path).map_err(|err| FileError::io(&params.path, &err))?;
                let current = format!("{:x}", Sha256::digest(&current));
                if !current.eq_ignore_ascii_case(expected.trim()) {
                    return Err(FileError::new(
                        "stale_content",
                        format!("{} changed since it was read", params.path),
                    ));
                }
            }
            path.clone()
        }
        None => {
            if params.expected_sha256.is_some() {
                return Err(FileError::new(
                    "stale_content",
                    format!("{} no longer exists", params.path),
                ));
            }
            let parent = requested.parent().ok_or_else(|| {
                FileError::new(
                    "invalid_path",
                    format!("{}: no parent directory", params.path),
                )
            })?;
            let parent =
                fs::canonicalize(parent).map_err(|err| FileError::io(&params.path, &err))?;
            match requested.file_name() {
                Some(name) => parent.join(name),
                None => {
                    return Err(FileError::new(
                        "invalid_path",
                        format!("{}: no file name", params.path),
                    ))
                }
            }
        }
    };
    replace_atomically(&target, params.text.as_bytes(), existing.is_some())
        .map_err(|err| FileError::io(&params.path, &err))?;
    Ok(ResponseResult::FileWritten {
        file: FileWriteInfo {
            path: target.to_string_lossy().into_owned(),
            size: params.text.len() as u64,
            sha256: format!("{:x}", Sha256::digest(params.text.as_bytes())),
        },
    })
}

/// Write `bytes` to a temporary sibling, flush it, copy the original permissions, then rename it
/// over `target`, so readers never observe a partially written file.
fn replace_atomically(target: &Path, bytes: &[u8], preserve_permissions: bool) -> io::Result<()> {
    use std::io::Write;

    let dir = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"))?;
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let temp = dir.join(format!(".{name}.hpp-write-{}-{nanos}", std::process::id()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if preserve_permissions {
            if let Ok(metadata) = fs::metadata(target) {
                fs::set_permissions(&temp, metadata.permissions())?;
            }
        }
        fs::rename(&temp, target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

pub(crate) fn git_diff(params: &GitDiffParams) -> Result<ResponseResult, FileError> {
    let requested = require_absolute(&params.path)?;
    let path = fs::canonicalize(requested).map_err(|err| FileError::io(&params.path, &err))?;
    let dir = if path.is_dir() {
        path.clone()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.clone())
    };
    let toplevel = git_output(&dir, &["rev-parse", "--show-toplevel"]).map_err(|message| {
        FileError::new(
            "not_a_git_repository",
            format!("{}: {message}", params.path),
        )
    })?;
    let repo_root = toplevel.trim_end().to_owned();
    let mut args = vec!["diff", "--no-color", "--no-ext-diff"];
    if params.staged {
        args.push("--cached");
    } else {
        args.push("HEAD");
    }
    let path_arg = path.to_string_lossy().into_owned();
    args.push("--");
    args.push(&path_arg);
    let mut text = match git_output(&dir, &args) {
        Ok(text) => text,
        // A repository without commits has no HEAD; fall back to the index diff.
        Err(_) if !params.staged => git_output(
            &dir,
            &["diff", "--no-color", "--no-ext-diff", "--", &path_arg],
        )
        .map_err(|message| FileError::new("git_error", message))?,
        Err(message) => return Err(FileError::new("git_error", message)),
    };
    let truncated = text.len() > GIT_DIFF_MAX_BYTES;
    if truncated {
        let mut cut = GIT_DIFF_MAX_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    Ok(ResponseResult::GitDiff {
        diff: GitDiffInfo {
            path: path.to_string_lossy().into_owned(),
            repo_root,
            text,
            truncated,
        },
    })
}

fn git_output(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = crate::noninteractive_process::command("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|err| format!("failed to run git: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr
            .trim()
            .lines()
            .last()
            .unwrap_or("git failed")
            .to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn encode_success(id: String, result: ResponseResult) -> String {
    serde_json::to_string(&SuccessResponse { id, result }).unwrap_or_else(|_| "{}".to_owned())
}

fn encode_error(id: String, code: &str, message: String) -> String {
    serde_json::to_string(&ErrorResponse {
        id,
        error: ErrorBody {
            code: code.to_owned(),
            message,
        },
    })
    .unwrap_or_else(|_| "{}".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "hpp-file-access-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }

        fn path(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn listed(result: ResponseResult) -> (String, Option<String>, Vec<FileEntryInfo>, bool) {
        match result {
            ResponseResult::FileList {
                path,
                parent,
                entries,
                truncated,
            } => (path, parent, entries, truncated),
            other => panic!("expected file list, got {other:?}"),
        }
    }

    fn content(result: ResponseResult) -> FileContentInfo {
        match result {
            ResponseResult::FileContent { file } => file,
            other => panic!("expected file content, got {other:?}"),
        }
    }

    #[test]
    fn list_sorts_directories_first_and_hides_dotfiles() {
        let dir = TempDir::new("list");
        fs::create_dir(dir.0.join("zeta")).unwrap();
        fs::write(dir.0.join("Alpha.md"), b"a").unwrap();
        fs::write(dir.0.join("beta.txt"), b"bb").unwrap();
        fs::write(dir.0.join(".hidden"), b"h").unwrap();

        let (path, parent, entries, truncated) = listed(
            list_dir(&FileListParams {
                path: dir.0.to_string_lossy().into_owned(),
                show_hidden: false,
            })
            .unwrap(),
        );
        assert_eq!(path, dir.0.to_string_lossy());
        assert!(parent.is_some());
        assert!(!truncated);
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["zeta", "Alpha.md", "beta.txt"]);
        assert_eq!(entries[0].kind, FileEntryKind::Directory);
        assert_eq!(entries[2].size, 2);

        let (_, _, entries, _) = listed(
            list_dir(&FileListParams {
                path: dir.0.to_string_lossy().into_owned(),
                show_hidden: true,
            })
            .unwrap(),
        );
        assert!(entries.iter().any(|entry| entry.name == ".hidden"));
    }

    #[test]
    fn list_rejects_relative_missing_and_file_paths() {
        let dir = TempDir::new("list-errors");
        fs::write(dir.0.join("file"), b"x").unwrap();
        let err = |path: String| {
            list_dir(&FileListParams {
                path,
                show_hidden: false,
            })
            .unwrap_err()
            .code
        };
        assert_eq!(err("relative/dir".into()), "invalid_path");
        assert_eq!(err(dir.path("missing")), "not_found");
        assert_eq!(err(dir.path("file")), "not_a_directory");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_sort_and_open_as_directories() {
        let dir = TempDir::new("symlink");
        fs::create_dir(dir.0.join("real")).unwrap();
        fs::write(dir.0.join("a-file"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.0.join("real"), dir.0.join("b-link")).unwrap();
        let (_, _, entries, _) = listed(
            list_dir(&FileListParams {
                path: dir.0.to_string_lossy().into_owned(),
                show_hidden: false,
            })
            .unwrap(),
        );
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["b-link", "real", "a-file"]);
        assert!(entry_is_dir(&entries[0]));
    }

    #[test]
    fn read_returns_text_and_whole_file_hash() {
        let dir = TempDir::new("read");
        fs::write(dir.0.join("notes.md"), "# title\nbody ✓\n").unwrap();
        let file = content(
            read_file(&FileReadParams {
                path: dir.path("notes.md"),
                max_bytes: None,
            })
            .unwrap(),
        );
        assert_eq!(file.text.as_deref(), Some("# title\nbody ✓\n"));
        assert!(!file.truncated && !file.binary && !file.lossy);
        assert_eq!(file.size, "# title\nbody ✓\n".len() as u64);
        assert_eq!(
            file.sha256,
            format!("{:x}", Sha256::digest("# title\nbody ✓\n".as_bytes()))
        );
    }

    #[test]
    fn read_truncates_without_splitting_a_character() {
        let dir = TempDir::new("truncate");
        fs::write(dir.0.join("wide.txt"), "ab✓cd").unwrap();
        // "ab" is 2 bytes and ✓ is 3, so a 3-byte cut lands inside the character.
        let file = content(
            read_file(&FileReadParams {
                path: dir.path("wide.txt"),
                max_bytes: Some(3),
            })
            .unwrap(),
        );
        assert!(file.truncated);
        assert!(!file.lossy);
        assert_eq!(file.text.as_deref(), Some("ab"));
        assert_eq!(file.size, "ab✓cd".len() as u64);
    }

    #[test]
    fn read_flags_binary_and_invalid_utf8() {
        let dir = TempDir::new("binary");
        fs::write(dir.0.join("blob"), [1u8, 0, 2, 3]).unwrap();
        fs::write(dir.0.join("latin1"), [b'c', b'a', b'f', 0xe9]).unwrap();
        let blob = content(
            read_file(&FileReadParams {
                path: dir.path("blob"),
                max_bytes: None,
            })
            .unwrap(),
        );
        assert!(blob.binary);
        assert!(blob.text.is_none());
        let latin1 = content(
            read_file(&FileReadParams {
                path: dir.path("latin1"),
                max_bytes: None,
            })
            .unwrap(),
        );
        assert!(latin1.lossy);
        assert!(latin1.text.unwrap().starts_with("caf"));
    }

    #[test]
    fn read_rejects_directories_and_missing_files() {
        let dir = TempDir::new("read-errors");
        let err = |path: String| {
            read_file(&FileReadParams {
                path,
                max_bytes: None,
            })
            .unwrap_err()
            .code
        };
        assert_eq!(err(dir.0.to_string_lossy().into_owned()), "is_a_directory");
        assert_eq!(err(dir.path("missing")), "not_found");
        assert_eq!(err("notes.md".into()), "invalid_path");
    }

    fn sha(text: &str) -> String {
        format!("{:x}", Sha256::digest(text.as_bytes()))
    }

    #[test]
    fn write_replaces_content_when_the_hash_matches() {
        let dir = TempDir::new("write");
        fs::write(dir.0.join("doc.md"), "old\n").unwrap();
        let result = write_file(&FileWriteParams {
            path: dir.path("doc.md"),
            text: "new ✓\n".into(),
            expected_sha256: Some(sha("old\n")),
            create: false,
        })
        .unwrap();
        let ResponseResult::FileWritten { file } = result else {
            panic!("expected written result");
        };
        assert_eq!(file.sha256, sha("new ✓\n"));
        assert_eq!(fs::read_to_string(dir.0.join("doc.md")).unwrap(), "new ✓\n");
        let leftovers: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains("hpp-write"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files must not remain");
    }

    #[test]
    fn write_refuses_stale_content_and_leaves_the_file_alone() {
        let dir = TempDir::new("stale");
        fs::write(dir.0.join("doc.md"), "agent edited this\n").unwrap();
        let err = write_file(&FileWriteParams {
            path: dir.path("doc.md"),
            text: "human overwrite\n".into(),
            expected_sha256: Some(sha("what the human read\n")),
            create: false,
        })
        .unwrap_err();
        assert_eq!(err.code, "stale_content");
        assert_eq!(
            fs::read_to_string(dir.0.join("doc.md")).unwrap(),
            "agent edited this\n"
        );
    }

    #[test]
    fn write_creates_only_when_asked() {
        let dir = TempDir::new("create");
        let params = |create| FileWriteParams {
            path: dir.path("fresh.md"),
            text: "hello".into(),
            expected_sha256: None,
            create,
        };
        assert_eq!(write_file(&params(false)).unwrap_err().code, "not_found");
        write_file(&params(true)).unwrap();
        assert_eq!(fs::read_to_string(dir.0.join("fresh.md")).unwrap(), "hello");
        assert_eq!(
            write_file(&FileWriteParams {
                path: "relative.md".into(),
                text: String::new(),
                expected_sha256: None,
                create: true,
            })
            .unwrap_err()
            .code,
            "invalid_path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_preserves_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("perms");
        let path = dir.0.join("script.sh");
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
        write_file(&FileWriteParams {
            path: dir.path("script.sh"),
            text: "#!/bin/sh\necho hi\n".into(),
            expected_sha256: None,
            create: false,
        })
        .unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o750);
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn git_diff_reports_work_tree_and_staged_changes() {
        let dir = TempDir::new("git");
        git(&dir.0, &["init", "-q"]);
        fs::write(dir.0.join("a.md"), "one\n").unwrap();
        git(&dir.0, &["add", "a.md"]);
        git(&dir.0, &["commit", "-q", "-m", "init"]);
        fs::write(dir.0.join("a.md"), "one\ntwo\n").unwrap();

        let ResponseResult::GitDiff { diff } = git_diff(&GitDiffParams {
            path: dir.path("a.md"),
            staged: false,
        })
        .unwrap() else {
            panic!("expected diff");
        };
        assert_eq!(diff.repo_root, dir.0.to_string_lossy());
        assert!(diff.text.contains("+two"), "{}", diff.text);

        let ResponseResult::GitDiff { diff } = git_diff(&GitDiffParams {
            path: dir.path("a.md"),
            staged: true,
        })
        .unwrap() else {
            panic!("expected diff");
        };
        assert!(diff.text.is_empty(), "nothing staged yet: {}", diff.text);
    }

    #[test]
    fn git_diff_outside_a_repository_is_a_clear_error() {
        let dir = TempDir::new("nogit");
        fs::write(dir.0.join("a.md"), "x").unwrap();
        let err = git_diff(&GitDiffParams {
            path: dir.path("a.md"),
            staged: false,
        })
        .unwrap_err();
        assert_eq!(err.code, "not_a_git_repository");
    }

    #[test]
    fn handle_request_encodes_success_and_error_envelopes() {
        let dir = TempDir::new("envelope");
        fs::write(dir.0.join("a.txt"), b"hi").unwrap();
        let ok = handle_request(Request {
            id: "r1".into(),
            method: Method::FileRead(FileReadParams {
                path: dir.path("a.txt"),
                max_bytes: None,
            }),
        });
        let ok: serde_json::Value = serde_json::from_str(&ok).unwrap();
        assert_eq!(ok["id"], "r1");
        assert_eq!(ok["result"]["type"], "file_content");
        assert_eq!(ok["result"]["file"]["text"], "hi");

        let err = handle_request(Request {
            id: "r2".into(),
            method: Method::FileList(FileListParams {
                path: dir.path("missing"),
                show_hidden: false,
            }),
        });
        let err: serde_json::Value = serde_json::from_str(&err).unwrap();
        assert_eq!(err["error"]["code"], "not_found");
    }
}
