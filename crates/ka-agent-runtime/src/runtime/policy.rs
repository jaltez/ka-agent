//! Approval routing policy (ADR-0004, D9).
//!
//! An ask raised in a user's turn is *self-scope* when the action stays
//! inside that user's own grants and quota and is not on their role's
//! escalation list; the acting user then decides it on their own surface.
//! Everything else — shared resources, over-quota actions, escalation-list
//! hits — goes to the admin queue with the reason on the card.

use ka_agent_core::config::RoleSpec;

/// Tools that always mutate an org-shared resource: no subject's approval is
/// ever enough for these (D9's "shared resources" class).
pub const SHARED_RESOURCE_TOOLS: &[&str] = &[
    "kagent__kb_write",
    "kagent__kb_rm",
    "kagent__kb_grant",
    "kagent__kb_create",
    "kagent__persona",
    "kagent__config",
    "kagent__agent_share",
];

/// One ask's routing context.
pub struct AskContext<'a> {
    /// The acting role (`admin` for the operator).
    pub role: &'a str,
    /// The role's spec (`None` for admin).
    pub spec: Option<&'a RoleSpec>,
    /// The user's own root: paths under it are theirs.
    pub user_root: &'a std::path::Path,
    /// Today's budget verdict already exceeded.
    pub budget_exceeded: bool,
    /// The tool behind the ask (the engine's latest call), if any.
    pub tool: Option<&'a str>,
    /// The engine's question and detail (where a path appears).
    pub question: &'a str,
    pub detail: Option<&'a str>,
}

impl AskContext<'_> {
    /// Paths named by the ask: absolute tokens in the question/detail.
    fn named_paths(&self) -> Vec<std::path::PathBuf> {
        let mut text = self.question.to_string();
        if let Some(detail) = self.detail {
            text.push(' ');
            text.push_str(detail);
        }
        let mut out = Vec::new();
        for token in text.split(|c: char| {
            c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '(' | ')' | '[' | ']' | ',' | ';')
        }) {
            let token = token.trim_end_matches(':');
            if token.starts_with('/') && token.len() > 1 {
                out.push(std::path::PathBuf::from(token));
            }
        }
        out
    }
}

/// Whether `spec`'s escalation list matches `tool` (exact hand name, or
/// `mcp:<server>[:<tool>]` against the engine's `server.tool` spelling).
pub fn escalate_matches(spec_list: &[String], tool: &str) -> Option<String> {
    for spec in spec_list {
        if spec == tool {
            return Some(spec.clone());
        }
        if let Some(rest) = spec.strip_prefix("mcp:") {
            let (server, tool_name) = match rest.split_once(':') {
                Some((s, t)) => (s, Some(t)),
                None => (rest, None),
            };
            if server.is_empty() {
                continue;
            }
            let prefix_dot = format!("{server}.");
            let prefix_us = format!("{server}__");
            let matches = match tool_name {
                None => tool.starts_with(&prefix_dot) || tool.starts_with(&prefix_us),
                Some(t) => tool == format!("{server}.{t}") || tool == format!("{server}__{t}"),
            };
            if matches {
                return Some(spec.clone());
            }
        }
    }
    None
}

