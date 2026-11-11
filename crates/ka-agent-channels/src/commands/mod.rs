//! The shared command router (SEED.md §3.6): every channel parses the same
//! `/commands` and gets the same replies; only the transport differs.
//! Free text is returned as [`Outcome::NotACommand`] for the channel to
//! hand to the runtime as a message.
//!
//! One table drives everything: [`COMMANDS`] lists every command with its
//! aliases, reach, Telegram-menu flag and handler; [`route`], [`help_text`],
//! [`help_for`] and the Telegram `setMyCommands` menu all iterate it, so
//! help, menu and routing can never drift apart.

mod approvals;
mod memory;
mod misc;
mod schedule;
mod session;
mod update;
/// The interactive pickers (`/model`, `/mode`, `/effort`) exported for
/// the surfaces that render them natively (Telegram's keyboards build
/// on the same strings).
pub use session::{EFFORTS, MODES, model_header, model_picker, model_vendor_listing};
mod skills;

pub use misc::{fmt_interval, local_compact};
pub use schedule::{cron_add, cron_list_text};

use ka_agent_runtime::Runtime;
use std::pin::Pin;
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Send this text back to the sender.
    Reply(String),
    /// Handled, nothing to send (e.g. `/stop` succeeded quietly).
    Silent,
    /// Not a command: the channel forwards it to `Runtime::handle_message`.
    NotACommand,
}

/// The boxed future returned by every command handler.
pub type CmdFuture<'a> = Pin<Box<dyn Future<Output = Outcome> + Send + 'a>>;

/// Everything a handler needs: the runtime, the channel identity and the
/// trimmed text after the command word.
pub struct Ctx<'a> {
    pub runtime: &'a Runtime,
    pub source: &'a str,
    pub args: &'a str,
}

/// Where a command can be used: everywhere, or only from local surfaces
/// (terminal, CLI, web panel).
pub enum Reach {
    Everywhere,
    Local,
}

