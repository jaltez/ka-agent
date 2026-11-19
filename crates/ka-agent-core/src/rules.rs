//! Durable allow-rules (v4 B7): what `/y <id> always` writes and the
//! runtime consults at ask time. A rule is deliberately narrow — it can
//! only ever answer an ask the acting user themselves would decide (the
//! runtime evaluates rules after D9 routing, never on escalated or orphan
//! asks), and a shape must pass its own generated cases before it is
//! stored (fail-closed against over-broad patterns).
//!
//! Rules never widen anything: they answer an ask the engine already
//! raised, so the engine's own permission gate, grants and role ceilings
//! all still apply upstream. A rule is the absence of a card, not a grant.

use serde::{Deserialize, Serialize};

/// The bash metacharacters that make a command *not provably plain*: a
/// command containing any of them is never prefix-matched — it can only
/// become an [`RuleShape::AskKey`] rule (byte-identical ask).
const NOT_PLAIN: &[char] = &[';', '|', '&', '$', '(', ')', '<', '>', '`', '\n'];

/// Whether a bash command is provably plain: a single command with no
/// separators, splices or redirections. `cargo test --color never` is;
/// `cargo test && rm -rf x`, `a; b`, `echo $(x)` and any pipeline are not.
/// Splitting is only done when it cannot change meaning.
pub fn plain_command(cmd: &str) -> bool {
    let c = cmd.trim();
    !c.is_empty() && !c.chars().any(|ch| NOT_PLAIN.contains(&ch))
}

/// Whether `cmd` extends the plain prefix `prefix` with more arguments
/// (Codex `prefix_rule` semantics: the command starts with these words).
fn prefix_matches(prefix: &str, cmd: &str) -> bool {
    let p = prefix.trim();
    let c = cmd.trim();
    if p.is_empty() || c.is_empty() {
        return false;
    }
    c == p || c.starts_with(&format!("{p} "))
}

/// The shape of a rule: what it matches. The ladder in [`RuleShape::for_ask`]
/// only ever picks the narrowest derivable shape for an approved ask.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuleShape {
    /// The exact ask identity (the `session_allow_key` pair, made durable):
    /// byte-identical question and detail, nothing else. The fallback when
    /// no call context exists — the engine decorates questions with
    /// volatile context, so this shape is last resort, never first.
    AskKey {
        question: String,
        detail: Option<String>,
    },
    /// A byte-identical bash command — the compound-command landing shape.
    /// The command text (from the call, not the decorated question) is the
    /// stable identity. Not a prefix: one extra argument is a different
    /// ask.
    CallKey { command: String },
    /// One tool by hand name. Never `bash` — one approved bash call must
    /// not become a blanket bash allow.
    Tool { name: String },
    /// A provably-plain bash command prefix: `cargo test` also answers
    /// `cargo test --color never`, never `cargo test && …`.
    Command { prefix: String },
}

/// One ask as the matcher sees it, for generated cases.
#[derive(Debug, Clone)]
pub struct AskSample {
    pub question: String,
    pub detail: Option<String>,
    pub last_call: Option<(String, String)>,
}

impl RuleShape {
    /// The narrowest shape derivable from an approved ask. `last_call` is
    /// the engine's (tool, argument summary) for the call behind the ask.
    pub fn for_ask(
        question: &str,
        detail: Option<&str>,
        last_call: Option<(&str, &str)>,
    ) -> RuleShape {
        if let Some((tool, cmd)) = last_call {
            if tool == "bash" {
                let cmd = cmd.trim();
                // Plain command → prefix rule; compound → the exact
                // command key (a `Tool { "bash" }` shape does not exist,
                // and question text is too decorated to key on).
                if !cmd.is_empty() {
                    return if plain_command(cmd) {
                        RuleShape::Command {
                            prefix: cmd.to_string(),
                        }
                    } else {
                        RuleShape::CallKey {
                            command: cmd.to_string(),
                        }
                    };
                }
            } else if !tool.trim().is_empty() {
                return RuleShape::Tool {
                    name: tool.trim().to_string(),
                };
            }
        }
        RuleShape::AskKey {
            question: question.to_string(),
            detail: detail.map(str::to_string),
        }
    }

    /// Whether this shape answers the described ask. `last_call` is the
    /// engine's (tool, argument summary) for the call behind the ask.
    pub fn matches(
        &self,
        question: &str,
        detail: Option<&str>,
        last_call: Option<(&str, &str)>,
    ) -> bool {
        match self {
            RuleShape::AskKey {
                question: q,
                detail: d,
            } => q == question && d.as_deref() == detail,
            RuleShape::CallKey { command } => last_call.is_some_and(|(t, cmd)| {
                t == "bash" && !command.trim().is_empty() && cmd.trim() == command.trim()
            }),
            RuleShape::Tool { name } => {
                // Structurally never bash (validate() rejects it too) —
                // the check here is the second lock on the same door.
                name != "bash"
                    && !name.trim().is_empty()
                    && last_call.is_some_and(|(t, _)| t == name)
            }
            RuleShape::Command { prefix } => last_call.is_some_and(|(t, cmd)| {
                t == "bash"
                    && plain_command(prefix)
                    && plain_command(cmd)
                    && prefix_matches(prefix, cmd)
            }),
        }
    }

