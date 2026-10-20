//! The sandbox: one plugin tool call = one bubblewrap-wrapped MCP-stdio
//! subprocess (SEED.md §6.4).
//!
//! Profile: read-only system dirs, tmpfs `/tmp` and `/home`, `--unshare-net`
//! (plugins have no egress in this host version), `--die-with-parent`,
//! `--clearenv` — the process sees only `PATH`/`HOME` and **its declared
//! secrets**, never other plugins' secrets, the bot token or provider keys.
//! Every call has a wall-clock limit (watchdog `kill -9`, no unsafe) and an
//! output cap.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Wall clock for one plugin tool call, start to answer.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// A plugin answer longer than this is truncated (it lands in engine context).
pub const OUTPUT_CAP: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("sandbox: bubblewrap not found — install it (apt install bubblewrap)")]
    NoBubblewrap,
    #[error("sandbox spawn: {0}")]
    Spawn(String),
    #[error("plugin protocol: {0}")]
    Protocol(String),
    #[error("plugin timed out after {CALL_TIMEOUT:?}")]
    Timeout,
    #[error("plugin failed: {0}")]
    Failed(String),
}

/// A plugin execution request, resolved by the caller (the bridge).
#[derive(Debug, Clone)]
pub struct Sandbox {
    /// The plugin's installed directory (mounted read-only at `/plugin`).
    pub plugin_root: PathBuf,
    /// Entry command as declared (`/plugin/…`).
    pub command: String,
    pub args: Vec<String>,
    /// Only the secrets the manifest declares, already resolved.
    pub secrets: HashMap<String, String>,
}

impl Sandbox {
    /// The full bwrap argument vector (public for tests and `doctor`).
    pub fn bwrap_argv(&self) -> Vec<String> {
        let mut argv = vec![
            "--unshare-net".to_string(),
            "--die-with-parent".to_string(),
            // Read-only system: /usr plus the merged-usr symlinks (on
            // merged systems /bin, /lib… are symlinks, and binding a symlink
            // leaves the sandbox without a loader).
            "--ro-bind-try".to_string(),
            "/usr".to_string(),
            "/usr".to_string(),
            "--symlink".to_string(),
            "usr/bin".to_string(),
            "/bin".to_string(),
            "--symlink".to_string(),
            "usr/lib".to_string(),
            "/lib".to_string(),
            "--symlink".to_string(),
            "usr/lib64".to_string(),
            "/lib64".to_string(),
            "--symlink".to_string(),
            "usr/sbin".to_string(),
            "/sbin".to_string(),
            "--proc".to_string(),
            "/proc".to_string(),
            "--dev".to_string(),
            "/dev".to_string(),
            "--tmpfs".to_string(),
            "/tmp".to_string(),
            "--ro-bind".to_string(),
            self.plugin_root.display().to_string(),
            "/plugin".to_string(),
            "--clearenv".to_string(),
            "--setenv".to_string(),
            "PATH".to_string(),
            "/usr/bin:/bin".to_string(),
            "--setenv".to_string(),
            "HOME".to_string(),
            "/tmp/home".to_string(),
        ];
        // Secrets, name-checked at manifest validation, injected one by one
        // under the conventional env name (dots/dashes → underscores), which
        // is also the name tiers of `secret::resolve` use.
        let mut names: Vec<&String> = self.secrets.keys().collect();
        names.sort();
        for name in names {
            let env_name = name.to_ascii_uppercase().replace(['.', '-'], "_");
            argv.extend(["--setenv".to_string(), env_name, self.secrets[name].clone()]);
        }
        argv.extend([self.command.clone(), "--".to_string()]);
        argv.extend(self.args.iter().cloned());
        argv
    }