/// One command, for `/help`, `/help <name>` and the Telegram command menu.
pub struct CommandSpec {
    pub name: &'static str,
    pub args: &'static str,
    pub summary: &'static str,
    pub example: &'static str,
    /// Listed in Telegram's bot-command menu (fits its 256-char limit).
    pub telegram: bool,
    /// Alternate names (`/start` for `/help`), without the leading slash.
    pub aliases: &'static [&'static str],
    /// Whether the command works from remote channels.
    pub reach: Reach,
    pub run: fn(Ctx<'_>) -> CmdFuture<'_>,
}

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "help",
        args: "[command]",
        summary: "list commands, or explain one",
        example: "/help remind",
        telegram: true,
        aliases: &["start"],
        reach: Reach::Everywhere,
        run: misc::run_help,
    },
    CommandSpec {
        name: "status",
        args: "",
        summary: "session, model, spend, queue, next task",
        example: "/status",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: session::run_status,
    },
    CommandSpec {
        name: "new",
        args: "[private]",
        summary: "start a fresh session (stops the current worker); `private` = incognito — nothing is remembered",
        example: "/new private",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: session::run_new,
    },
    CommandSpec {
        name: "stop",
        args: "",
        summary: "abort the running reply",
        example: "/stop",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: session::run_stop,
    },
    CommandSpec {
        name: "resume",
        args: "",
        summary: "continue after a pause (budget or provider key)",
        example: "/resume",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: session::run_resume,
    },
    CommandSpec {
        name: "retry",
        args: "",
        summary: "re-send the last failed or interrupted question",
        example: "/retry",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: session::run_retry,
    },
    CommandSpec {
        name: "cleanup",
        args: "",
        summary: "remove the bot's recent error and status notices",
        example: "/cleanup",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: misc::run_cleanup,
    },
    CommandSpec {
        name: "update",
        args: "[check]",
        summary: "install the newest release (needs write access to the binary; restarts the daemon)",
        example: "/update",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: update::run_update,
    },
    CommandSpec {
        name: "model",
        args: "[provider/model]",
        summary: "show or switch the model",
        example: "/model anthropic/claude-sonnet-5",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: session::run_model,
    },
    CommandSpec {
        name: "mode",
        args: "[ask|accept_edits|allow|readonly]",
        summary: "how much the agent may do without asking",
        example: "/mode readonly",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: session::run_mode,
    },
    CommandSpec {
        name: "remember",
        args: "<fact>",
        summary: "keep a durable fact",
        example: "/remember I am allergic to shellfish",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: memory::run_remember,
    },
    CommandSpec {
        name: "forget",
        args: "<id>",
        summary: "remove a remembered fact (see /memory)",
        example: "/forget a1b2c3d4",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: memory::run_forget,
    },
    CommandSpec {
        name: "memory",
        args: "[files|review]",
        summary: "what is remembered and its budget",
        example: "/memory",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: memory::run_memory,
    },
    CommandSpec {
        name: "history",
        args: "[query|show <id>]",
        summary: "past turns of this session",
        example: "/history volcano",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: memory::run_history,
    },
    CommandSpec {
        name: "remind",
        args: "<when> <text>",
        summary: "remind me at a time (2h30m, 18:30, tomorrow 9am)",
        example: "/remind tomorrow 9am call mom",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: schedule::run_remind,
    },
    CommandSpec {
        name: "reminders",
        args: "",
        summary: "upcoming reminders and crons",
        example: "/reminders",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: schedule::run_reminders,
    },
    CommandSpec {
        name: "cron",
        args: "[add|rm]",
        summary: "scheduled prompts (in 10m, every 1h, daily 08:00)",
        example: "/cron add every day at 08:00 summarize my inbox",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: schedule::run_cron,
    },
    CommandSpec {
        name: "calendar",
        args: "add <src>|today",
        summary: "daily calendar digest",
        example: "/calendar add https://…/calendar.ics",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: schedule::run_calendar,
    },
    CommandSpec {
        name: "skills",
        args: "",
        summary: "enabled skills",
        example: "/skills",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: skills::run_skills,
    },
    CommandSpec {
        name: "skill",
        args: "<name> <text>",
        summary: "run one turn with a skill loaded",
        example: "/skill review this diff",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: skills::run_skill,
    },
    CommandSpec {
        name: "approvals",
        args: "",
        summary: "pending approvals",
        example: "/approvals",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: approvals::run_approvals,
    },
    CommandSpec {
        name: "y",
        args: "[id] [session]",
        summary: "allow a pending approval (`session`: and identical ones until /new)",
        example: "/y",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: approvals::run_y,
    },
    CommandSpec {
        name: "n",
        args: "[id]",
        summary: "deny a pending approval",
        example: "/n",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: approvals::run_n,
    },
    CommandSpec {
        name: "persona",
        args: "",
        summary: "show the assistant's persona",
        example: "/persona",
        telegram: false,
        aliases: &[],
        reach: Reach::Everywhere,
        run: session::run_persona,
    },
    CommandSpec {
        name: "effort",
        args: "[level]",
        summary: "reasoning effort for this session (off|low|medium|high|max)",
        example: "/effort high",
        telegram: false,
        aliases: &[],
        reach: Reach::Everywhere,
        run: session::run_effort,
    },
    CommandSpec {
        name: "undo",
        args: "",
        summary: "revert the last file change of this session",
        example: "/undo",
        telegram: false,
        aliases: &[],
        reach: Reach::Everywhere,
        run: session::run_undo,
    },
    CommandSpec {
        name: "steer",
        args: "<text>",
        summary: "add a thought to the reply that is running",
        example: "/steer use metric units",
        telegram: true,
        aliases: &[],
        reach: Reach::Everywhere,
        run: session::run_steer,
    },
    CommandSpec {
        name: "queue",
        args: "[clear]",
        summary: "list or clear messages waiting for the next turn",
        example: "/queue",
        telegram: false,
        aliases: &[],
        reach: Reach::Everywhere,
        run: session::run_queue,
    },
    CommandSpec {
        name: "tools",
        args: "",
        summary: "the tools the engine can reach",
        example: "/tools",
        telegram: false,
        aliases: &[],
        reach: Reach::Everywhere,
        run: session::run_tools,
    },
    CommandSpec {
        name: "keys",
        args: "",
        summary: "the terminal client's key bindings",
        example: "/keys",
        telegram: false,
        aliases: &[],
        reach: Reach::Local,
        run: run_keys,
    },
    CommandSpec {
        name: "quit",
        args: "",
        summary: "leave the terminal chat",
        example: "/quit",
        telegram: false,
        aliases: &["q", "exit"],
        reach: Reach::Local,
        run: session::run_quit,
    },
];