/// The routing verdict: `Ok(())` = self-scope, `Err(reason)` = escalate with
/// that reason on the card.
pub fn route(ctx: &AskContext<'_>) -> Result<(), String> {
    // The operator decides everything, including their own asks.
    if ctx.role == ka_agent_core::ADMIN_ROLE {
        return Ok(());
    }
    if ctx.budget_exceeded {
        return Err("over today's quota".to_string());
    }
    if let Some(tool) = ctx.tool {
        if let Some(hit) = SHARED_RESOURCE_TOOLS.iter().find(|t| **t == tool) {
            return Err(format!("shared resource ({hit})"));
        }
        if let Some(spec) = ctx.spec
            && let Some(hit) = escalate_matches(&spec.escalate, tool)
        {
            return Err(format!("role escalation rule {hit:?}"));
        }
    }
    // A path outside the user's own tree needs a grant covering it; without
    // one the action reaches files the operator did not delegate.
    let allow: Vec<std::path::PathBuf> = ctx
        .spec
        .map(|s| {
            s.grants
                .path
                .iter()
                .filter(|g| g.access == ka_agent_core::config::Access::Allow)
                .map(|g| g.dir.clone())
                .collect()
        })
        .unwrap_or_default();
    for path in ctx.named_paths() {
        let in_root = path.starts_with(ctx.user_root);
        let granted = allow.iter().any(|dir| path.starts_with(dir));
        if !in_root && !granted {
            return Err(format!("{} is outside your granted paths", path.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use ka_agent_core::config::{Access, PathGrant};

    fn spec() -> RoleSpec {
        RoleSpec {
            name: "member".into(),
            mode: "ask".into(),
            allow: vec!["read".into()],
            grants: ka_agent_core::config::Grants {
                path: vec![PathGrant {
                    dir: "/srv/reports".into(),
                    access: Access::Allow,
                }],
                ..Default::default()
            },
            escalate: vec!["kagent__github_issue".into(), "mcp:jira".into()],
            ..Default::default()
        }
    }

    fn ctx<'a>(spec: &'a RoleSpec, tool: &'a str, question: &'a str) -> AskContext<'a> {
        AskContext {
            role: "member",
            spec: Some(spec),
            user_root: std::path::Path::new("/data/users/ana"),
            budget_exceeded: false,
            tool: Some(tool),
            question,
            detail: None,
        }
    }

    #[test]
    fn inside_grants_is_self_scope() {
        let s = spec();
        assert!(route(&ctx(&s, "write", "write /srv/reports/q3.md?")).is_ok());
        assert!(route(&ctx(&s, "write", "write /data/users/ana/notes.md?")).is_ok());
        assert!(route(&ctx(&s, "read", "read the README?")).is_ok());
    }

    #[test]
    fn shared_over_quota_and_escalations_go_to_the_admin() {
        let s = spec();
        assert!(route(&ctx(&s, "kagent__kb_write", "write kb?")).is_err());
        assert!(route(&ctx(&s, "kagent__kb_search_any", "x?")).is_ok());
        assert!(
            route(&ctx(&s, "kagent__github_issue", "open issue?")).is_err(),
            "an escalation-list hit escalates"
        );
        assert!(
            route(&ctx(&s, "jira.create", "create JIRA-1?")).is_err(),
            "mcp:<server> matches the engine's server.tool spelling"
        );
        let mut over = ctx(&s, "write", "write /srv/reports/q3.md?");
        over.budget_exceeded = true;
        assert_eq!(route(&over).unwrap_err(), "over today's quota");
    }

    #[test]
    fn a_path_outside_the_grants_escalates() {
        let s = spec();
        let err = route(&ctx(&s, "write", "write /etc/passwd?")).unwrap_err();
        assert!(err.contains("/etc/passwd"), "{err}");
        // An admin is never asked to escalate to themselves.
        let admin = AskContext {
            role: "admin",
            spec: None,
            ..ctx(&s, "write", "write /etc/passwd?")
        };
        assert!(route(&admin).is_ok());
    }

    #[test]
    fn escalate_specs_match_exactly_or_by_server() {
        let list = vec!["tool_a".to_string(), "mcp:github".to_string()];
        assert_eq!(escalate_matches(&list, "tool_a").as_deref(), Some("tool_a"));
        assert_eq!(
            escalate_matches(&list, "github.create_issue").as_deref(),
            Some("mcp:github")
        );
        assert!(escalate_matches(&list, "gitlab.create_issue").is_none());
        let list = vec!["mcp:github:create_issue".to_string()];
        assert!(escalate_matches(&list, "github.create_issue").is_some());
        assert!(escalate_matches(&list, "github.list_issues").is_none());
    }
}
