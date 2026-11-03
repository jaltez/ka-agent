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
    for args in [
        &["chat"][..],
        &["run"][..],
        &["mcp", "list"][..],
        &["status"][..],
    ] {
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
    // C1: the everyday set stays ≤ 16 rows; the rest moved to the footer.
    assert!(n <= 16, "{n} commands listed:\n{t}");
    assert!(!t.contains("SEED.md"), "{t}");
    assert!(t.contains("More commands:"), "{t}");
    // C3: `import` is named in the footer, not left undocumented.
    assert!(t.contains("import"), "{t}");
    for hidden in [
        "grant", "plugin", "purge", "identity", "run", "notify", "hooks", "persona", "mcp",
        "usage", "memory", "import",
    ] {
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
        let asset = format!("ka-agent-{triple}.tar.gz");
        let index = serde_json::json!({
            "tag_name": "v9.9.9",
            "assets": [{
                "name": asset,
                "browser_download_url": format!("http://127.0.0.1:{port}/t.tar.gz")
            }]
        });
        let meta = serde_json::to_vec(&index).unwrap();
        // Real releases ship `shasum -a 256` output: `HEX  FILENAME`.
        let checksum = if good_checksum {
            format!("{digest}  {asset}\n").into_bytes()
        } else {
            format!("{}deadbeef  {asset}\n", &digest[..8]).into_bytes()
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
        // The data root must be the test's own: `update` records its check
        // in the store, and without this the test writes into the real
        // instance's state.db (doctor then reports a bogus 9.9.9 release).
        let o = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
            .args(args)
            .env("KA_AGENT_DATA_DIR", dir.join("data"))
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
    assert!(
        ok && text.contains("→ 9.9.9"),
        "current → target on one line: {text}"
    );
    assert_eq!(text.lines().count(), 1, "check output is one line: {text}");
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

/// Config CLI: a whole registered section is addressable (`get` renders its
/// TOML body, `unset` removes the table), while leaf behavior is unchanged.
#[test]
fn config_get_and_unset_address_whole_sections() {
    let root = temp_root("sections");
    std::fs::write(
        root.join("config.toml"),
        "# keep\nmode = \"guarded\"\n\n[search]\nprovider = \"tavily\"\nbase_url = \"https://api.tavily.com\"\n\n[voice]\nprovider = \"local\"\nbase_url = \"http://127.0.0.1:8000/v1\"\n\n[budgets]\ndaily_usd = 2.0\n",
    )
    .unwrap();

    let o = ka(&root, &["config", "get", "search"]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains("provider = \"tavily\"")
            && out.contains("base_url = \"https://api.tavily.com\""),
        "{out}"
    );

    // Leaves keep working.
    let o = ka(&root, &["config", "get", "search.provider"]);
    assert_eq!(text(&o).trim(), "tavily");
    let o = ka(&root, &["config", "set", "search.provider", "brave"]);
    assert!(o.status.success(), "{}", text(&o));
    let cfg = ka_agent_core::DaemonConfig::load(&root.join("config.toml")).unwrap();
    assert_eq!(
        cfg.search.as_ref().map(|s| s.provider.as_str()),
        Some("brave")
    );

    // Unset the whole table: gone, the rest untouched, and still valid.
    let o = ka(&root, &["config", "unset", "search"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("removed search"), "{}", text(&o));
    let cfg_text = std::fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(!cfg_text.contains("[search]"), "{cfg_text}");
    assert!(cfg_text.contains("# keep"), "{cfg_text}");
    assert!(cfg_text.contains("[voice]"), "{cfg_text}");
    assert!(cfg_text.contains("daily_usd = 2.0"), "{cfg_text}");
    assert!(
        ka_agent_core::DaemonConfig::load(&root.join("config.toml")).is_ok(),
        "{cfg_text}"
    );

    let o = ka(&root, &["config", "get", "search"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("section absent"), "{}", text(&o));

    // An unknown key is still refused.
    let o = ka(&root, &["config", "get", "nope"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("unknown key"), "{}", text(&o));

    // A second section is removable the same way.
    let o = ka(&root, &["config", "unset", "voice"]);
    assert!(o.status.success(), "{}", text(&o));
    let cfg_text = std::fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(!cfg_text.contains("[voice]"), "{cfg_text}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A one-shot fake HTTP server: answers connections with `answers`
/// (one each, in order), records every request it saw, and stops
/// listening after the last answer — the URL then refuses connections,
/// which is how the tests go "offline".
struct FakeHttp {
    url: String,
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

fn fake_http(answers: Vec<(u16, &'static str)>) -> FakeHttp {
    use std::io::Write as _;
    fn read_request(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read as _;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let head = String::from_utf8_lossy(&buf);
            let Some(end) = head.find("\r\n\r\n") else {
                continue;
            };
            let len: usize = head[..end]
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .starts_with("content-length:")
                        .then(|| {
                            l.split(':')
                                .nth(1)
                                .unwrap_or("0")
                                .trim()
                                .parse()
                                .unwrap_or(0)
                        })
                })
                .unwrap_or(0);
            if buf.len() >= end + 4 + len {
                break;
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = seen.clone();
    std::thread::spawn(move || {
        for (i, mut stream) in listener.incoming().flatten().enumerate() {
            let req = read_request(&mut stream);
            recorder.lock().unwrap().push(req);
            let Some((status, body)) = answers.get(i) else {
                break;
            };
            let reason = if *status == 200 { "OK" } else { "Bad Request" };
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
            if i + 1 == answers.len() {
                break; // drop the listener: further connects are refused
            }
        }
    });
    FakeHttp {
        url: format!("http://127.0.0.1:{port}"),
        seen,
    }
}

/// `ka-agent` with an isolated home plus extra environment (the fake
/// registry/gateway URLs). Provider keys are stripped so the key state
/// in `models` output and the probe's no-key path are deterministic.
fn ka_env(root: &Path, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ka-agent"));
    cmd.args(args)
        .env("HOME", root)
        .env("KA_AGENT_DATA_DIR", root.join("data"))
        .env("KA_AGENT_CONFIG", root.join("config.toml"))
        .env_remove("KA_AGENT_PROFILE")
        .env_remove("KA_AGENT_HOME")
        .env_remove("OPENCODE_API_KEY");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

/// The live models.dev registry merged into `ka-agent models`: new
/// models lead the list, the fetch is cached, and the footer says
/// where the live half came from.
#[test]
fn models_merges_the_registry_and_writes_the_cache() {
    let root = temp_root("reg-merge");
    let reg = fake_http(vec![(
        200,
        r#"{"opencode": {"models": {"new-shiny": {}, "alpha-free": {}}},
            "anthropic": {"models": {"claude-sonnet-5": {}}}}"#,
    )]);
    let o = ka_env(
        &root,
        &["models", "opencode-go"],
        &[("KA_AGENT_MODELS_REGISTRY", &reg.url)],
    );
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("opencode-go  key "), "{out}");
    assert_eq!(
        lines[1], "  opencode-go/alpha-free",
        "registry models lead, sorted: {out}"
    );
    assert_eq!(lines[2], "  opencode-go/new-shiny", "{out}");
    assert_eq!(
        lines[3], "  opencode-go/glm-5.3",
        "catalog models follow (default first): {out}"
    );
    let footer = lines.iter().find(|l| l.starts_with("live ")).unwrap_or(&"");
    assert!(
        footer.starts_with("live 2 from models.dev ") && !footer.contains("(cached)"),
        "fresh fetch footer: {out}"
    );
    // The raw body and its fetched-at marker are cached under the data
    // root.
    let cache = root.join("data/cache/models-dev.json");
    assert!(
        std::fs::read_to_string(&cache)
            .unwrap_or_default()
            .contains("new-shiny"),
        "cache not written: {}",
        cache.display()
    );
    assert!(root.join("data/cache/models-dev.json.at").exists());
    let _ = std::fs::remove_dir_all(&root);
}

/// Offline, the cached registry keeps answering (marked `(cached)`),
/// and with no cache at all the listing is the pinned catalog —
/// default first, footer saying the live registry did not answer.
#[test]
fn models_serves_the_cache_when_the_registry_is_unreachable() {
    let root = temp_root("reg-cache");
    // One answer, then the listener closes: the second run is offline.
    let reg = fake_http(vec![(
        200,
        r#"{"opencode": {"models": {"new-shiny": {}}}}"#,
    )]);
    let o = ka_env(
        &root,
        &["models", "opencode-go"],
        &[("KA_AGENT_MODELS_REGISTRY", &reg.url)],
    );
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("  opencode-go/new-shiny"), "{}", text(&o));

    let o = ka_env(
        &root,
        &["models", "opencode-go"],
        &[("KA_AGENT_MODELS_REGISTRY", &reg.url)],
    );
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains("  opencode-go/new-shiny"),
        "the cache still answers: {out}"
    );
    assert!(
        out.lines()
            .any(|l| l.starts_with("live 1 from models.dev ") && l.ends_with(" (cached)")),
        "cached footer: {out}"
    );

    // Offline with no cache at all: the pinned catalog listing
    // (header, default model first) plus a footer saying the live
    // registry did not answer.
    let bare = temp_root("reg-none");
    let o = ka_env(
        &bare,
        &["models", "opencode-go"],
        &[("KA_AGENT_MODELS_REGISTRY", "http://127.0.0.1:9/api.json")],
    );
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert_eq!(
        out.lines().next().unwrap(),
        "opencode-go  key ✗  OpenCode Go subscription gateway — open coding models",
        "{out}"
    );
    assert_eq!(
        out.lines().nth(1).unwrap(),
        "  opencode-go/glm-5.3",
        "the default leads as before: {out}"
    );
    assert!(
        !out.contains("models.dev"),
        "no live-registry footer when offline without a cache: {out}"
    );
    assert!(
        out.trim_end()
            .ends_with("catalog only (live registry unreachable)"),
        "the offline footer says so: {out}"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&bare);
}

/// `models --probe <vendor/model>`: one real request; a dead model
/// reports the gateway's own error message verbatim.
#[test]
fn probe_reports_a_dead_model_with_the_gateway_message() {
    let root = temp_root("probe-dead");
    let gw = fake_http(vec![(
        400,
        r#"{"error":{"type":"server_error","message":"Upstream request failed: Model is unavailable."}}"#,
    )]);
    let o = ka_env(
        &root,
        &["models", "--probe", "opencode-go/cat-jam-free"],
        &[
            ("KA_AGENT_PROBE_URL", &gw.url),
            ("OPENCODE_API_KEY", "sk-test"),
        ],
    );
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(
        out.trim_end().ends_with(
            "✗ opencode-go/cat-jam-free: Upstream request failed: Model is unavailable."
        ),
        "{out}"
    );
    // What left the process was one minimal chat completion with the
    // gateway's session-routing header.
    let seen = gw.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "exactly one request: {seen:?}");
    let req = &seen[0];
    assert!(req.starts_with("POST /chat/completions"), "{req}");
    assert!(
        req.to_ascii_lowercase()
            .contains("x-opencode-session: ka-agent-doctor"),
        "{req}"
    );
    assert!(req.contains("Bearer sk-test"), "{req}");
    assert!(req.contains("\"max_tokens\":1"), "{req}");
    let _ = std::fs::remove_dir_all(&root);
}

/// `models --probe` without a key sends nothing and says so, naming
/// the env var that fixes it.
#[test]
fn probe_without_a_key_is_not_probed() {
    let root = temp_root("probe-nokey");
    // The URL is unreachable on purpose: without a key the probe must
    // not touch the network at all.
    let o = ka_env(
        &root,
        &["models", "--probe", "opencode-go/glm-5.3"],
        &[("KA_AGENT_PROBE_URL", "http://127.0.0.1:9/v1")],
    );
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(
        out.trim_end().ends_with(
            "· opencode-go/glm-5.3 not probed (no key) — ka-agent key set OPENCODE_API_KEY"
        ),
        "{out}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The setup picker offers the registry's models ahead of the catalog
/// (its default pick becomes the registry's first model), and the
/// answer still goes through the catalog's selector validation.
#[test]
fn setup_picker_offers_registry_models_first() {
    use std::io::Write as _;
    let root = temp_root("pick-reg");
    let reg = fake_http(vec![(
        200,
        r#"{"anthropic": {"models": {"claude-super-9": {}}}}"#,
    )]);
    let mut child = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .args([
            "setup",
            "--provider",
            "anthropic",
            "--key-stdin",
            "--no-verify",
            "--no-service",
        ])
        .env("HOME", &root)
        .env("KA_AGENT_DATA_DIR", root.join("data"))
        .env("KA_AGENT_CONFIG", root.join("config.toml"))
        .env("KA_AGENT_MODELS_REGISTRY", &reg.url)
        .env_remove("KA_AGENT_PROFILE")
        .env_remove("KA_AGENT_HOME")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // key, model default pick, timezone, workspace, channels, search,
    // briefing, save, service, test message — the piped flow.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"sk-pick\n\ny\n\n3\n\n\ny\nn\nn\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    let cfg = std::fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(
        cfg.contains("model = \"anthropic/claude-super-9\""),
        "the registry's first model is the default pick: {cfg}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// D1: `grant path add|list|rm` edits `[[grants.path]]` row by row, and
/// `config unset grants.path` never bulk-deletes silently.
#[test]
fn grant_path_add_list_rm_round_trip() {
    let root = temp_root("grantpath");
    std::fs::write(root.join("config.toml"), "# keep\nmode = \"ask\"\n").unwrap();

    let o = ka(&root, &["grant", "path", "add", "/tmp/proj"]);
    assert!(o.status.success(), "{}", text(&o));
    let o = ka(
        &root,
        &["grant", "path", "add", "/tmp/other", "--access", "ask"],
    );
    assert!(o.status.success(), "{}", text(&o));
    let cfg = ka_agent_core::DaemonConfig::load(&root.join("config.toml")).unwrap();
    assert_eq!(cfg.grants.path.len(), 2);
    assert_eq!(cfg.grants.path[0].dir, std::path::Path::new("/tmp/proj"));
    assert!(o.status.success());

    let out = text(&ka(&root, &["config", "get", "grants.path"]));
    assert!(out.contains("/tmp/proj") && out.contains("allow"), "{out}");
    let out = text(&ka(&root, &["grant", "path", "list"]));
    assert!(out.contains("ask   /tmp/other"), "{out}");
    // The operator's comment survives every write.
    assert!(
        std::fs::read_to_string(root.join("config.toml"))
            .unwrap()
            .contains("# keep")
    );

    // Re-adding the same dir re-aims it (validation forbids duplicates).
    assert!(
        ka(
            &root,
            &["grant", "path", "add", "/tmp/proj", "--access", "deny"]
        )
        .status
        .success()
    );
    let cfg = ka_agent_core::DaemonConfig::load(&root.join("config.toml")).unwrap();
    assert_eq!(cfg.grants.path.len(), 2);
    assert_eq!(
        cfg.grants.path[0].access,
        ka_agent_core::config::Access::Deny
    );

    // rm removes exactly one row.
    assert!(
        ka(&root, &["grant", "path", "rm", "/tmp/proj"])
            .status
            .success()
    );
    let cfg = ka_agent_core::DaemonConfig::load(&root.join("config.toml")).unwrap();
    assert_eq!(cfg.grants.path.len(), 1);
    assert_eq!(cfg.grants.path[0].dir, std::path::Path::new("/tmp/other"));
    assert!(
        !ka(&root, &["grant", "path", "rm", "/nope"])
            .status
            .success()
    );

    // D1: unset of the table key lists what it would remove and refuses
    // without a terminal.
    let o = ka(&root, &["config", "unset", "grants.path"]);
    assert!(!o.status.success(), "{}", text(&o));
    let out = text(&o);
    assert!(out.contains("1 row") && out.contains("/tmp/other"), "{out}");
    assert!(out.contains("without a terminal"), "{out}");
    let cfg = ka_agent_core::DaemonConfig::load(&root.join("config.toml")).unwrap();
    assert_eq!(cfg.grants.path.len(), 1, "the refusal left the file alone");

    // D12: the scope store's empty state points at the config grants.
    let out = text(&ka(&root, &["grant", "list"]));
    assert!(out.contains("no tool scopes"), "{out}");
    assert!(out.contains("[[grants.path]]"), "{out}");
    let _ = std::fs::remove_dir_all(&root);
}

/// D2: the schema's bounds are exactly what `config set` accepts; D9:
/// `config get` names the default; D8: `language` is a validated enum.
#[test]
fn schema_bounds_defaults_and_language() {
    let root = temp_root("bounds");
    std::fs::write(root.join("config.toml"), "mode = \"ask\"\n").unwrap();

    // The old registry bound (30) refused this; validate() accepts it.
    let o = ka(&root, &["config", "set", "approval_ttl_secs", "10"]);
    assert!(o.status.success(), "{}", text(&o));
    let schema = text(&ka(&root, &["config", "schema"]));
    assert!(
        schema.contains("approval_ttl_secs · int 1..=86400"),
        "{schema}"
    );
    assert!(
        schema.contains("worker_idle_timeout_secs · int 0..=604800"),
        "{schema}"
    );

    // D9: unset keys name their default from the registry.
    let out = text(&ka(&root, &["config", "get", "mode"]));
    // mode IS set here; use a key that is not.
    assert!(out.trim() == "ask", "{out}");
    let out = text(&ka(&root, &["config", "get", "effort"]));
    assert!(
        out.contains("effort is not set (default: unset (model default))"),
        "{out}"
    );

    // D8: language accepts en|es and nothing else.
    let o = ka(&root, &["config", "set", "language", "es"]);
    assert!(o.status.success(), "{}", text(&o));
    let cfg = ka_agent_core::DaemonConfig::load(&root.join("config.toml")).unwrap();
    assert_eq!(cfg.language, "es");
    assert!(cfg.language_preamble().contains("español"));
    let o = ka(&root, &["config", "set", "language", "fr"]);
    assert!(!o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("language must be"), "{}", text(&o));
    let cfg = ka_agent_core::DaemonConfig::load(&root.join("config.toml")).unwrap();
    assert_eq!(cfg.language, "es", "the refusal left the file alone");
    let _ = std::fs::remove_dir_all(&root);
}

/// D5: a broken engine layer is refused at set time; D10: every write
/// keeps the previous file one generation as `<config>.bak`.
#[test]
fn engine_layer_checked_at_set_time_and_writes_keep_a_backup() {
    let root = temp_root("englayer");
    std::fs::write(root.join("config.toml"), "mode = \"ask\"\n").unwrap();

    let o = ka(
        &root,
        &["config", "set", "ka.engine_layer", "/nonexistent/ka.toml"],
    );
    assert!(!o.status.success(), "{}", text(&o));
    assert!(
        text(&o).contains("cannot read /nonexistent/ka.toml"),
        "{}",
        text(&o)
    );
    assert!(
        !std::fs::read_to_string(root.join("config.toml"))
            .unwrap()
            .contains("engine_layer")
    );

    // A present but broken TOML layer is refused too.
    std::fs::write(root.join("bad.toml"), "not toml =").unwrap();
    let o = ka(
        &root,
        &[
            "config",
            "set",
            "ka.engine_layer",
            &root.join("bad.toml").display().to_string(),
        ],
    );
    assert!(!o.status.success(), "{}", text(&o));

    // D10: one-generation backup on every config write.
    let o = ka(&root, &["config", "set", "mode", "allow"]);
    assert!(o.status.success(), "{}", text(&o));
    let bak = root.join("config.toml.bak");
    assert!(
        std::fs::read_to_string(&bak)
            .unwrap()
            .contains("mode = \"ask\"")
    );
    let o = ka(&root, &["config", "set", "mode", "readonly"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(
        std::fs::read_to_string(&bak)
            .unwrap()
            .contains("mode = \"allow\"")
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// D4: `mcp test` proves a server answers at setup time — a live stdio
/// server reports its tools, a dead URL fails naming the server.
#[test]
fn mcp_test_probes_servers_and_names_failures() {
    let root = temp_root("mcptest");
    std::fs::write(root.join("config.toml"), "mode = \"ask\"\n").unwrap();

    let o = ka(
        &root,
        &["mcp", "add", "dead", "--url", "http://127.0.0.1:1/mcp"],
    );
    assert!(o.status.success(), "{}", text(&o));
    let o = ka(&root, &["mcp", "test", "dead"]);
    assert!(!o.status.success(), "{}", text(&o));
    let out = text(&o);
    assert!(out.contains("dead") && out.contains("FAILED"), "{out}");

    let o = ka(&root, &["mcp", "test", "nope"]);
    assert!(!o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("no MCP server \"nope\""), "{}", text(&o));

    // A minimal line-oriented MCP server: initialize + tools/list.
    let script = root.join("mcp-server.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *initialize*) echo '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"serverInfo\":{\"name\":\"t\",\"version\":\"0\"}}}';;\n    *tools/list*) echo '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"echo\"},{\"name\":\"note\"}]}}';;\n  esac\ndone\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let o = ka(
            &root,
            &["mcp", "add", "demo", "--", &script.display().to_string()],
        );
        assert!(o.status.success(), "{}", text(&o));
        let o = ka(&root, &["mcp", "test", "demo"]);
        assert!(o.status.success(), "{}", text(&o));
        assert!(text(&o).contains("2 tool"), "{}", text(&o));
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// C2: fresh-root `status` points at `setup` (not `service install`), rc 1.
#[test]
fn fresh_status_points_at_setup_not_the_service() {
    let root = temp_root("fresh-status");
    let o = ka(&root, &["status"]);
    let t = text(&o);
    assert_eq!(o.status.code(), Some(1), "{t}");
    assert!(t.contains("isn't set up yet"), "{t}");
    assert!(!t.contains("service install"), "{t}");
    let _ = std::fs::remove_dir_all(&root);
}

/// D13: `doctor --fix` tightens loose data/secrets permissions and says so.
#[test]
fn doctor_fix_tightens_loose_permissions() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("doctor-fix");
        let secrets = root.join("data/secrets");
        std::fs::create_dir_all(&secrets).unwrap();
        let token = secrets.join("telegram.bot_token");
        std::fs::write(&token, "1:test").unwrap();
        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
        let o = ka(&root, &["doctor", "--fix"]);
        let t = text(&o);
        assert!(t.contains("fixed: chmod 700"), "{t}");
        assert!(t.contains("chmod 600"), "{t}");
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode();
        assert_eq!(mode(&secrets) & 0o777, 0o700);
        assert_eq!(mode(&token) & 0o777, 0o600);
        // The next plain run is green on that line.
        let t = text(&ka(&root, &["doctor"]));
        assert!(!t.contains("readable by other users"), "{t}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