/// The terminal key table `/keys` prints — the same rows as
/// `docs/contract/terminal.md`'s key table, so the in-app list and the
/// contract cannot drift apart in spirit (both name every binding).
fn keys_text() -> String {
    [
        "Enter — submit the draft",
        "Shift-Enter / Alt-Enter / Ctrl-J — newline inside the draft (typed newlines fold to a space; pasted text keeps its lines)",
        "←/→, Home/End, Ctrl-A — cursor movement",
        "Alt-b / Alt-f, Ctrl-←/→ — word movement",
        "Backspace / Delete — delete",
        "Ctrl-W — kill the word before the cursor",
        "Ctrl-K / Ctrl-U — kill to end / to start",
        "Ctrl-Y — yank the last kill back",
        "↑/↓ — history (the walk filters by the draft it started with)",
        "Ctrl-R — incremental reverse search (Enter accepts, Esc or Ctrl-G cancels)",
        "Tab — complete /commands and @paths (a single match inserts; several list and Tab cycles through them)",
        "Ctrl-C — clear the draft; on an empty draft stop the running turn; twice on an idle prompt quits",
        "Esc — stop the running turn; idle, dismiss the completions",
        "Ctrl-D — quit on an empty draft",
        "Ctrl-L — clear the screen, keep the live area",
        "Ctrl-O — toggle tool detail rows",
        "Ctrl-T — toggle streamed thinking",
        "Ctrl-E — expand the last tool's call and output",
        "Ctrl-G — edit the draft in $VISUAL/$EDITOR",
    ]
    .join("\n")
}

fn run_keys<'a>(_ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move { Outcome::Reply(keys_text()) })
}

/// One line per command: `/name args — summary`.
pub fn help_text() -> String {
    let mut out = COMMANDS
        .iter()
        .map(|c| {
            if c.args.is_empty() {
                format!("/{} — {}", c.name, c.summary)
            } else {
                format!("/{} {} — {}", c.name, c.args, c.summary)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    out.push_str(
        "\n\nTip: just talk to me — commands are optional. @file or @https://… adds context.",
    );
    out
}

/// `/help <name>`: usage, summary, example. Resolves aliases too.
pub fn help_for(name: &str) -> Option<String> {
    let c = COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name))?;
    Some(format!(
        "/{} {}\n{}\nexample: {}",
        c.name, c.args, c.summary, c.example
    ))
}

/// Whether one role may use a command by name (or alias): admin always
/// can; a subject gets its configured list plus the `help`/`status` floor,
/// and the hard admin-only set stays unreachable whatever the list says.
pub fn allowed_for(runtime: &Runtime, role: &str, name: &str) -> bool {
    if role == ka_agent_core::ADMIN_ROLE {
        return true;
    }
    const ADMIN_ONLY: &[&str] = &["update", "keys", "quit", "persona", "model", "config"];
    const FLOOR: &[&str] = &["help", "status"];
    let Some(spec) = COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name))
    else {
        return false;
    };
    if ADMIN_ONLY.contains(&spec.name) {
        return false;
    }
    let allowed = runtime.effective_role(role).commands;
    allowed.is_empty()
        || allowed
            .iter()
            .any(|c| c == spec.name || spec.aliases.contains(&c.as_str()))
        || FLOOR.contains(&spec.name)
}

