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

/// B2: every `--json` output is one valid JSON document, success or not.
#[test]
fn json_outputs_are_single_valid_documents() {
    let root = temp_root("json");
    std::fs::write(root.join("config.toml"), "mode = \"guarded\"\n").unwrap();
    for args in [
        &["doctor", "--json"][..],
        &["status", "--json"][..],
        &["usage", "--json"][..],
        &["cron", "list", "--json"][..],
        &["profile", "list", "--json"][..],
    ] {
        let o = ka(&root, args);
        let out = String::from_utf8_lossy(&o.stdout);
        let v: serde_json::Value = serde_json::from_str(out.trim())
            .unwrap_or_else(|e| panic!("{args:?}: not one JSON document ({e}):\n{out}"));
        assert!(v.get("ok").is_some_and(|b| b.is_boolean()), "{args:?}: {v}");
        assert_eq!(
            v["ok"].as_bool(),
            Some(o.status.success()),
            "{args:?}: exit code agrees"
        );
    }
    // The checks carry level + text.
    let v: serde_json::Value =
        serde_json::from_slice(&ka(&root, &["doctor", "--json"]).stdout).unwrap();
    assert!(
        v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["level"].is_string() && c["text"].is_string())
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// B4: completions generate, and bash completes profile names after -p.
#[test]
fn bash_completion_offers_profile_names() {
    let root = temp_root("compl");
    let o = ka(&root, &["completions", "bash"]);
    assert!(o.status.success());
    let script = root.join("ka.bash");
    std::fs::write(&script, &o.stdout).unwrap();
    assert!(String::from_utf8_lossy(&o.stdout).contains("_ka__agent()"));
    for shell in ["zsh", "fish", "powershell"] {
        assert!(
            ka(&root, &["completions", shell]).status.success(),
            "{shell}"
        );
    }
    std::fs::create_dir_all(root.join(".ka-agent/profiles/work")).unwrap();
    std::fs::create_dir_all(root.join(".ka-agent/profiles/home")).unwrap();
    let probe = format!(
        "source {s}; COMP_WORDS=(ka-agent -p w); COMP_CWORD=2; _ka__agent ka-agent w -p; echo \"${{COMPREPLY[@]}}\"",
        s = script.display()
    );
    let out = Command::new("bash")
        .args(["-c", &probe])
        .env("HOME", &root)
        .env_remove("KA_AGENT_HOME")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "work",
        "{out:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// K1: `update` against fixture release servers — check reports the newer
/// version and downloads nothing; install checksum-verifies and swaps the
/// target; a mismatched checksum installs nothing.
#[test]
fn update_downloads_verifies_and_swaps() {
    use std::io::{Read as _, Write as _};
    let dir = temp_root("update");
    let staging = dir.join("rel");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(
        staging.join("ka-agent"),
        "#!/bin/sh\necho i am the update\n",
    )
    .unwrap();
    let tar_out = std::process::Command::new("tar")
        .arg("czf")
        .arg(staging.join("t.tar.gz"))
        .arg("-C")
        .arg(&staging)
        .arg("ka-agent")
        .output()
        .unwrap();
    assert!(tar_out.status.success());
    let tarball = std::fs::read(staging.join("t.tar.gz")).unwrap();
    use sha2::{Digest, Sha256};
    let digest: String = Sha256::digest(&tarball)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    // One fake release server; `checksum` may be wrong on purpose.
    let serve = move |good_checksum: bool| -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // The asset name the updater asks for on THIS machine.
        let triple = format!(
            "{}-{}",
            std::env::consts::ARCH,
            match std::env::consts::OS {
                "linux" => "unknown-linux-gnu",
                "macos" => "apple-darwin",
                other => other,
            }
        );
        let index = serde_json::json!({
            "tag_name": "v9.9.9",
            "assets": [{
                "name": format!("ka-agent-{triple}.tar.gz"),
                "browser_download_url": format!("http://127.0.0.1:{port}/t.tar.gz")
            }]
        });
        let meta = serde_json::to_vec(&index).unwrap();
        let checksum = if good_checksum {
            format!("{digest}\n").into_bytes()
        } else {
            format!("{}deadbeef\n", &digest[..8]).into_bytes()
        };
        let tarball = tarball.clone();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let (head, body): (&str, Vec<u8>) = if buf.starts_with(b"GET /meta") {
                    ("application/json", meta.clone())
                } else if buf.starts_with(b"GET /t.tar.gz.sha256") {
                    ("text/plain", checksum.clone())
                } else {
                    ("application/octet-stream", tarball.clone())
                };
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                );
                let _ = stream.write_all(&body);
            }
        });
        format!("http://127.0.0.1:{port}/meta")
    };
    let good_api = serve(true);
    let bad_api = serve(false);

    let target = dir.join("bin/ka-agent");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "old binary").unwrap();
    let run = |api: &str, args: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
            .args(args)
            .env("KA_AGENT_UPDATE_API", api)
            .env("KA_AGENT_INSTALL_TO", &target)
            .output()
            .unwrap();
        (
            o.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        )
    };

    let (ok, text) = run(&good_api, &["update", "check"]);
    assert!(ok && text.contains("update available"), "{text}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old binary");

    let (ok, text) = run(&bad_api, &["update", "run"]);
    assert!(!ok, "a bad checksum must fail: {text}");
    assert!(text.contains("mismatch"), "{text}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old binary");

    let (ok, text) = run(&good_api, &["update", "run"]);
    assert!(ok, "{text}");
    assert!(
        std::fs::read_to_string(&target)
            .unwrap()
            .contains("i am the update"),
        "target swapped: {}",
        std::fs::read_to_string(&target).unwrap()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A4: import turns another agent's memory into candidates (never straight
/// into stable memory) and its skills into library entries; --dry-run
/// writes nothing.
#[test]
fn import_brings_memory_as_candidates_and_skills() {
    let home = temp_root("import-home");
    // A fake Hermes install.
    std::fs::create_dir_all(home.join(".hermes/memories")).unwrap();
    std::fs::create_dir_all(home.join(".hermes/skills/research")).unwrap();
    std::fs::write(
        home.join(".hermes/memories/MEMORY.md"),
        "# Memory\n\n- Prefers short answers\n- Lives in Madrid\n\nsome prose line about tea\n",
    )
    .unwrap();
    std::fs::write(
        home.join(".hermes/skills/research/SKILL.md"),
        "---\nname: research\n---\nDo research.\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .args(["import", "agent", "hermes", "--dry-run"])
        .env("HOME", &home)
        .env("KA_AGENT_DATA_DIR", home.join("data"))
        .env("KA_AGENT_CONFIG", home.join("config.toml"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("3 fact(s)"), "{text}");
    assert!(text.contains("dry run — nothing was written"), "{text}");
    assert!(!home.join("data/state.db").exists(), "nothing written");

    let out = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .args(["import", "agent", "hermes"])
        .env("HOME", &home)
        .env("KA_AGENT_DATA_DIR", home.join("data"))
        .env("KA_AGENT_CONFIG", home.join("config.toml"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    let store = ka_agent_store::Store::open(&home.join("data/state.db")).unwrap();
    let pending = store.pending_candidates_full().unwrap();
    assert_eq!(pending.len(), 3, "{pending:?}");
    assert!(pending.iter().all(|c| c.source == "import"), "{pending:?}");
    assert!(
        std::fs::read_to_string(home.join(".ka-agent/skills/research/SKILL.md")).is_ok(),
        "skill copied into the library"
    );
    let _ = std::fs::remove_dir_all(&home);
}
