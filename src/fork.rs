//! Identity of the hpp fork ("herdr++").
//!
//! hpp is a personal fork of herdr that installs alongside stock herdr. Everything that
//! must differ between the two installs is derived from these constants: the executable
//! name, the per-user config/state directory name, and the version label.

/// Executable name of the fork.
pub const BIN_NAME: &str = "hpp";

/// Fork revision, bumped for each fork release on top of the same upstream version.
pub const FORK_REVISION: u32 = 1;

/// Per-user application directory name (config, state, sessions, sockets, logs).
/// Debug builds use a separate directory, as upstream does.
pub fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "hpp-dev"
    } else {
        "hpp"
    }
}

/// Text printed by `hpp --version`, e.g. `hpp 0.9.1+hpp.1`.
pub fn version_label() -> String {
    format!(
        "{BIN_NAME} {}+hpp.{FORK_REVISION}",
        crate::build_info::version()
    )
}

/// Agent-facing description of the `hc:` inline comment format (`hpp protocol`).
pub const HC_PROTOCOL: &str = include_str!("../skills/hpp-review-comments/hc-comments.md");

/// Message used wherever upstream would download or self-install a herdr.dev release.
pub fn no_published_releases_message() -> String {
    format!(
        "{BIN_NAME} is a source-built fork of herdr with no published releases; \
         rebuild it from source instead"
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_label_names_the_fork() {
        let label = super::version_label();
        assert!(label.starts_with("hpp "));
        assert!(label.contains("+hpp."));
    }

    #[test]
    fn protocol_documents_every_directive() {
        for word in [
            "directive",
            "reply",
            "fix",
            "discuss",
            "hc:body",
            "reply-to",
        ] {
            assert!(super::HC_PROTOCOL.contains(word), "{word}");
        }
    }

    #[test]
    fn app_dir_is_separate_from_stock_herdr() {
        assert!(super::app_dir_name().starts_with("hpp"));
    }
}
