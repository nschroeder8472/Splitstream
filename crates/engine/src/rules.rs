//! Process/session → `GroupId` matching (session-routing L4). Pure — no OS,
//! no I/O — unit-testable without any session-enumeration port. Lives in
//! `engine`, not `control` as the L4 text literally said: `control` depends
//! on `engine` (not the reverse), and `engine::routing::RoutingCoordinator`'s
//! own contract takes `GroupRules` as a parameter — same "type home" fix as
//! `ConfigSnapshot`/`GroupConfig` (`.lattice/context/engine-core.md`). See
//! `.lattice/context/session-routing.md`, 2026-07-20 decision.

use std::path::PathBuf;

use audio_core::GroupId;

/// `*` = any run of characters (including none), `?` = exactly one character.
/// Matching is case-insensitive (Windows process names/paths aren't
/// case-sensitive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobPattern(String);

impl GlobPattern {
    pub fn new(pattern: impl Into<String>) -> GlobPattern {
        GlobPattern(pattern.into())
    }

    pub fn matches(&self, s: &str) -> bool {
        wildcard_match(&self.0.to_ascii_lowercase(), &s.to_ascii_lowercase())
    }
}

/// Classic greedy two-pointer wildcard match, backtracking to the last `*`
/// on a mismatch. O(n*m) worst case, fine for process-name-length inputs.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut match_from = 0usize;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            match_from = ti;
            pi += 1;
        } else if let Some(si) = star {
            pi = si + 1;
            match_from += 1;
            ti = match_from;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchRule {
    ExactName(String),
    Glob(GlobPattern),
}

impl MatchRule {
    /// `*`/`?` present → glob; otherwise an exact (case-insensitive) name.
    pub fn parse(raw: &str) -> MatchRule {
        if raw.contains('*') || raw.contains('?') {
            MatchRule::Glob(GlobPattern::new(raw))
        } else {
            MatchRule::ExactName(raw.to_string())
        }
    }
}

pub struct GroupRules {
    pub group: GroupId,
    pub rules: Vec<MatchRule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub pid: u32,
    pub process_path: PathBuf,
    pub display_name: String,
}

/// Matches against the process image name and the full path (spec §15.5) —
/// not `display_name` (window title was explicitly dropped: volatile,
/// rematch churn on retitle). Exact-name rules win over glob rules as a
/// class; ties within a tier go to the first group in config order.
pub fn match_session(info: &SessionInfo, rules: &[GroupRules]) -> Option<GroupId> {
    let file_name = info
        .process_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let full_path = info.process_path.to_str().unwrap_or("");

    let exact = |group_rules: &&GroupRules| {
        group_rules.rules.iter().any(|r| match r {
            MatchRule::ExactName(name) => {
                name.eq_ignore_ascii_case(file_name) || name.eq_ignore_ascii_case(full_path)
            }
            MatchRule::Glob(_) => false,
        })
    };
    if let Some(gr) = rules.iter().find(exact) {
        return Some(gr.group);
    }

    let glob = |group_rules: &&GroupRules| {
        group_rules.rules.iter().any(|r| match r {
            MatchRule::Glob(pattern) => pattern.matches(file_name) || pattern.matches(full_path),
            MatchRule::ExactName(_) => false,
        })
    };
    rules.iter().find(glob).map(|gr| gr.group)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(path: &str) -> SessionInfo {
        SessionInfo {
            pid: 1234,
            process_path: PathBuf::from(path),
            display_name: "irrelevant — display_name is not a match target".into(),
        }
    }

    fn exact(group: u16, name: &str) -> GroupRules {
        GroupRules {
            group: GroupId(group),
            rules: vec![MatchRule::ExactName(name.into())],
        }
    }

    fn glob(group: u16, pattern: &str) -> GroupRules {
        GroupRules {
            group: GroupId(group),
            rules: vec![MatchRule::Glob(GlobPattern::new(pattern))],
        }
    }

    #[test]
    fn glob_star_matches_any_run_of_characters() {
        assert!(GlobPattern::new("game*.exe").matches("game64.exe"));
        assert!(GlobPattern::new("game*.exe").matches("game.exe"));
        assert!(!GlobPattern::new("game*.exe").matches("other.exe"));
    }

    #[test]
    fn glob_question_mark_matches_exactly_one_character() {
        assert!(GlobPattern::new("app?.exe").matches("app1.exe"));
        assert!(!GlobPattern::new("app?.exe").matches("app12.exe"));
        assert!(!GlobPattern::new("app?.exe").matches("app.exe"));
    }

    #[test]
    fn glob_matching_is_case_insensitive() {
        assert!(GlobPattern::new("Game*.EXE").matches("game64.exe"));
    }

    #[test]
    fn parse_detects_wildcard_characters() {
        assert_eq!(
            MatchRule::parse("game.exe"),
            MatchRule::ExactName("game.exe".into())
        );
        assert_eq!(
            MatchRule::parse("game*.exe"),
            MatchRule::Glob(GlobPattern::new("game*.exe"))
        );
    }

    #[test]
    fn matches_process_image_name() {
        let rules = vec![exact(0, "game.exe")];
        let info = session(r"C:\Games\game.exe");
        assert_eq!(match_session(&info, &rules), Some(GroupId(0)));
    }

    #[test]
    fn matches_full_path() {
        let rules = vec![exact(0, r"C:\Games\game.exe")];
        let info = session(r"C:\Games\game.exe");
        assert_eq!(match_session(&info, &rules), Some(GroupId(0)));
    }

    #[test]
    fn no_matching_rule_returns_none() {
        let rules = vec![exact(0, "other.exe")];
        let info = session(r"C:\Games\game.exe");
        assert_eq!(match_session(&info, &rules), None);
    }

    #[test]
    fn matching_is_case_insensitive() {
        let rules = vec![exact(0, "GAME.EXE")];
        let info = session(r"C:\Games\game.exe");
        assert_eq!(match_session(&info, &rules), Some(GroupId(0)));
    }

    #[test]
    fn exact_name_beats_glob_even_when_the_glob_groups_config_order_is_earlier() {
        let rules = vec![glob(0, "game*.exe"), exact(1, "game.exe")];
        let info = session(r"C:\Games\game.exe");
        assert_eq!(match_session(&info, &rules), Some(GroupId(1)));
    }

    #[test]
    fn ties_within_the_same_tier_pick_the_first_group_in_config_order() {
        let rules = vec![exact(0, "game.exe"), exact(1, "game.exe")];
        let info = session(r"C:\Games\game.exe");
        assert_eq!(match_session(&info, &rules), Some(GroupId(0)));
    }

    #[test]
    fn display_name_is_never_a_match_target() {
        let rules = vec![exact(0, "irrelevant — display_name is not a match target")];
        let info = session(r"C:\Games\game.exe");
        assert_eq!(match_session(&info, &rules), None);
    }
}
