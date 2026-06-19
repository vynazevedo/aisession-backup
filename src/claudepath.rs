//! Canonical encoding of a working directory into a Claude Code project
//! directory name.
//!
//! Claude Code stores each session under `~/.claude/projects/<encoded>/`,
//! where `<encoded>` is derived from the absolute `cwd` by replacing every
//! character that is not ASCII alphanumeric with a single `-`. Repeated
//! separators are NOT collapsed, so `/home/u/.config` becomes
//! `-home-u--config` (the `/` and the `.` each map to their own `-`).
//!
//! This transformation is lossy and therefore intentionally has no inverse:
//! `alianca-divergente` could have come from `alianca/divergente`,
//! `alianca_divergente`, `alianca.divergente` or `alianca-divergente`. The
//! only reliable source of a session's true `cwd` is the `cwd` field stored
//! inside the session JSONL. Never attempt to decode a directory name.

/// Encodes an absolute path into the Claude Code project directory name.
pub fn encode(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Reports whether two encoded names collide on a case-insensitive
/// filesystem (macOS APFS default, Windows). Used to warn before a restore
/// would silently overwrite a distinct session on such targets.
pub fn case_insensitive_eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_becomes_dash() {
        assert_eq!(encode("/home/user/project"), "-home-user-project");
    }

    #[test]
    fn underscore_collapses_to_dash() {
        assert_eq!(
            encode("/mnt/e/workspace/alianca_divergente"),
            "-mnt-e-workspace-alianca-divergente"
        );
    }

    #[test]
    fn dot_and_slash_each_map_to_their_own_dash() {
        assert_eq!(
            encode("/home/n00b/.claude-mem/observer-sessions"),
            "-home-n00b--claude-mem-observer-sessions"
        );
    }

    #[test]
    fn existing_dash_is_preserved_as_dash() {
        assert_eq!(encode("/x/claude-migrate"), "-x-claude-migrate");
    }

    #[test]
    fn separators_are_not_collapsed() {
        assert_eq!(encode("/a//b"), "-a--b");
    }

    #[test]
    fn spaces_and_specials_become_dash() {
        assert_eq!(encode("/Users/jo ao/My App"), "-Users-jo-ao-My-App");
    }

    #[test]
    fn case_is_preserved() {
        assert_eq!(encode("/Users/John/App"), "-Users-John-App");
    }

    #[test]
    fn case_insensitive_collision_detected() {
        assert!(case_insensitive_eq("-users-john-app", "-Users-John-App"));
        assert!(!case_insensitive_eq("-users-john-app", "-users-jane-app"));
    }
}
