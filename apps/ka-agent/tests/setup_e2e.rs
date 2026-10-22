//! Setup wizard e2e: the scripted `--yes` flow, the piped-stdin flow, and
//! live config edits against a running daemon (plan Verification 9–11).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn temp_root(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-agent-setup-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The wizard runs `ka-agent` with a clean key environment so defaults are
/// deterministic.
fn ka(root: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ka-agent"));
    cmd.env("KA_AGENT_DATA_DIR", root.join("data"))
        .env("KA_AGENT_CONFIG", root.join("ka-agent.toml"))
        .env_remove("KA_MODEL");
    for (k, _) in std::env::vars() {
        if k.ends_with("_API_KEY") {
            cmd.env_remove(k);
        }
    }
    cmd
}

fn config_of(root: &std::path::Path) -> String {
    std::fs::read_to_string(root.join("ka-agent.toml")).unwrap()
}

#[test]
fn setup_yes_flow_writes_config_env_and_doctor_passes_model() {
    let root = temp_root("yes");
    let mut child = ka(&root)
        .args([
            "setup",
            "--provider",
            "anthropic",
            "--model",
            "anthropic/claude-sonnet-5",
            "--key-stdin",
            "--timezone",
            "Europe/Madrid",
            "--no-verify",
            "--no-service",
            "--yes",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"sk-test\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "setup: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // A2: the run ends with the "you're live" card, not a bare "Done".
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("You're live."), "{stdout}");
    assert!(
        stdout.contains("Talk to Ka:") && stdout.contains("Try:"),
        "{stdout}"
    );
    assert!(stdout.contains("remind me at 18:30"), "{stdout}");
    let cfg = config_of(&root);
    assert!(
        cfg.contains("model = \"anthropic/claude-sonnet-5\""),
        "config: {cfg}"
    );
    assert!(
        cfg.contains("timezone = \"Europe/Madrid\""),
        "config: {cfg}"
    );
    let env_path = root.join("data/engine-home/.config/ka/.env");
    let env_text = std::fs::read_to_string(&env_path).unwrap();
    assert!(env_text.contains("ANTHROPIC_API_KEY=sk-test"), "{env_text}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&env_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "engine .env mode is {mode:o}");
    }

    // Doctor now shows the model check green and exits 0.
    let doc = ka(&root).arg("doctor").output().unwrap();
    let text = String::from_utf8_lossy(&doc.stdout);
    assert!(text.contains("model anthropic/claude-sonnet-5"), "{text}");
    assert!(
        text.contains("provider key ANTHROPIC_API_KEY set"),
        "{text}"
    );
    assert_eq!(doc.status.code(), Some(0), "doctor: {text}");
    let _ = out;
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn setup_piped_stdin_answers_each_prompt_and_eof_aborts() {
    let root = temp_root("piped");
    // Full scripted run: key, model default, timezone, workspace, channel
    // "3" (not now), web search default (not now), save, service, test
    // message.
    let script = b"sk-piped\n\ny\n\n3\n\ny\nn\nn\n";
    let mut child = ka(&root)
        .args([
            "setup",
            "--provider",
            "anthropic",
            "--no-verify",
            "--no-service",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(script).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "setup: {}\nstdout:\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    let cfg = config_of(&root);
    assert!(
        cfg.contains("model = \"anthropic/claude-sonnet-5\""),
        "config: {cfg}"
    );
    let env_text = std::fs::read_to_string(root.join("data/engine-home/.config/ka/.env")).unwrap();
    assert!(
        env_text.contains("ANTHROPIC_API_KEY=sk-piped"),
        "{env_text}"
    );

    // EOF mid-wizard aborts cleanly and writes nothing.
    let root2 = temp_root("eof");
    let mut child = ka(&root2)
        .args([
            "setup",
            "--provider",
            "anthropic",
            "--no-verify",
            "--no-service",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"sk-piped\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1), "EOF must abort with exit 1");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("setup aborted (input closed)"), "{stdout}");
    assert!(!root2.join("ka-agent.toml").exists(), "no config on abort");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&root2);
}

#[test]
fn config_edit_is_format_preserving_and_live_with_a_daemon() {
    let root = temp_root("cfg");
    let cfg_path = root.join("ka-agent.toml");
    std::fs::write(
        &cfg_path,
        "# keep me\nmode = \"guarded\"\n\n[budgets]\ndaily_usd = 1.0\n",
    )
    .unwrap();

    // set keeps the comment and rewrites only its own line.
    let out = ka(&root)
        .args(["config", "set", "budgets.daily_usd", "2.5"])
        .output()
        .unwrap();
    assert!(out.status.success(), "config set: {out:?}");
    let text = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(text.contains("# keep me"), "{text}");
    assert!(text.contains("daily_usd = 2.5"), "{text}");

    // Invalid value: refused, file untouched.
    let before = std::fs::read(&cfg_path).unwrap();
    let out = ka(&root)
        .args(["config", "set", "mode", "yolo"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "yolo must be refused");
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown mode"));
    assert_eq!(std::fs::read(&cfg_path).unwrap(), before);

    let out = ka(&root)
        .args(["config", "get", "budgets.daily_usd"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "2.5");

    // Live reload against a running daemon.
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .args([
            "run",
            "--config",
            cfg_path.to_str().unwrap(),
            "--web-port",
            "0",
        ])
        .env("KA_AGENT_DATA_DIR", root.join("data"))
        .env("KA_AGENT_CONFIG", &cfg_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !root.join("data/admin.sock").exists() {
        assert!(Instant::now() < deadline, "daemon never opened its socket");
        std::thread::sleep(Duration::from_millis(50));
    }
    // web.enabled=false would avoid the port race, but --web-port 0 binds an
    // ephemeral port; either way the daemon must be up by now.

    let out = ka(&root)
        .args(["config", "set", "worker_idle_timeout_secs", "60"])
        .env("KA_AGENT_CONFIG", &cfg_path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "set: {text}");
    assert!(text.contains("applied live"), "{text}");

    let out = ka(&root)
        .args([
            "config",
            "set",
            "workspace",
            &root.join("other-ws").display().to_string(),
        ])
        .env("KA_AGENT_CONFIG", &cfg_path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "set workspace: {text}");
    assert!(text.contains("restart needed for: workspace"), "{text}");

    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = std::fs::remove_dir_all(&root);
}

/// Regression: the wizard accepted a *correct* bot token (getMe answered)
/// and then aborted writing the config — `telegram.enabled` was written
/// before `telegram.token_secret`, and every `config set` validates the
/// whole file. The token itself was never the problem.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_telegram_section_writes_config_with_a_verified_token() {
    use ka_agent_channels::fake::FakeBotServer;
    use std::io::Write as _;

    let root = temp_root("tg-write");
    let (fake, base) = FakeBotServer::spawn();
    std::fs::write(
        root.join("ka-agent.toml"),
        format!("[telegram]\napi_base = {base:?}\n"),
    )
    .unwrap();

    let mut child = ka(&root)
        .args(["setup", "telegram"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"123:correct-token\ny\n")
        .unwrap();
    // Pair for real (instead of waiting out the code's TTL): read the code
    // the wizard prints and send it from the fake Bot API's "phone".
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        use std::io::BufRead as _;
        let mut all = String::new();
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if let Some(code) = line.split("/pair ").nth(1) {
                let _ = tx.send(code.trim().to_string());
            }
            all.push_str(&line);
            all.push('\n');
        }
        all
    });
    let code = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("wizard printed a pairing code");
    fake.push_text_message(1, 42, 42, &format!("/pair {code}"));
    let out = child.wait_with_output().unwrap();
    let text = format!(
        "{}{}",
        reader.join().unwrap(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("connected to @fake_bot"), "{text}");
    assert!(!text.contains("write failed"), "{text}");
    assert!(text.contains("paired with"), "{text}");

    // The config landed with both keys, in a state validation accepts.
    let cfg = config_of(&root);
    assert!(
        cfg.contains("token_secret = \"telegram.bot_token\""),
        "config: {cfg}"
    );
    assert!(cfg.contains("enabled = true"), "config: {cfg}");
    ka_agent_core::DaemonConfig::load(&root.join("ka-agent.toml")).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

/// A3: `setup search` turns on web search — provider in the config, key in
/// the engine `.env` the worker reads — and doctor confirms it.
#[test]
fn setup_search_section_writes_provider_and_key() {
    let root = temp_root("search");
    std::fs::write(root.join("ka-agent.toml"), "mode = \"guarded\"\n").unwrap();
    let mut child = ka(&root)
        .args(["setup", "search"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"1\ntvly-test-key\ny\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("Web search is on"), "{text}");
    let cfg = config_of(&root);
    assert!(
        cfg.contains("[search]") && cfg.contains("provider = \"tavily\""),
        "{cfg}"
    );
    let env = std::fs::read_to_string(root.join("data/engine-home/.config/ka/.env")).unwrap();
    assert!(env.contains("TAVILY_API_KEY=tvly-test-key"), "{env}");
    let doctor = ka(&root).arg("doctor").output().unwrap();
    let d = String::from_utf8_lossy(&doctor.stdout);
    assert!(d.contains("✓ web search tavily (key set)"), "{d}");
    let _ = std::fs::remove_dir_all(&root);
}
