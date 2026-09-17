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
