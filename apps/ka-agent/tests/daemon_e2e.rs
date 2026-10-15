//! Daemon e2e (no channels): `ka-agent run` with Telegram disabled —
//! admin socket appears, answers `{"op":"status"}` with live runtime state,
//! socket mode is 0600, and SIGTERM exits 0.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn temp_root(tag: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("ka-agent-daemon-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Daemon {
    child: Child,
    data: std::path::PathBuf,
}

impl Daemon {
    fn sock(&self) -> std::path::PathBuf {
        self.data.join("admin.sock")
    }

    fn wait_sock(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.sock().exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "admin socket did not appear within {timeout:?} at {}",
            self.sock().display()
        );
    }

    /// SIGTERM the child and wait (bounded) for a clean exit.
    fn sigterm_wait(&mut self, timeout: Duration) {
        std::process::Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status()
            .unwrap();
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait().unwrap() {
                Some(code) => {
                    assert!(code.success(), "exit was {code:?}");
                    return;
                }
                None if Instant::now() > deadline => {
                    panic!("daemon did not exit within {timeout:?} after SIGTERM");
                }
                None => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_daemon(root: &std::path::Path) -> Daemon {
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = root.join("ka-agent.toml");
    std::fs::write(
        &cfg,
        format!(
            "workspace = {:?}\nmode = \"guarded\"\nturn_grace_secs = 5\n",
            ws.display().to_string()
        ),
    )
    .unwrap();
    let data = root.join("data");
    let child = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .arg("run")
        .arg("--config")
        .arg(&cfg)
        .env("KA_AGENT_DATA_DIR", &data)
        .env_remove("KA_MODEL")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    Daemon { child, data }
}

fn status_over(sock: &std::path::Path) -> serde_json::Value {
    let mut stream = std::os::unix::net::UnixStream::connect(sock).unwrap();
    stream.write_all(b"{\"op\":\"status\"}\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(line.trim()).unwrap()
}

#[test]
fn admin_socket_status_and_clean_sigterm() {
    let root = temp_root("status");
    let mut daemon = spawn_daemon(&root);
    daemon.wait_sock(Duration::from_secs(10));

    // Socket must be owner-only.
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(daemon.sock())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "socket mode is {mode:o}");
    }

    let v = status_over(&daemon.sock());
    assert_eq!(v["ok"], serde_json::json!(true));
    assert!(v["session"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(v["status"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(v["queue"].is_u64());
    assert!(v["crashes"].is_u64());

    // Unknown op → error.
    let mut stream = std::os::unix::net::UnixStream::connect(daemon.sock()).unwrap();
    stream.write_all(b"{\"op\":\"nope\"}\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["error"], serde_json::json!("unknown op"));

    // SIGTERM → exit 0.
    daemon.sigterm_wait(Duration::from_secs(10));
    std::fs::remove_dir_all(&root).ok();
}