    /// Generated (description, expected, sample) cases for this shape —
    /// the inline tests a rule must pass before it is stored
    /// ([`Rule::passes_cases`]) and the examples `ka-agent rule check`
    /// prints. They encode the intended semantics; a matcher regression
    /// fails them.
    pub fn cases(&self) -> Vec<(&'static str, bool, AskSample)> {
        match self {
            RuleShape::AskKey { question, detail } => {
                let exact = AskSample {
                    question: question.clone(),
                    detail: detail.clone(),
                    last_call: None,
                };
                vec![
                    ("the exact question and detail", true, exact),
                    (
                        "a mutated question",
                        false,
                        AskSample {
                            question: format!("{question}!"),
                            detail: detail.clone(),
                            last_call: None,
                        },
                    ),
                    (
                        "a different detail",
                        false,
                        AskSample {
                            question: question.clone(),
                            detail: match detail {
                                Some(d) => Some(format!("{d}!")),
                                // The shape's detail is None: any Some is
                                // a different ask, never a match.
                                None => Some("anything".into()),
                            },
                            last_call: None,
                        },
                    ),
                ]
            }
            RuleShape::CallKey { command } => {
                let c0 = command.trim().to_string();
                let cmd = |c: &str| AskSample {
                    question: format!("run `{c}`? (volatile estimate text)"),
                    detail: None,
                    last_call: Some(("bash".into(), c.into())),
                };
                vec![
                    ("the exact compound command", true, cmd(&c0)),
                    (
                        "the same command with more arguments",
                        false,
                        cmd(&format!("{c0} --extra")),
                    ),
                    ("a mutated command", false, cmd(&format!("{c0}; echo x"))),
                    (
                        "a different tool",
                        false,
                        AskSample {
                            question: "use `grep`?".into(),
                            detail: None,
                            last_call: Some(("grep".into(), c0.clone())),
                        },
                    ),
                ]
            }
            RuleShape::Tool { name } => {
                let tool = name.clone();
                vec![
                    (
                        "the same tool",
                        true,
                        AskSample {
                            question: format!("use `{tool}`?"),
                            detail: None,
                            last_call: Some((tool.clone(), "args".into())),
                        },
                    ),
                    (
                        "a bash ask (never auto-allowed by a tool rule)",
                        false,
                        AskSample {
                            question: "run `cargo test`?".into(),
                            detail: None,
                            last_call: Some(("bash".into(), "cargo test".into())),
                        },
                    ),
                    (
                        "a different tool",
                        false,
                        AskSample {
                            question: "use `other`?".into(),
                            detail: None,
                            last_call: Some(("other".into(), "args".into())),
                        },
                    ),
                ]
            }
            RuleShape::Command { prefix } => {
                let p = prefix.trim().to_string();
                let cmd = |c: &str| AskSample {
                    question: format!("run `{c}`?"),
                    detail: None,
                    last_call: Some(("bash".into(), c.into())),
                };
                vec![
                    ("the exact command", true, cmd(&p)),
                    (
                        "more arguments after the prefix",
                        true,
                        cmd(&format!("{p} --color never")),
                    ),
                    (
                        "a compound after &&",
                        false,
                        cmd(&format!("{p} && rm -rf x")),
                    ),
                    ("a compound after ;", false, cmd(&format!("{p}; rm x"))),
                    ("piped into sh", false, cmd(&format!("{p} | sh"))),
                    ("a command splice", false, cmd(&format!("echo $({p})"))),
                    (
                        "a different tool",
                        false,
                        AskSample {
                            question: "use `grep`?".into(),
                            detail: None,
                            last_call: Some(("grep".into(), p.clone())),
                        },
                    ),
                ]
            }
        }
    }

    /// One-line render for cards, `/rules` and audit rows.
    pub fn label(&self) -> String {
        match self {
            RuleShape::AskKey { question, .. } => {
                format!("exact ask “{question}”")
            }
            RuleShape::CallKey { command } => format!("exact command “{command}”"),
            RuleShape::Tool { name } => format!("tool {name}"),
            RuleShape::Command { prefix } => format!("command “{prefix} …”"),
        }
    }

    /// Structural validation before storage. Empty patterns, a `bash`
    /// tool shape (the blanket-allow ladder hole) and non-plain command
    /// prefixes are refused.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            RuleShape::AskKey { question, .. } if question.trim().is_empty() => {
                Err("ask-key rule needs the exact question".into())
            }
            RuleShape::CallKey { command } if command.trim().is_empty() => {
                Err("call-key rule needs the exact command".into())
            }
            RuleShape::CallKey { command } if plain_command(command) => {
                Err("plain commands take the prefix shape — the exact key is for compounds".into())
            }
            RuleShape::Tool { name } if name.trim().is_empty() => {
                Err("tool rule needs a tool name".into())
            }
            RuleShape::Tool { name } if name == "bash" => {
                Err("no blanket bash rules — compound commands stay exact-ask rules".into())
            }
            RuleShape::Command { prefix } if !plain_command(prefix) => {
                Err("command rules need a provably plain prefix (no ; | & $ ( ) < > `)".into())
            }
            _ => Ok(()),
        }
    }
}

