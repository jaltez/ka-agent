//! CLI surface e2e: commands that only edit files or print (no daemon).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn temp_root(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ka-agent-cli-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `ka-agent` with an isolated home: the data root and config live in `root`.
fn ka(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .args(args)
        .env("HOME", root)
        .env("KA_AGENT_DATA_DIR", root.join("data"))
        .env("KA_AGENT_CONFIG", root.join("config.toml"))
        .env_remove("KA_AGENT_PROFILE")
        .env_remove("KA_AGENT_HOME")
        .output()
        .unwrap()
}

fn text(o: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn mcp_add_list_rm_round_trip_keeps_the_config_valid() {
    let root = temp_root("mcp");
    std::fs::write(root.join("config.toml"), "# mine\nmode = \"guarded\"\n").unwrap();

    let o = ka(
        &root,
        &[
            "mcp",
            "add",
            "notes",
            "--env",
            "TOKEN=secret:notes.token",
            "--",
            "npx",
            "-y",
            "notes-mcp",
        ],
    );
    assert!(o.status.success(), "{}", text(&o));
    let o = ka(
        &root,
        &["mcp", "add", "home", "--url", "http://127.0.0.1:8123/mcp"],
    );
    assert!(o.status.success(), "{}", text(&o));

    // The file is valid config and keeps the operator's comment.
    let cfg_text = std::fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(cfg_text.contains("# mine"), "{cfg_text}");
    let cfg = ka_agent_core::DaemonConfig::load(&root.join("config.toml")).unwrap();
    assert_eq!(cfg.mcp.len(), 2);
    assert_eq!(cfg.mcp[0].command.as_deref(), Some("npx"));
    assert_eq!(cfg.mcp[0].args, vec!["-y", "notes-mcp"]);
    assert_eq!(
        cfg.mcp[0].env.get("TOKEN").map(String::as_str),
        Some("secret:notes.token")
    );

    let o = ka(&root, &["mcp", "list"]);
    let out = text(&o);
    assert!(
        out.contains("notes") && out.contains("secrets: notes.token"),
        "{out}"
    );
    assert!(out.contains("http://127.0.0.1:8123/mcp"), "{out}");

    // Invalid input is refused without touching the file.
    let before = std::fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(
        !ka(&root, &["mcp", "add", "kagent", "--", "x"])
            .status
            .success()
    );
    assert!(
        !ka(
            &root,
            &["mcp", "add", "both", "--url", "http://x", "--", "y"]
        )
        .status
        .success()
    );
    assert!(
        !ka(&root, &["mcp", "add", "notes", "--", "dup"])
            .status
            .success()
    );
    assert_eq!(
        before,
        std::fs::read_to_string(root.join("config.toml")).unwrap()
    );

    assert!(ka(&root, &["mcp", "rm", "notes"]).status.success());
    assert!(ka(&root, &["mcp", "rm", "home"]).status.success());
    let cfg_text = std::fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(!cfg_text.contains("mcp"), "{cfg_text}");
    assert!(!ka(&root, &["mcp", "rm", "nope"]).status.success());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn config_set_fallback_validates_against_the_catalog() {
    let root = temp_root("fallback");
    std::fs::write(root.join("config.toml"), "mode = \"guarded\"\n").unwrap();
    let o = ka(
        &root,
        &["config", "set", "fallback", "[\"nope/not-a-model\"]"],
    );
    assert!(!o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("unknown model"), "{}", text(&o));
    let o = ka(
        &root,
        &["config", "set", "fallback", "[\"openai/gpt-5.1\"]"],
    );
    assert!(o.status.success(), "{}", text(&o));
    let cfg = ka_agent_core::DaemonConfig::load(&root.join("config.toml")).unwrap();
    assert_eq!(cfg.fallback, vec!["openai/gpt-5.1"]);
    // Three hops is more than the engine takes.
    let o = ka(
        &root,
        &[
            "config",
            "set",
            "fallback",
            "[\"openai/gpt-5.1\", \"openai/o3\", \"openai/o4-mini\"]",
        ],
    );
    assert!(!o.status.success(), "{}", text(&o));
    let _ = std::fs::remove_dir_all(&root);
}

/// A1: a fresh machine is "not set up yet", never an I/O error.
#[test]
fn commands_on_a_fresh_machine_say_not_set_up_yet() {
    let root = temp_root("fresh");
    for args in [&["chat"][..], &["run"][..], &["mcp", "list"][..]] {
        let o = ka(&root, args);
        assert!(!o.status.success(), "{args:?}");
        let t = text(&o);
        assert!(t.contains("isn't set up yet"), "{args:?}: {t}");
        assert!(t.contains("ka-agent setup"), "{args:?}: {t}");
        assert!(!t.contains("No such file"), "{args:?}: {t}");
    }
    // An existing but broken config keeps the precise error.
    std::fs::write(root.join("config.toml"), "mode = \"nonsense\"\n").unwrap();
    let t = text(&ka(&root, &["chat"]));
    assert!(
        t.contains("is invalid") && t.contains("unknown mode"),
        "{t}"
    );
    // No arguments and no terminal: a short "start here", not 28 commands.
    let t = text(&ka(&temp_root("fresh-noarg"), &[]));
    assert!(t.contains("Start here"), "{t}");
    assert!(t.lines().count() <= 8, "{t}");
    let _ = std::fs::remove_dir_all(&root);
}

/// B1: the main help is the everyday set; the rest still works.
#[test]
fn help_lists_the_everyday_commands_and_hides_the_rest() {
    let root = temp_root("help");
    let o = ka(&root, &["--help"]);
    let t = text(&o);
    let commands = t
        .split("Commands:")
        .nth(1)
        .and_then(|rest| rest.split("Options:").next())
        .unwrap_or("");
    let n = commands
        .lines()
        .filter(|l| l.starts_with("  ") && !l.trim().is_empty())
        .count();
    assert!(n <= 16, "{n} commands listed:\n{t}");
    assert!(!t.contains("SEED.md"), "{t}");
    assert!(t.contains("More commands:"), "{t}");
    for hidden in ["grant", "plugin", "purge", "identity", "run"] {
        let o = ka(&root, &[hidden, "--help"]);
        assert!(o.status.success(), "{hidden}: {}", text(&o));
        assert!(!text(&o).contains("SEED.md"), "{hidden}: {}", text(&o));
    }
    let _ = std::fs::remove_dir_all(&root);
}
