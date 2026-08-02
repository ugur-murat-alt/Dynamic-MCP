use std::{collections::HashSet, fs, path::Path};

use serde::Deserialize;
use thiserror::Error;

// ---------------------------------------------------------------------------
// public types
// ---------------------------------------------------------------------------

/// An action that can be restricted by a policy rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    Inspect,
    List,
    Connect,
    Disconnect,
    Refresh,
    Call,
    PackageInstall,
    AuthStart,
    AuthLogout,
    SkillRun,
}

impl PolicyAction {
    const fn allows_tool(&self) -> bool {
        matches!(self, Self::Call)
    }

    const fn allows_skill(&self) -> bool {
        matches!(self, Self::SkillRun)
    }
}

/// The outcome of a policy check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    Deny,
}

/// Errors produced when loading or validating a policy file.
#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("failed to read policy file")]
    Io(#[from] std::io::Error),
    #[error("failed to parse policy TOML: {0}")]
    Parse(String),
    #[error("invalid policy: {0}")]
    Validation(String),
}

/// The loaded, validated policy engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    default_effect: Effect,
    rules: Vec<PolicyRule>,
}

impl Default for Policy {
    fn default() -> Self {
        Self::allow_all()
    }
}

// ---------------------------------------------------------------------------
// deserialization helpers (private)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Effect {
    Allow,
    Deny,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    #[serde(default)]
    default: Option<Effect>,
    #[serde(default)]
    rules: Vec<PolicyRuleFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyRuleFile {
    id: String,
    action: PolicyAction,
    effect: Effect,
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    skill: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PolicyRule {
    #[allow(dead_code)]
    id: String,
    action: PolicyAction,
    effect: Effect,
    server_pattern: Option<String>,
    tool_pattern: Option<String>,
    skill_pattern: Option<String>,
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

impl Policy {
    /// Loads `policy.toml` from `config_dir`. When the file does not exist an
    /// allow-all policy is returned. Parse and validation errors are surfaced
    /// as typed, source-free [`PolicyError`] values.
    pub fn load_optional(config_dir: impl AsRef<Path>) -> Result<Self, PolicyError> {
        let path = config_dir.as_ref().join("policy.toml");
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::allow_all());
            }
            Err(e) => return Err(PolicyError::Io(e)),
        };

        let file: PolicyFile =
            toml::from_str(&content).map_err(|e| PolicyError::Parse(e.message().to_string()))?;

        let mut seen_ids = HashSet::new();
        let mut rules = Vec::with_capacity(file.rules.len());

        for r in file.rules {
            if r.id.is_empty() {
                return Err(PolicyError::Validation("rule id must not be empty".into()));
            }
            if !seen_ids.insert(r.id.clone()) {
                return Err(PolicyError::Validation(format!(
                    "duplicate rule id: {}",
                    r.id
                )));
            }
            if r.tool.is_some() && !r.action.allows_tool() {
                return Err(PolicyError::Validation(format!(
                    "rule '{}': tool pattern is only valid with action 'call'",
                    r.id
                )));
            }
            if r.skill.is_some() && !r.action.allows_skill() {
                return Err(PolicyError::Validation(format!(
                    "rule '{}': skill pattern is only valid with action 'skill_run'",
                    r.id
                )));
            }
            if r.server.is_some() && r.action.allows_skill() {
                return Err(PolicyError::Validation(format!(
                    "rule '{}': server pattern is not valid with action 'skill_run'",
                    r.id
                )));
            }
            rules.push(PolicyRule {
                id: r.id,
                action: r.action,
                effect: r.effect,
                server_pattern: r.server,
                tool_pattern: r.tool,
                skill_pattern: r.skill,
            });
        }

        let default_effect = file.default.unwrap_or(Effect::Allow);

        Ok(Self {
            default_effect,
            rules,
        })
    }

    /// Evaluates the policy for the given action, server, and optional tool.
    ///
    /// # Algorithm
    ///
    /// 1. Deny-matching rules are checked first. A single match produces
    ///    [`PolicyDecision::Deny`].
    /// 2. Allow-matching rules are checked next. A single match produces
    ///    [`PolicyDecision::Allow`].
    /// 3. When no rule matches, the `default` effect is returned.
    pub fn check(
        &self,
        action: PolicyAction,
        server_id: &str,
        tool_name: Option<&str>,
    ) -> PolicyDecision {
        self.check_context(action, Some(server_id), tool_name, None)
    }

    #[must_use]
    pub fn check_skill(&self, skill_id: &str) -> PolicyDecision {
        self.check_context(PolicyAction::SkillRun, None, None, Some(skill_id))
    }

    fn check_context(
        &self,
        action: PolicyAction,
        server_id: Option<&str>,
        tool_name: Option<&str>,
        skill_id: Option<&str>,
    ) -> PolicyDecision {
        for rule in &self.rules {
            if rule.effect == Effect::Deny
                && rule_matches(rule, action, server_id, tool_name, skill_id)
            {
                return PolicyDecision::Deny;
            }
        }

        // phase 2 – allow rules
        for rule in &self.rules {
            if rule.effect == Effect::Allow
                && rule_matches(rule, action, server_id, tool_name, skill_id)
            {
                return PolicyDecision::Allow;
            }
        }

        // phase 3 – default
        match self.default_effect {
            Effect::Allow => PolicyDecision::Allow,
            Effect::Deny => PolicyDecision::Deny,
        }
    }

    fn allow_all() -> Self {
        Self {
            default_effect: Effect::Allow,
            rules: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// matching
// ---------------------------------------------------------------------------

fn rule_matches(
    rule: &PolicyRule,
    action: PolicyAction,
    server_id: Option<&str>,
    tool_name: Option<&str>,
    skill_id: Option<&str>,
) -> bool {
    if rule.action != action {
        return false;
    }

    if let Some(ref pattern) = rule.server_pattern
        && !server_id.is_some_and(|server_id| glob_match(pattern, server_id))
    {
        return false;
    }

    if rule.action.allows_tool()
        && let Some(ref pattern) = rule.tool_pattern
    {
        return tool_name.is_some_and(|name| glob_match(pattern, name));
    }

    if rule.action.allows_skill()
        && let Some(ref pattern) = rule.skill_pattern
    {
        return skill_id.is_some_and(|skill_id| glob_match(pattern, skill_id));
    }

    true
}

/// Case-sensitive glob matching supporting `*` (any sequence of characters)
/// and `?` (exactly one character). No other meta-characters are recognised.
fn glob_match(pattern: &str, value: &str) -> bool {
    let p = pattern.as_bytes();
    let v = value.as_bytes();
    let (mut pi, mut vi) = (0usize, 0usize);
    let (mut star_pi, mut star_vi) = (None, 0);

    while vi < v.len() {
        if pi < p.len() && p[pi] == b'*' {
            star_pi = Some(pi);
            star_vi = vi;
            pi += 1;
        } else if pi < p.len() && (p[pi] == b'?' || p[pi] == v[vi]) {
            pi += 1;
            vi += 1;
        } else if let Some(star) = star_pi {
            pi = star + 1;
            star_vi += 1;
            vi = star_vi;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }

    pi == p.len()
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // helpers ----------------------------------------------------------------

    fn policy_from(content: &str) -> Policy {
        let file: PolicyFile = toml::from_str(content).expect("test TOML must parse");
        let mut rules = Vec::with_capacity(file.rules.len());
        let mut seen = HashSet::new();
        for r in file.rules {
            assert!(seen.insert(r.id.clone()), "test data: duplicate id");
            rules.push(PolicyRule {
                id: r.id,
                action: r.action,
                effect: r.effect,
                server_pattern: r.server,
                tool_pattern: r.tool,
                skill_pattern: r.skill,
            });
        }
        Policy {
            default_effect: file.default.unwrap_or(Effect::Allow),
            rules,
        }
    }

    fn check(
        policy: &Policy,
        action: PolicyAction,
        server_id: &str,
        tool_name: Option<&str>,
    ) -> PolicyDecision {
        policy.check(action, server_id, tool_name)
    }

    const ALLOW: PolicyDecision = PolicyDecision::Allow;
    const DENY: PolicyDecision = PolicyDecision::Deny;

    // default allow (v0.2 backwards compatibility) ---------------------------

    #[test]
    fn default_allow_when_no_file() {
        let policy = Policy::allow_all();
        // no rules, default allow
        assert_eq!(check(&policy, PolicyAction::Connect, "any", None), ALLOW);
        assert_eq!(
            check(&policy, PolicyAction::Call, "any", Some("tool")),
            ALLOW
        );
    }

    #[test]
    fn default_allow_returns_default_for_unmatched() {
        let policy = policy_from(
            r#"
            [[rules]]
            id = "only-github"
            action = "connect"
            effect = "allow"
            server = "github"
            "#,
        );
        // matches the rule
        assert_eq!(check(&policy, PolicyAction::Connect, "github", None), ALLOW);
        // unmatched server → default allow
        assert_eq!(check(&policy, PolicyAction::Connect, "gitlab", None), ALLOW);
    }

    // default deny -----------------------------------------------------------

    #[test]
    fn default_deny_when_no_rules() {
        let policy = policy_from(r#"default = "deny""#);
        assert_eq!(check(&policy, PolicyAction::Connect, "github", None), DENY);
    }

    #[test]
    fn default_deny_blocks_unmatched() {
        let policy = policy_from(
            r#"
            default = "deny"

            [[rules]]
            id = "allow-github"
            action = "connect"
            effect = "allow"
            server = "github"
            "#,
        );
        // matched allow
        assert_eq!(check(&policy, PolicyAction::Connect, "github", None), ALLOW);
        // unmatched → default deny
        assert_eq!(check(&policy, PolicyAction::Connect, "gitlab", None), DENY);
    }

    // deny precedence (order-independent) ------------------------------------

    #[test]
    fn deny_wins_regardless_of_rule_order() {
        let deny_first = policy_from(
            r#"
            [[rules]]
            id = "deny-github"
            action = "connect"
            effect = "deny"
            server = "github"

            [[rules]]
            id = "allow-github"
            action = "connect"
            effect = "allow"
            server = "github"
            "#,
        );

        let allow_first = policy_from(
            r#"
            [[rules]]
            id = "allow-github"
            action = "connect"
            effect = "allow"
            server = "github"

            [[rules]]
            id = "deny-github"
            action = "connect"
            effect = "deny"
            server = "github"
            "#,
        );

        assert_eq!(
            check(&deny_first, PolicyAction::Connect, "github", None),
            DENY
        );
        assert_eq!(
            check(&allow_first, PolicyAction::Connect, "github", None),
            DENY
        );
    }

    #[test]
    fn deny_specific_overrides_allow_glob() {
        let policy = policy_from(
            r#"
            [[rules]]
            id = "allow-all-connect"
            action = "connect"
            effect = "allow"
            server = "*"

            [[rules]]
            id = "deny-danger"
            action = "connect"
            effect = "deny"
            server = "danger-server"
            "#,
        );
        assert_eq!(
            check(&policy, PolicyAction::Connect, "safe-server", None),
            ALLOW
        );
        assert_eq!(
            check(&policy, PolicyAction::Connect, "danger-server", None),
            DENY
        );
    }

    // wildcard matching ------------------------------------------------------

    #[test]
    fn star_matches_everything() {
        let policy = policy_from(
            r#"
            [[rules]]
            id = "allow-all"
            action = "list"
            effect = "allow"
            server = "*"
            "#,
        );
        assert_eq!(check(&policy, PolicyAction::List, "foo", None), ALLOW);
        assert_eq!(check(&policy, PolicyAction::List, "bar", None), ALLOW);
        assert_eq!(check(&policy, PolicyAction::List, "", None), ALLOW);
    }

    #[test]
    fn question_mark_matches_single_char() {
        let policy = policy_from(
            r#"
            default = "deny"

            [[rules]]
            id = "allow-srvN"
            action = "list"
            effect = "allow"
            server = "srv?"
            "#,
        );
        assert_eq!(check(&policy, PolicyAction::List, "srv1", None), ALLOW);
        assert_eq!(check(&policy, PolicyAction::List, "srvX", None), ALLOW);
        assert_eq!(check(&policy, PolicyAction::List, "srva", None), ALLOW);
        assert_eq!(check(&policy, PolicyAction::List, "srv", None), DENY);
        assert_eq!(check(&policy, PolicyAction::List, "srv12", None), DENY);
    }

    #[test]
    fn glob_case_sensitive() {
        let policy = policy_from(
            r#"
            default = "deny"

            [[rules]]
            id = "allow-github"
            action = "list"
            effect = "allow"
            server = "GitHub"
            "#,
        );
        assert_eq!(check(&policy, PolicyAction::List, "GitHub", None), ALLOW);
        assert_eq!(check(&policy, PolicyAction::List, "github", None), DENY);
        assert_eq!(check(&policy, PolicyAction::List, "GITHUB", None), DENY);
    }

    #[test]
    fn no_server_constraint_matches_any_server() {
        let policy = policy_from(
            r#"
            [[rules]]
            id = "allow-connect"
            action = "connect"
            effect = "allow"
            "#,
        );
        assert_eq!(check(&policy, PolicyAction::Connect, "a", None), ALLOW);
        assert_eq!(check(&policy, PolicyAction::Connect, "b", None), ALLOW);
    }

    // per-tool matching (call action) ---------------------------------------

    #[test]
    fn tool_pattern_only_effective_for_call_action() {
        let policy = policy_from(
            r#"
            default = "deny"

            [[rules]]
            id = "allow-safe-calls"
            action = "call"
            effect = "allow"
            server = "*"
            tool = "safe_*"
            "#,
        );
        assert_eq!(
            check(&policy, PolicyAction::Call, "srv", Some("safe_read")),
            ALLOW
        );
        assert_eq!(
            check(&policy, PolicyAction::Call, "srv", Some("rm_file")),
            DENY
        );
        // without a tool name the tool pattern cannot match
        assert_eq!(check(&policy, PolicyAction::Call, "srv", None), DENY);
    }

    #[test]
    fn specific_tool_deny_overrides_glob_tool_allow() {
        let policy = policy_from(
            r#"
            [[rules]]
            id = "allow-calls"
            action = "call"
            effect = "allow"
            server = "*"
            tool = "*"

            [[rules]]
            id = "deny-dangerous"
            action = "call"
            effect = "deny"
            tool = "rm_*"
            "#,
        );
        assert_eq!(
            check(&policy, PolicyAction::Call, "srv", Some("safe_read")),
            ALLOW
        );
        assert_eq!(
            check(&policy, PolicyAction::Call, "srv", Some("rm_cache")),
            DENY
        );
    }

    #[test]
    fn tool_pattern_ignored_for_non_call_actions() {
        // Non-call actions with a tool pattern pass validation only if the
        // test data bypasses validation. Rule matching ignores tool for
        // non-call.
        let policy = policy_from(
            r#"
            [[rules]]
            id = "allow-inspect"
            action = "inspect"
            effect = "allow"
            server = "*"
            "#,
        );
        // inspect matches regardless of tool name
        assert_eq!(
            check(&policy, PolicyAction::Inspect, "srv", Some("some_tool")),
            ALLOW
        );
        assert_eq!(check(&policy, PolicyAction::Inspect, "srv", None), ALLOW);
    }

    // malformed / validation errors ------------------------------------------

    #[test]
    fn load_optional_missing_file_returns_allow_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = Policy::load_optional(dir.path()).expect("missing file is ok");
        assert_eq!(policy.check(PolicyAction::Connect, "anything", None), ALLOW);
    }

    #[test]
    fn rejects_duplicate_rule_ids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("policy.toml");
        fs::write(
            &path,
            r#"
            [[rules]]
            id = "r1"
            action = "list"
            effect = "allow"

            [[rules]]
            id = "r1"
            action = "connect"
            effect = "deny"
            "#,
        )
        .expect("write");
        let err = Policy::load_optional(dir.path()).expect_err("duplicate ids must fail");
        assert!(matches!(err, PolicyError::Validation(_)));
        let msg = err.to_string();
        assert!(msg.contains("duplicate"), "expected duplicate: {msg}");
    }

    #[test]
    fn rejects_empty_rule_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("policy.toml");
        fs::write(
            &path,
            r#"
            [[rules]]
            id = ""
            action = "list"
            effect = "allow"
            "#,
        )
        .expect("write");
        let err = Policy::load_optional(dir.path()).expect_err("empty id must fail");
        assert!(matches!(err, PolicyError::Validation(_)));
        let msg = err.to_string();
        assert!(msg.contains("must not be empty"), "expected empty: {msg}");
    }

    #[test]
    fn rejects_tool_on_non_call_action() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("policy.toml");
        fs::write(
            &path,
            r#"
            [[rules]]
            id = "bad"
            action = "connect"
            effect = "allow"
            tool = "some_tool"
            "#,
        )
        .expect("write");
        let err = Policy::load_optional(dir.path()).expect_err("tool on connect must fail");
        assert!(matches!(err, PolicyError::Validation(_)));
        let msg = err.to_string();
        assert!(msg.contains("tool pattern"), "expected tool pattern: {msg}");
    }

    #[test]
    fn rejects_unknown_action_at_parse_level() {
        let result = toml::from_str::<PolicyFile>(
            r#"
            [[rules]]
            id = "bad"
            action = "delete"
            effect = "allow"
            "#,
        );
        assert!(result.is_err(), "unknown action must fail at serde level");
    }

    #[test]
    fn rejects_unknown_effect_at_parse_level() {
        let result = toml::from_str::<PolicyFile>(
            r#"
            [[rules]]
            id = "bad"
            action = "list"
            effect = "maybe"
            "#,
        );
        assert!(result.is_err(), "unknown effect must fail at serde level");
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let result = toml::from_str::<PolicyFile>(
            r#"
            unexpected = true

            [[rules]]
            id = "ok"
            action = "list"
            effect = "allow"
            "#,
        );
        assert!(result.is_err(), "unknown top-level field must fail");
    }

    #[test]
    fn rejects_unknown_rule_field() {
        let result = toml::from_str::<PolicyFile>(
            r#"
            [[rules]]
            id = "ok"
            action = "list"
            effect = "allow"
            secret = "leak"
            "#,
        );
        assert!(result.is_err(), "unknown rule field must fail");
    }

    #[test]
    fn skill_patterns_use_deny_precedence() {
        let policy = policy_from(
            r#"
            default = "deny"

            [[rules]]
            id = "allow-skills"
            action = "skill_run"
            effect = "allow"
            skill = "issue-*"

            [[rules]]
            id = "deny-admin"
            action = "skill_run"
            effect = "deny"
            skill = "issue-admin"
            "#,
        );

        assert_eq!(policy.check_skill("issue-create"), PolicyDecision::Allow);
        assert_eq!(policy.check_skill("issue-admin"), PolicyDecision::Deny);
        assert_eq!(policy.check_skill("other"), PolicyDecision::Deny);
    }

    #[test]
    fn skill_and_server_patterns_are_not_interchangeable() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(
            directory.path().join("policy.toml"),
            "[[rules]]\nid='bad'\naction='connect'\neffect='allow'\nskill='*'\n",
        )
        .expect("policy fixture");
        assert!(matches!(
            Policy::load_optional(directory.path()),
            Err(PolicyError::Validation(_))
        ));

        fs::write(
            directory.path().join("policy.toml"),
            "[[rules]]\nid='bad'\naction='skill_run'\neffect='allow'\nserver='*'\n",
        )
        .expect("policy fixture");
        assert!(matches!(
            Policy::load_optional(directory.path()),
            Err(PolicyError::Validation(_))
        ));
    }

    // glob_match unit tests --------------------------------------------------

    #[test]
    fn glob_star_matches_everything() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*c", "ac"));
        assert!(glob_match("a*c", "abc"));
        assert!(glob_match("a*c", "aXYZc"));
    }

    #[test]
    fn glob_question_matches_one() {
        assert!(glob_match("a?c", "abc"));
        assert!(glob_match("a?c", "aXc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(!glob_match("a?c", "abbc"));
    }

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("github", "github"));
        assert!(!glob_match("github", "gitlab"));
    }

    #[test]
    fn glob_empty_pattern() {
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn glob_trailing_star() {
        assert!(glob_match("abc*", "abc"));
        assert!(glob_match("abc*", "abcdef"));
        assert!(!glob_match("abc*", "ab"));
    }

    #[test]
    fn glob_leading_star() {
        assert!(glob_match("*def", "def"));
        assert!(glob_match("*def", "abcdef"));
        assert!(!glob_match("*def", "abcde"));
    }

    #[test]
    fn glob_multiple_stars() {
        assert!(glob_match("a*b*c", "aXYZbPQRc"));
        assert!(glob_match("a*b*c", "abc"));
        assert!(!glob_match("a*b*c", "aXc"));
    }

    // PolicyAction enum coverage ---------------------------------------------

    #[test]
    fn policy_action_deserializes_all_variants() {
        let actions = [
            "inspect",
            "list",
            "connect",
            "disconnect",
            "refresh",
            "call",
            "package_install",
            "auth_start",
            "auth_logout",
            "skill_run",
        ];
        for action_str in actions {
            let toml =
                format!("[[rules]]\nid = \"r\"\naction = \"{action_str}\"\neffect = \"allow\"\n");
            let file: PolicyFile =
                toml::from_str(&toml).expect("all action variants must deserialize");
            assert_eq!(file.rules.len(), 1);
        }
    }

    #[test]
    fn policy_action_allows_tool_only_for_call() {
        assert!(!PolicyAction::Connect.allows_tool());
        assert!(!PolicyAction::List.allows_tool());
        assert!(!PolicyAction::Inspect.allows_tool());
        assert!(PolicyAction::Call.allows_tool());
    }

    // error messages are source-free -----------------------------------------

    #[test]
    fn parse_error_does_not_include_source_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("policy.toml");
        fs::write(
            &path,
            "[[rules]]\nid = \"x\"\naction = \"list\"\neffect = \"allow\"\nbad = 1\n",
        )
        .expect("write");
        let err = Policy::load_optional(dir.path()).expect_err("must fail");
        let msg = err.to_string();
        // must not contain the source snippet
        assert!(!msg.contains("bad ="));
    }
}
