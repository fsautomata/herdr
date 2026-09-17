//! File access methods (`file.list`, `file.read`).
//!
//! These run on the server host, so a client attached over SSH browses the files next to its
//! panes rather than its own filesystem.

use serde::{Deserialize, Serialize};

/// Default cap on bytes returned by `file.read`.
pub const FILE_READ_DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024;
/// Hard cap on bytes returned by `file.read`, whatever the request asks for.
pub const FILE_READ_MAX_BYTES_LIMIT: u64 = 8 * 1024 * 1024;
/// Maximum number of entries returned by `file.list`.
pub const FILE_LIST_MAX_ENTRIES: usize = 5000;
/// Largest text accepted by `file.write`.
pub const FILE_WRITE_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Largest diff returned by `git.diff`.
pub const GIT_DIFF_MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct FileListParams {
    /// Absolute path of the directory to list.
    pub path: String,
    /// Include entries whose name starts with a dot.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub show_hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct FileReadParams {
    /// Absolute path of the file to read.
    pub path: String,
    /// Maximum bytes to return; defaults to 2 MiB and is capped at 8 MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileEntryKind {
    File,
    Directory,
    Symlink,
    Other,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FileEntryInfo {
    pub name: String,
    pub kind: FileEntryKind,
    /// Size in bytes for files (and symlinks to files).
    #[serde(default)]
    pub size: u64,
    /// For symlinks: the target is a directory.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub target_is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FileContentInfo {
    /// Canonical absolute path that was read.
    pub path: String,
    /// Size of the whole file in bytes.
    pub size: u64,
    /// Only the first `max_bytes` were returned.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub truncated: bool,
    /// The file looks binary (a NUL byte in its first 8 KiB); no text is returned.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub binary: bool,
    /// The bytes were not valid UTF-8 and were decoded lossily.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub lossy: bool,
    /// Text content; absent for binary files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Lowercase hex SHA-256 of the returned bytes (the whole file unless truncated).
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct FileWriteParams {
    /// Absolute path of the file to write.
    pub path: String,
    /// Full new UTF-8 content of the file.
    pub text: String,
    /// Only write if the file's current SHA-256 (lowercase hex) matches; otherwise fail with
    /// `stale_content` so the caller can re-read and retry. Omit to overwrite unconditionally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
    /// Create the file when it does not exist.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub create: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FileWriteInfo {
    /// Canonical absolute path that was written.
    pub path: String,
    pub size: u64,
    /// Lowercase hex SHA-256 of the written content.
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct GitDiffParams {
    /// Absolute path of a file or directory inside a git work tree.
    pub path: String,
    /// Diff the staged changes (index vs HEAD) instead of the work tree vs HEAD.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub staged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitDiffInfo {
    /// Canonical absolute path that was diffed.
    pub path: String,
    /// Top level of the git work tree.
    pub repo_root: String,
    /// Unified diff text (empty when there are no changes).
    pub text: String,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub truncated: bool,
}
