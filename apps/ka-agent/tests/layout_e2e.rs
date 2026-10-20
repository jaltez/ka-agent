//! Layout migration e2e: a pre-0.3 install (`~/.local/share/ka-agent` +
//! `~/.config/ka-agent/ka-agent.toml`, XDG vars pinned to a fake HOME) moves
//! wholesale to `~/.ka-agent` on the next run, workspace path included.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

fn home() -> PathBuf {
    // A per-call counter, not just the clock: macOS SystemTime has
    // microsecond granularity, and two parallel tests minting a home in
    // the same microsecond got the *same* directory — one cleanup then
    // deleted the other's tree mid-flight.
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ka-agent-layout-{}-{n}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn ka(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .args(args)
        .env("HOME", home)
        .env_remove("KA_AGENT_DATA_DIR")
        .env_remove("KA_AGENT_CONFIG")
        .env_remove("KA_AGENT_HOME")
        .env_remove("KA_AGENT_PROFILE")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap()
}

#[test]
fn legacy_install_moves_to_ka_agent_home_on_next_run() {
    let home = home();

    // A pre-0.3 instance: config with a workspace under the old root, some
    // data, a secret file with restrictive perms (must survive the move).
    let old_root = home.join(".local/share/ka-agent");
    std::fs::create_dir_all(old_root.join("memory")).unwrap();
    std::fs::create_dir_all(home.join(".config/ka-agent")).unwrap();
    let old_ws = old_root.join("workspace");
    std::fs::write(
        home.join(".config/ka-agent/ka-agent.toml"),
        format!(
            "workspace = {:?}\nmode = \"guarded\"\n",
            old_ws.display().to_string()
        ),
    )
    .unwrap();
    std::fs::write(old_root.join("memory/facts.md"), "- migrated fact\n").unwrap();
    std::fs::write(old_root.join("state.db"), b"placeholder").unwrap();
    let secret = old_root.join("secrets/telegram.bot_token");
    std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
    std::fs::write(&secret, b"tok\n").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();

    // Any ordinary command migrates before doing anything else.
    let out = ka(&home, &["config", "path"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(home.join(".ka-agent/config.toml").to_str().unwrap()),
        "new config path: {stdout}"
    );

    // Everything moved, nothing left behind. Poll briefly: the assert
    // suite ran ahead of the filesystem once on a loaded runner.
    let new_root = home.join(".ka-agent/profiles/default");
    let moved_in = || home.join(".ka-agent/config.toml").is_file();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !moved_in() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(moved_in(), "config never landed in the new base");
    assert!(new_root.join("memory/facts.md").is_file());
    assert!(new_root.join("state.db").is_file());
    assert!(!old_root.exists(), "old data root must be gone");
    assert!(
        !home.join(".config/ka-agent/ka-agent.toml").exists(),
        "old config must be gone"
    );
    assert_eq!(
        std::fs::metadata(new_root.join("secrets/telegram.bot_token"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600,
        "secret perms survive the move"
    );

    // The workspace value followed the data root.
    let cfg = std::fs::read_to_string(home.join(".ka-agent/config.toml")).unwrap();
    let new_ws = new_root.join("workspace").display().to_string();
    assert!(cfg.contains(&new_ws), "workspace rewritten: {cfg}");
    assert!(!cfg.contains(".local/share"), "no stale old path: {cfg}");

    // Second run: idempotent, nothing to do, same answer.
    let out = ka(&home, &["config", "path"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains(".ka-agent/config.toml"));

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn fresh_install_never_touches_legacy_locations() {
    let home = home();
    let out = ka(&home, &["config", "path"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(".ka-agent/config.toml"), "{stdout}");
    assert!(!home.join(".local/share/ka-agent").exists());
    assert!(!home.join(".config/ka-agent").exists());

    // Profiles are selectable without touching `default`.
    let out = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .args(["config", "path"])
        .env("HOME", &home)
        .env("KA_AGENT_PROFILE", "work")
        .env_remove("KA_AGENT_DATA_DIR")
        .env_remove("KA_AGENT_CONFIG")
        .env_remove("KA_AGENT_HOME")
        .output()
        .unwrap();
    assert!(out.status.success());
    // The config is the profile's own …
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(".ka-agent/profiles/work/config.toml"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    // … while the data root lands in the named profile: `identity link`
    // opens <root>/state.db, so the file appearing there is the proof.
    let out = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .args(["identity", "link", "telegram", "1"])
        .env("HOME", &home)
        .env("KA_AGENT_PROFILE", "work")
        .env_remove("KA_AGENT_DATA_DIR")
        .env_remove("KA_AGENT_CONFIG")
        .env_remove("KA_AGENT_HOME")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        home.join(".ka-agent/profiles/work/state.db").is_file(),
        "profile data root used"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn migration_never_lands_in_a_non_default_profile() {
    let home = home();
    let old_root = home.join(".local/share/ka-agent");
    std::fs::create_dir_all(old_root.join("memory")).unwrap();
    std::fs::write(old_root.join("memory/facts.md"), b"- keeps its place\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .args(["identity", "list"])
        .env("HOME", &home)
        .env("KA_AGENT_PROFILE", "work")
        .env_remove("KA_AGENT_DATA_DIR")
        .env_remove("KA_AGENT_CONFIG")
        .env_remove("KA_AGENT_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap();
    assert!(out.status.success());

    // The legacy tree stays exactly where it was …
    if !old_root.join("memory/facts.md").is_file() {
        panic!(
            "legacy tree was moved under a non-default profile; child stdout:\n{}\nstderr:\n{}\ntree:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            std::process::Command::new("find")
                .arg(&home)
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default(),
        );
    }
    // … the work profile got its own fresh root …
    assert!(home.join(".ka-agent/profiles/work").is_dir());
    // … and nothing moved into it.
    assert!(!home.join(".ka-agent/profiles/work/memory").exists());

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn profiles_get_their_own_config_and_the_active_one_sticks() {
    let home = home();
    let base = home.join(".ka-agent");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("config.toml"),
        "model = \"x/y\"\n[telegram]\nenabled = true\ntoken_secret = \"tg\"\n",
    )
    .unwrap();

    let out = ka(&home, &["profile", "create", "work"]);
    assert!(out.status.success(), "{out:?}");
    let work_cfg = base.join("profiles/work/config.toml");
    let text = std::fs::read_to_string(&work_cfg).unwrap();
    assert!(text.contains("model = \"x/y\""), "{text}");
    assert!(
        !text.contains("telegram"),
        "bot token must not be shared: {text}"
    );
    assert!(text.contains("port = 8481"), "{text}");

    let path = |args: &[&str]| {
        let o = ka(&home, args);
        assert!(o.status.success(), "{o:?}");
        String::from_utf8(o.stdout).unwrap().trim().to_string()
    };
    assert_eq!(
        path(&["config", "path"]),
        base.join("config.toml").display().to_string()
    );
    assert_eq!(
        path(&["-p", "work", "config", "path"]),
        work_cfg.display().to_string()
    );

    assert!(ka(&home, &["profile", "use", "work"]).status.success());
    assert_eq!(path(&["config", "path"]), work_cfg.display().to_string());
    assert_eq!(
        path(&["-p", "default", "config", "path"]),
        base.join("config.toml").display().to_string()
    );

    assert!(
        !ka(&home, &["profile", "remove", "default", "--yes"])
            .status
            .success()
    );
    assert!(
        ka(&home, &["profile", "remove", "work", "--yes"])
            .status
            .success()
    );
    assert!(!base.join("profiles/work").exists());
    assert_eq!(
        path(&["config", "path"]),
        base.join("config.toml").display().to_string()
    );
}