    /// Run one MCP `tools/call` and return the plugin's text answer.
    pub fn call(&self, tool: &str, args: &Value) -> Result<String, SandboxError> {
        let started = Instant::now();
        let mut child = self.spawn()?;
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| SandboxError::Spawn("stdin pipe".into()))?;
        let mut send = |msg: &Value| -> Result<(), SandboxError> {
            let line =
                serde_json::to_string(msg).map_err(|e| SandboxError::Protocol(e.to_string()))?;
            stdin
                .write_all(line.as_bytes())
                .and_then(|_| stdin.write_all(b"\n"))
                .and_then(|_| stdin.flush())
                .map_err(|e| SandboxError::Protocol(format!("write: {e}")))
        };
        send(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "ka-agent", "version": env!("CARGO_PKG_VERSION")}
            }
        }))?;
        send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        send(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": tool, "arguments": args}
        }))?;
        let answer = self.read_answer(&mut child);
        let status = child.wait().ok();
        let stderr_tail = child
            .stderr
            .take()
            .map(|err| {
                use std::io::Read;
                let mut buf = Vec::new();
                let _ = err.take(2048).read_to_end(&mut buf);
                String::from_utf8_lossy(&buf).trim().to_string()
            })
            .unwrap_or_default();
        let answer = match answer {
            Err(SandboxError::Protocol(ref e)) if e.contains("exited before answering") => {
                // The sandbox never started (bubblewrap missing, restricted
                // user namespaces) or the plugin crashed before answering:
                // say which, with the stderr tail that was collected for
                // exactly this case.
                let code = status.map(|s| s.to_string()).unwrap_or_default();
                Err(SandboxError::Failed(if stderr_tail.is_empty() {
                    format!("plugin exited before answering ({code})")
                } else {
                    format!("plugin exited before answering ({code}): {stderr_tail}")
                }))
            }
            other => other,
        };
        let answer = answer?;
        let _ = started;
        // The answer's text, capped.
        let mut text = answer;
        if text.len() > OUTPUT_CAP {
            // Char-boundary cut: model output is routinely non-ASCII and a
            // byte cut would panic.
            text.truncate(ka_agent_core::truncate_at_char(&text, OUTPUT_CAP).len());
            text.push_str("… (truncated)");
        }
        Ok(text)
    }

    fn spawn(&self) -> Result<Child, SandboxError> {
        let mut cmd = Command::new("bwrap");
        cmd.args(self.bwrap_argv())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Captured for the failure message; KAGENT_PLUGIN_DEBUG=1 also
            // streams it live (authoring and ops).
            .stderr(if std::env::var("KAGENT_PLUGIN_DEBUG").is_ok() {
                Stdio::inherit()
            } else {
                Stdio::piped()
            });
        cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                SandboxError::NoBubblewrap
            } else {
                SandboxError::Spawn(e.to_string())
            }
        })
    }

    /// Read stdout lines until the `tools/call` response (id 2) arrives, the
    /// process dies, or the watchdog kills it at the deadline.
    fn read_answer(&self, child: &mut Child) -> Result<String, SandboxError> {
        let done = Arc::new(AtomicBool::new(false));
        let killed = Arc::new(AtomicBool::new(false));
        let watchdog_done = Arc::clone(&done);
        let watchdog_killed = Arc::clone(&killed);
        let pid = child.id();
        let watchdog = std::thread::spawn(move || {
            let deadline = Instant::now() + CALL_TIMEOUT;
            while Instant::now() < deadline {
                if watchdog_done.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            // Wall clock exceeded: hard-kill (the read below then sees EOF).
            watchdog_killed.store(true, Ordering::Relaxed);
            let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
        });
        let answer = (|| -> Result<String, SandboxError> {
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| SandboxError::Spawn("stdout pipe".into()))?;
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let line = line.map_err(|e| SandboxError::Protocol(format!("read: {e}")))?;
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if msg.get("id").and_then(Value::as_i64) == Some(2) {
                    let result = &msg["result"];
                    if result.get("isError").and_then(Value::as_bool) == Some(true) {
                        let detail = result["content"][0]["text"]
                            .as_str()
                            .unwrap_or("unknown plugin error");
                        return Err(SandboxError::Failed(detail.to_string()));
                    }
                    return result["content"][0]["text"]
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| SandboxError::Protocol("no text in answer".into()));
                }
            }
            Err(SandboxError::Protocol(
                "plugin exited before answering".into(),
            ))
        })();
        let watchdog_killed = killed.load(Ordering::Relaxed);
        done.store(true, Ordering::Relaxed);
        let _ = watchdog.join();
        // A watchdog kill is the timeout; an EOF without it means the
        // sandbox never started or the plugin crashed — the caller has the
        // stderr tail for that case.
        if watchdog_killed {
            return Err(SandboxError::Timeout);
        }
        answer
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn sandbox() -> Sandbox {
        Sandbox {
            plugin_root: PathBuf::from("/data/plugins/probe"),
            command: "/plugin/bin/probe".into(),
            args: vec![],
            secrets: [("probe.token".to_string(), "s3cr3t".to_string())]
                .into_iter()
                .collect(),
        }
    }

    /// Live run against a POSIX-sh MCP plugin: requires bubblewrap (CI
    /// installs it; the README documents it).
    #[test]
    #[cfg(target_os = "linux")] // bubblewrap is Linux-only
    fn runs_a_script_plugin_and_sees_only_the_sandbox_view() {
        if !bwrap_present() {
            panic!("bubblewrap is required for the sandbox tests: apt install bubblewrap");
        }
        let dir = std::env::temp_dir().join("ka-sandbox-smoke");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(
            dir.join("bin/probe"),
            "#!/bin/sh\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *'\"method\":\"initialize\"'*) printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"serverInfo\":{\"name\":\"smoke\"}}}\n';;\n    *'\"id\":2'*) printf '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"PATH=%s HOME=%s\"}]}}\n' \"$PATH\" \"$HOME\";;\n  esac\ndone\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            dir.join("bin/probe"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let sandbox = Sandbox {
            plugin_root: dir.clone(),
            command: "/plugin/bin/probe".into(),
            args: vec![],
            secrets: [("smoke.token".to_string(), "s3cr3t".to_string())]
                .into_iter()
                .collect(),
        };
        let answer = sandbox
            .call("smoke.echo", &serde_json::json!({}))
            .expect("sandbox call");
        assert!(
            answer.contains("PATH=/usr/bin:/bin") && answer.contains("HOME=/tmp/home"),
            "sandbox view wrong: {answer}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn bwrap_present() -> bool {
        std::process::Command::new("bwrap")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }

    #[test]
    fn argv_unshares_net_clears_env_and_mounts_the_plugin_ro() {
        let argv = sandbox().bwrap_argv().join(" ");
        assert!(argv.contains("--unshare-net"), "{argv}");
        assert!(argv.contains("--clearenv"), "{argv}");
        assert!(argv.contains("--tmpfs /tmp"), "{argv}");
        assert!(argv.contains("/data/plugins/probe /plugin"), "{argv}");
        assert!(argv.contains("PROBE_TOKEN s3cr3t"), "{argv}");
        // The dotted secret name is never used as an env var name.
        assert!(!argv.contains("probe.token"), "{argv}");
        // The instance's own environment never leaks in: only PATH/HOME and
        // the declared secret are set.
        assert!(!argv.contains("KA_AGENT"), "{argv}");
        assert!(!argv.contains("TELEGRAM"), "{argv}");
    }
}
