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
        args: "",
        summary: "start a fresh session (stops the current worker)",
        example: "/new",
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
        args: "[guarded|accept_edits|plan|free]",
        summary: "how much the agent may do without asking",
        example: "/mode plan",
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
    let args = text
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest.trim())
        .unwrap_or("");
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

    #[test]
    fn telegram_menu_only_lists_flagged_commands() {
        assert!(COMMANDS.iter().filter(|c| c.telegram).count() >= 10);
        assert!(COMMANDS.iter().all(|c| c.summary.len() <= 256));
    }
}