/// One durable rule row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    /// The acting user whose asks this answers (subject-partitioned).
    pub subject: String,
    pub shape: RuleShape,
    /// Canonical mode at creation; fires only in that mode and more
    /// permissive ones (`mode_rank(current) <= mode_rank(rule.mode)`).
    pub mode: String,
    /// The approval that created it, when it came from `/y <id> always`.
    pub from_approval: Option<String>,
    pub created_ms: i64,
}

impl Rule {
    /// The rule must pass its own generated cases before anything stores
    /// it — a shape whose matcher disagrees with its semantics never
    /// lands (fail-closed).
    pub fn passes_cases(&self) -> bool {
        self.shape.cases().into_iter().all(|(_, expected, sample)| {
            self.shape.matches(
                &sample.question,
                sample.detail.as_deref(),
                sample
                    .last_call
                    .as_ref()
                    .map(|(t, d)| (t.as_str(), d.as_str())),
            ) == expected
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape_cmd(prefix: &str) -> RuleShape {
        RuleShape::Command {
            prefix: prefix.into(),
        }
    }

    #[test]
    fn plain_commands_are_single_and_splice_free() {
        assert!(plain_command("cargo test"));
        assert!(plain_command("cargo test --color never"));
        assert!(plain_command("  git status  "));
        assert!(!plain_command("a && b"));
        assert!(!plain_command("a; b"));
        assert!(!plain_command("a | b"));
        assert!(!plain_command("echo $(whoami)"));
        assert!(!plain_command("cat f > g"));
        assert!(!plain_command("a\nb"));
        assert!(!plain_command(""));
    }

    #[test]
    fn command_prefix_covers_arguments_not_compounds() {
        let s = shape_cmd("cargo test");
        let m = |cmd: &str| s.matches("run?", None, Some(("bash", cmd)));
        assert!(m("cargo test"));
        assert!(m("cargo test --color never"));
        assert!(!m("cargo test && rm -rf x"));
        assert!(!m("cargo test; echo pwn"));
        assert!(!m("cargo tests")); // a longer word, not an argument
        assert!(!s.matches("run?", None, Some(("grep", "cargo test"))));
    }

    #[test]
    fn ladder_picks_the_narrowest_derivable_shape() {
        assert_eq!(
            RuleShape::for_ask("run?", None, Some(("bash", "cargo test --color never"))),
            shape_cmd("cargo test --color never")
        );
        // Compound bash lands the exact-command key — never a prefix or a
        // bash-tool rule, and never the question text (too decorated).
        assert_eq!(
            RuleShape::for_ask("run `a && b`?", Some("a && b"), Some(("bash", "a && b"))),
            RuleShape::CallKey {
                command: "a && b".into()
            }
        );
        assert_eq!(
            RuleShape::for_ask("use it?", None, Some(("kagent__notes_read", "notes"))),
            RuleShape::Tool {
                name: "kagent__notes_read".into()
            }
        );
        assert_eq!(
            RuleShape::for_ask("q?", None, None),
            RuleShape::AskKey {
                question: "q?".into(),
                detail: None,
            }
        );
    }

    #[test]
    fn tool_rules_never_match_bash() {
        let s = RuleShape::Tool {
            name: "bash".into(),
        };
        assert!(!s.matches("run?", None, Some(("bash", "cargo test"))));
        assert!(s.validate().is_err());
    }

    #[test]
    fn generated_cases_pass_for_legal_shapes() {
        for s in [
            shape_cmd("cargo test"),
            RuleShape::Tool {
                name: "kagent__notes_read".into(),
            },
            RuleShape::AskKey {
                question: "run `a && b`?".into(),
                detail: Some("a && b".into()),
            },
            // The compound ladder's landing shape, and the no-call-context
            // fallback (engine ask detail None).
            RuleShape::CallKey {
                command: "echo a && echo b".into(),
            },
            RuleShape::AskKey {
                question: "run `a && b`?".into(),
                detail: None,
            },
        ] {
            let rule = Rule {
                id: "rule_x".into(),
                subject: "operator".into(),
                shape: s.clone(),
                mode: "ask".into(),
                from_approval: None,
                created_ms: 0,
            };
            assert!(rule.passes_cases(), "cases failed for {:?}", s);
        }
    }

    #[test]
    fn illegal_shapes_are_refused_at_validation() {
        assert!(shape_cmd("a && b").validate().is_err());
        assert!(shape_cmd("  ").validate().is_err());
        assert!(
            RuleShape::Tool {
                name: "bash".into()
            }
            .validate()
            .is_err()
        );
        assert!(RuleShape::Tool { name: " ".into() }.validate().is_err());
    }
}