/// Route one inbound line. `source` is the channel identity
/// (`telegram:12345`, `matrix:@a:b`) used for provenance.
pub async fn route(runtime: &Runtime, source: &str, text: &str) -> Outcome {
    let Some(word) = text.split_whitespace().next() else {
        return Outcome::NotACommand;
    };
    if !word.starts_with('/') {
        return Outcome::NotACommand;
    }
    // `/status@my_bot`: Telegram appends the bot's name in groups.
    let word = word.split('@').next().unwrap_or(word);
    let Some(name) = word.strip_prefix('/') else {
        return Outcome::NotACommand;
    };
    let Some(spec) = COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name))
    else {
        return Outcome::Reply(format!("unknown command {word} — /help lists them"));
    };
    if matches!(spec.reach, Reach::Local) && !ka_agent_core::is_local_source(source) {
        return Outcome::Reply(format!(
            "/{} works in the terminal, CLI or web panel",
            spec.name
        ));
    }
    // Roles: a non-admin role's command surface is its configured list
    // plus the help/status floor; a hard admin-only set stays unreachable
    // whatever the list says (config, keys, model and persona are the
    // operator's, not a subject's).
    let role = runtime.role_for_source(source);
    if !allowed_for(runtime, &role, spec.name) {
        return Outcome::Reply(format!(
            "command /{} is not available for role {role}",
            spec.name
        ));
    }
    // Commands act on the requesting role's session: hand the core over
    // first (a running turn of another role is never disturbed).
    if !runtime.activate_role(source).await {
        return Outcome::Reply(
            "another conversation is running — try again when its turn finishes".into(),
        );
    }
    let args = text
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest.trim())
        .unwrap_or("");
    // Telegram deep links arrive as `/start <payload>`. The alias never
    // takes a topic, so the payload is ignored and the command list answers
    // (it used to be read as `/help <topic>` and answered "unknown command").
    let args = if spec.name == "help" && name != spec.name {
        ""
    } else {
        args
    };
    (spec.run)(Ctx {
        runtime,
        source,
        args,
    })
    .await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use ka_agent_core::DaemonConfig;
    use ka_agent_port::WorkerPort;
    use ka_agent_runtime::surface::Surface;
    use ka_agent_runtime::{Paths, Runtime};
    use ka_agent_store::Store;
    use std::sync::Arc;

    struct NullSurface;
    impl Surface for NullSurface {
        fn emit(&self, _env: &ka_agent_runtime::Envelope) {}
    }

    fn test_runtime() -> Runtime {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ka-cmd-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store::open(&dir.join("state.db")).unwrap());
        let cfg = DaemonConfig {
            workspace: dir.clone(),
            ..DaemonConfig::default()
        };
        Runtime::new(
            cfg,
            Paths::new(dir),
            store,
            WorkerPort::default(),
            Arc::new(NullSurface),
        )
        .unwrap()
    }

    #[test]
    fn names_and_aliases_are_unique_bare_words() {
        let mut seen: Vec<&str> = Vec::new();
        for c in COMMANDS {
            assert!(!c.name.is_empty() && !c.name.contains('/'), "{}", c.name);
            seen.push(c.name);
            for a in c.aliases {
                assert!(!a.is_empty() && !a.contains('/'), "{}", a);
                seen.push(a);
            }
        }
        let total = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), total, "duplicate command name or alias");
    }

    #[tokio::test]
    async fn local_command_from_remote_source_refuses() {
        let runtime = test_runtime();
        for word in ["/quit", "/q", "/exit"] {
            assert_eq!(
                route(&runtime, "telegram:123", word).await,
                Outcome::Reply("/quit works in the terminal, CLI or web panel".into())
            );
        }
        // Local surfaces get the real handler reply.
        assert_eq!(
            route(&runtime, "console", "/quit").await,
            Outcome::Reply("/quit leaves the terminal chat — nothing to quit here".into())
        );
    }

    #[test]
    fn help_lists_every_command_with_usage() {
        for c in COMMANDS {
            assert!(
                help_text().contains(&format!("/{}", c.name)),
                "/{} missing from help",
                c.name
            );
            assert!(!c.summary.is_empty());
        }
        assert!(help_text().contains("Tip: just talk to me"));
        assert!(help_for("remind").unwrap().contains("/remind <when>"));
        assert!(help_for("nope").is_none());
    }

    #[test]
    fn help_for_resolves_aliases() {
        assert!(help_for("start").unwrap().contains("list commands"));
        assert!(
            help_for("exit")
                .unwrap()
                .contains("leave the terminal chat")
        );
    }

    #[tokio::test]
    async fn unknown_command_reply_is_unchanged() {
        let runtime = test_runtime();
        assert_eq!(
            route(&runtime, "console", "/bogus one two").await,
            Outcome::Reply("unknown command /bogus — /help lists them".into())
        );
    }

    #[tokio::test]
    async fn start_deep_link_payload_is_ignored() {
        let runtime = test_runtime();
        // Telegram deep links append a payload: `/start <payload>` answers
        // the command list like a bare `/start`, never "unknown command".
        assert_eq!(
            route(&runtime, "telegram:1", "/start abc123").await,
            Outcome::Reply(help_text())
        );
        assert_eq!(
            route(&runtime, "telegram:1", "/start@my_bot abc123").await,
            Outcome::Reply(help_text())
        );
        // `/help <topic>` still resolves the topic.
        assert!(
            matches!(route(&runtime, "telegram:1", "/help remind").await, Outcome::Reply(r) if r.contains("/remind <when>"))
        );
    }

    #[test]
    fn telegram_menu_only_lists_flagged_commands() {
        assert!(COMMANDS.iter().filter(|c| c.telegram).count() >= 10);
        assert!(COMMANDS.iter().all(|c| c.summary.len() <= 256));
    }

    #[tokio::test]
    async fn model_command_is_an_interactive_picker() {
        let runtime = test_runtime();
        // No arguments: the current model plus the numbered providers.
        let Outcome::Reply(listing) = route(&runtime, "console", "/model").await else {
            panic!("expected a reply");
        };
        assert!(listing.contains("model: none"), "{listing}");
        assert!(listing.contains("1. anthropic — "), "{listing}");
        assert!(
            listing.contains("/model <n|provider> lists its models"),
            "{listing}"
        );

        // A provider argument: its models, numbered, recommended first.
        let Outcome::Reply(models) = route(&runtime, "console", "/model anthropic").await else {
            panic!("expected a reply");
        };
        assert!(models.contains("anthropic — "), "{models}");
        assert!(models.contains("1. anthropic/claude-sonnet-5"), "{models}");
        assert!(models.contains("/model anthropic <n> switches"), "{models}");

        // `/model <provider> <n>` switches through the same path as a
        // full selector (needs a config file to write).
        let cfg = std::env::temp_dir().join(format!("ka-cmd-model-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&cfg);
        runtime.set_config_path(cfg.clone());
        let Outcome::Reply(switch) = route(&runtime, "console", "/model anthropic 1").await else {
            panic!("expected a reply");
        };
        assert!(
            switch.contains("model → anthropic/claude-sonnet-5"),
            "{switch}"
        );
        assert_eq!(
            runtime.config().model.as_deref(),
            Some("anthropic/claude-sonnet-5")
        );

        // Out-of-range numbers are refused, never silently re-listed.
        let Outcome::Reply(oob) = route(&runtime, "console", "/model anthropic 999").await else {
            panic!("expected a reply");
        };
        assert!(oob.starts_with("! [model]"), "{oob}");
        let _ = std::fs::remove_file(&cfg);
    }

    #[tokio::test]
    async fn mode_and_effort_pick_by_number() {
        let runtime = test_runtime();
        let cfg = std::env::temp_dir().join(format!("ka-cmd-mode-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&cfg);
        runtime.set_config_path(cfg.clone());

        let Outcome::Reply(modes) = route(&runtime, "console", "/mode").await else {
            panic!("expected a reply");
        };
        assert!(modes.contains("mode: ask"), "{modes}");
        assert!(modes.contains("1. ask — asks before acting ←"), "{modes}");
        assert!(modes.contains("4. readonly"), "{modes}");
        let Outcome::Reply(switch) = route(&runtime, "console", "/mode 4").await else {
            panic!("expected a reply");
        };
        assert_eq!(switch, "mode → readonly");
        // Out of range names the bound.
        let Outcome::Reply(oob) = route(&runtime, "console", "/mode 9").await else {
            panic!("expected a reply");
        };
        assert!(oob.contains("no option 9"), "{oob}");

        let Outcome::Reply(levels) = route(&runtime, "console", "/effort").await else {
            panic!("expected a reply");
        };
        assert!(levels.contains("effort: model default"), "{levels}");
        assert!(levels.contains("3. medium"), "{levels}");
        let Outcome::Reply(effort) = route(&runtime, "console", "/effort 3").await else {
            panic!("expected a reply");
        };
        assert_eq!(effort, "effort → medium (this session)");
        let _ = std::fs::remove_file(&cfg);
    }

    #[tokio::test]
    async fn skill_with_no_arguments_lists_the_library() {
        let runtime = test_runtime();
        let Outcome::Reply(reply) = route(&runtime, "console", "/skill").await else {
            panic!("expected a reply");
        };
        assert!(reply.contains("no skills enabled"), "{reply}");
        assert!(
            reply.contains("/skill <name> <text>") || reply.contains("skills enable"),
            "{reply}"
        );
    }
}
