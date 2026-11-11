//! `ka-agent update`: self-update. One line says where it is going
//! (`0.8.1 → 0.9.0`); the rest is quiet unless something needs the operator.
//! Downloads the release for this machine, verifies its checksum, replaces
//! the running binary atomically, and restarts every installed profile's
//! service. `update check` only reports; `doctor` surfaces a cached newer
//! release.

use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use ka_agent_runtime::paths::Paths;

use crate::UpdateAction;

const REPO: &str = "jaltez/ka-agent";
/// Meta key: "version|checked_ms" of the last update check.
const CHECK_META: &str = "update.checked";

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("ka-agent: {msg}");
    ExitCode::FAILURE
}

fn target_triple() -> Result<String, String> {
    let os = match std::env::consts::OS {
        "linux" => "unknown-linux-gnu",
        "macos" => "apple-darwin",
        "windows" => "pc-windows-msvc",
        other => return Err(format!("unsupported OS {other}")),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => return Err(format!("unsupported architecture {other}")),
    };
    Ok(format!("{arch}-{os}"))
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

fn http_get(url: &str) -> Result<Vec<u8>, String> {
    let mut resp = reqwest::blocking::Client::new()
        .get(url)
        .timeout(std::time::Duration::from_secs(120))
        .header("User-Agent", "ka-agent-updater")
        .send()
        .map_err(|e| format!("{e}"))?;
    let status = resp.status();
    let mut buf = Vec::new();
    resp.read_to_end(&mut buf).map_err(|e| format!("{e}"))?;
    if !status.is_success() {
        // The API answers errors with `{"message": "..."}`; a rate limit or
        // a bad tag must read as "403 …: rate limit exceeded", never as a
        // serde error about a missing `tag_name`.
        let message = serde_json::from_slice::<serde_json::Value>(&buf)
            .ok()
            .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
            .unwrap_or_else(|| String::from_utf8_lossy(&buf).trim().to_string());
        return Err(format!("{status} {url}: {message}"));
    }
    Ok(buf)
}

/// The release to move to: the pinned `--version`, else the latest tag.
/// The release-metadata URL (`KA_AGENT_UPDATE_API` overrides the default —
/// tests point it at a fixture server).
fn api_url(path: &str) -> String {
    std::env::var("KA_AGENT_UPDATE_API")
        .ok()
        .filter(|u| !u.is_empty())
        .map(|u| u.to_string())
        .unwrap_or_else(|| format!("https://api.github.com/repos/{REPO}"))
        .trim_end_matches('/')
        .to_string()
        + path
}

fn fetch_release(want: Option<&str>) -> Result<Release, String> {
    let url = match want {
        Some(v) => {
            let v = v.trim_start_matches('v');
            api_url(&format!("/releases/tags/v{v}"))
        }
        None => api_url("/releases/latest"),
    };
    let body = http_get(&url)?;
    serde_json::from_slice(&body).map_err(|e| format!("release metadata: {e}"))
}

fn is_newer(latest: &str, current: &str) -> bool {
    let num = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    num(latest) > num(current)
}

/// The newest release tag from the metadata endpoint (no install) and
/// whether it is newer than this binary: the daemon's periodic check (F4)
/// uses it so `status`/`doctor` learn about a release without a manual
/// `update check`. Errors carry the same wording `update check` prints.
pub fn latest_release() -> Result<(String, bool), String> {
    let release = fetch_release(None)?;
    let latest = release.tag_name.trim_start_matches('v').to_string();
    let newer = is_newer(&latest, env!("CARGO_PKG_VERSION"));
    Ok((latest, newer))
}

/// Whether this process can replace the installed binary — the
/// permission gate for `/update`. The update writes a staging directory
/// next to the running image and renames over it, so the install
/// directory must be writable. The probe is one create+delete of a
/// hidden file in that directory (nothing is left behind); a refusal —
/// a system-wide install owned by root, a locked-down directory — is
/// reported with the remedy instead of failing halfway through a
/// download. Returns the running binary's path on success.
pub fn can_write_install_dir() -> Result<PathBuf, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("cannot locate the running binary: {e}"))?;
    let Some(dir) = exe.parent() else {
        return Err(format!(
            "the running binary has no directory: {}",
            exe.display()
        ));
    };
    if let Some(err) = gate_error(dir) {
        return Err(err);
    }
    Ok(exe)
}

/// The probe's core: `Some(error)` when a file cannot be created (and
/// removed) in `dir` — exactly the write the update's staging directory
/// and final rename need.
fn dir_probe_error(dir: &std::path::Path) -> Option<String> {
    let probe = dir.join(format!(".ka-agent-update-probe-{}", std::process::id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            None
        }
        Err(e) => Some(format!("cannot write {} — {e}", dir.display())),
    }
}

/// The gate's refusal text for a directory the probe cannot write —
/// one formatter shared by the gate and its tests so the wording
/// cannot drift.
fn gate_error(dir: &std::path::Path) -> Option<String> {
    dir_probe_error(dir).map(|e| {
        format!(
            "not enough permissions to update: {e}. The daemon must be able \
             to replace its own binary; update on the machine instead: \
             ka-agent update"
        )
    })
}

/// `/update` from a channel — the daemon-side flow over the same
/// machinery `ka-agent update` uses. `check_only` reports; the install
/// path checks the permission gate FIRST (nothing downloads when the
/// daemon cannot replace its binary), then runs the CLI updater as a
/// child (pinned to the release this daemon validated) and waits for
/// it. The restart choreography is deliberately conservative: the
/// daemon only stops itself when a supervisor will bring it back AND
/// no turn is in flight — the reply must land and a live turn must not
/// be killed. Blocking: callers keep it off the async workers.
pub fn update_from_daemon(
    check_only: bool,
    paths: &ka_agent_runtime::Paths,
    runtime: &ka_agent_runtime::Runtime,
) -> Result<String, String> {
    let current = env!("CARGO_PKG_VERSION");
    let (latest, newer) = latest_release()?;
    if !newer {
        return Ok(format!("ka-agent {current} (up to date)"));
    }
    if check_only {
        return Ok(format!(
            "{current} → {latest} available — /update installs it"
        ));
    }
    // The permission gate: refuse before any download.
    let exe = can_write_install_dir()?;
    let out = std::process::Command::new(&exe)
        .args(["update", "run", "--version", latest.as_str()])
        // The child must not run its service-restart loop: a manual
        // daemon has no KA_AGENT_DATA_DIR for the inherited check below
        // to bite on, and the loop's shutdown request would kill THIS
        // daemon before the reply is sent. This daemon arranges its own
        // restart instead.
        .env("KA_AGENT_UPDATE_SKIP_RESTARTS", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("cannot spawn the updater: {e}"))?;
    if !out.status.success() {
        let text = {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let pick = if stderr.trim().is_empty() {
                &stdout
            } else {
                &stderr
            };
            pick.trim().lines().last().unwrap_or("").to_string()
        };
        return Err(if text.is_empty() {
            format!("update failed (exit {:?})", out.status.code())
        } else {
            format!("update failed: {text}")
        });
    }
    // A turn that started while the child downloaded must not be killed
    // by our own restart: skip the self-shutdown and say so.
    let turn_active = matches!(
        runtime.session_status(),
        ka_agent_core::SessionStatus::Running
            | ka_agent_core::SessionStatus::AwaitingApproval
            | ka_agent_core::SessionStatus::Stopping
    );
    if turn_active {
        return Ok(format!(
            "{current} → {latest} installed — a turn is running, so the \
             daemon keeps the old binary until its next restart"
        ));
    }
    // Only stop ourselves when a supervisor will actually bring us back.
    // KA_AGENT_DATA_DIR alone is not proof: service units set it, but so
    // does a hand-started pinned instance (layout docs) — that daemon
    // must not exit into nothing. systemd marks unit processes with
    // INVOCATION_ID; launchd/Windows answer is_active().
    let supervised = std::env::var_os("KA_AGENT_DATA_DIR").is_some_and(|v| !v.is_empty())
        && (std::env::var_os("INVOCATION_ID").is_some() || crate::service::is_active());
    if !supervised {
        return Ok(format!(
            "{current} → {latest} installed — restart the daemon to run it \
             (it keeps the old binary until then)"
        ));
    }
    let p = paths.clone();
    let rt = runtime.clone();
    std::thread::spawn(move || {
        // Covers the reply crossing the router and the Telegram send,
        // including the send path's flood-control retry (clamped
        // 1..=60 s).
        std::thread::sleep(std::time::Duration::from_secs(70));
        // A turn that started after the check above must not be killed
        // by our own restart: skip it; the next restart picks the new
        // binary up.
        if matches!(
            rt.session_status(),
            ka_agent_core::SessionStatus::Running
                | ka_agent_core::SessionStatus::AwaitingApproval
                | ka_agent_core::SessionStatus::Stopping
        ) {
            eprintln!("update: a turn is in flight — skipping the self-restart");
            return;
        }
        if let Err(e) = crate::admin::request(&p, &serde_json::json!({ "op": "shutdown" })) {
            eprintln!("update: self-restart request failed: {e}");
        }
    });
    Ok(format!(
        "{current} → {latest} installed — the service restarts into it shortly"
    ))
}

/// Record a check result in the runtime's own store: the daemon-side
/// counterpart of the CLI's `record_check` (same `update.checked` meta
/// `doctor` reads).
pub fn record_check_in(store: &ka_agent_store::Store, latest: &str) {
    let _ = store.meta_set(CHECK_META, &format!("{latest}|{}", ka_agent_core::now_ms()));
}

fn sha256_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Move the running image aside and put the new one at `exe`. Windows
/// refuses to replace a running image but allows renaming it (the image
/// stays mapped under the old name until the process exits).
///
/// If the second rename fails the old name is restored, so a failed swap
/// leaves a runnable install instead of a missing binary. `rename` is a
/// parameter so the ordering is unit-testable on Linux.
#[cfg(any(windows, test))]
fn swap_image<R>(
    exe: &std::path::Path,
    new_bin: &std::path::Path,
    old: &std::path::Path,
    mut rename: R,
) -> Result<(), String>
where
    R: FnMut(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
{
    rename(exe, old).map_err(|e| format!("cannot set {} aside — {e}", exe.display()))?;
    if let Err(e) = rename(new_bin, exe) {
        let hint = "Stop the service first: ka-agent service stop";
        return Err(match rename(old, exe) {
            Ok(()) => format!("cannot replace {} — {e}. {hint}", exe.display()),
            Err(re) => format!(
                "cannot replace {} — {e}; restoring the old image also failed — {re}. {hint}",
                exe.display()
            ),
        });
    }
    Ok(())
}

pub fn update(action: UpdateAction) -> ExitCode {
    let paths = Paths::new(Paths::default_root());
    let _ = paths.ensure_dirs();
    let current = env!("CARGO_PKG_VERSION");
    let want = match &action {
        UpdateAction::Check { version } | UpdateAction::Run { version } => version.clone(),
    };
    let release = match fetch_release(want.as_deref()) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let latest = release.tag_name.trim_start_matches('v').to_string();
    if !is_newer(&latest, current) && want.is_none() {
        println!("ka-agent {current} (up to date)");
        record_check(&paths, &latest);
        return ExitCode::SUCCESS;
    }
    println!("{current} → {latest}");
    if matches!(action, UpdateAction::Check { .. }) {
        record_check(&paths, &latest);
        return ExitCode::SUCCESS;
    }

    // Download the tarball for this machine + its checksum.
    let triple = match target_triple() {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    // Windows ships a zip; every Unix target a tarball (see release.yml).
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    let name = format!("ka-agent-{triple}.{ext}");
    let Some(asset) = release.assets.iter().find(|a| a.name == name) else {
        return fail(format!("release {latest} has no {name}"));
    };
    let tarball = match http_get(&asset.browser_download_url) {
        Ok(b) => b,
        Err(e) => return fail(e),
    };
    // `.sha256` is `shasum -a 256` output (`HEX  FILENAME`); compare the hash token only.
    let expected = match http_get(&format!("{}.sha256", asset.browser_download_url)) {
        Ok(b) => String::from_utf8_lossy(&b)
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().next())
            .unwrap_or("")
            .to_ascii_lowercase(),
        Err(e) => return fail(e),
    };
    if expected.is_empty() {
        return fail("checksum file empty");
    }
    let got = sha256_of(&tarball);
    if got != expected {
        return fail(format!(
            "checksum mismatch: expected {expected}, got {got} — nothing was installed"
        ));
    }
    record_check(&paths, &latest);
    println!("✓ verified");

    // Extract with the system tar (no tar crate; the archive is one file).
    // KA_AGENT_INSTALL_TO overrides the target (tests); the real path is
    // the running binary.
    let exe = match std::env::var_os("KA_AGENT_INSTALL_TO") {
        Some(p) => PathBuf::from(p),
        None => match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => return fail(format!("cannot locate the running binary: {e}")),
        },
    };
    let staging = exe
        .parent()
        .map(|d| d.join(".ka-agent-update"))
        .unwrap_or_else(|| PathBuf::from(".ka-agent-update"));
    if let Err(e) = write_and_extract(&tarball, &staging) {
        return fail(e);
    }
    // Replace atomically: new file next to the old, rename over it.
    let new_bin = staging.join(bin_name());
    if let Ok(meta) = std::fs::metadata(&exe) {
        let _ = std::fs::set_permissions(&new_bin, meta.permissions());
    }
    // Windows refuses to replace a running image but allows renaming it
    // (the image stays mapped under the old name until the process exits).
    #[cfg(windows)]
    {
        let old = exe.with_extension("old.exe");
        let _ = std::fs::remove_file(&old);
        if let Err(e) = swap_image(&exe, &new_bin, &old, |from, to| std::fs::rename(from, to)) {
            let _ = std::fs::remove_file(&staging);
            return fail(e);
        }
    }
    #[cfg(not(windows))]
    if let Err(e) = std::fs::rename(&new_bin, &exe) {
        let _ = std::fs::remove_file(&staging);
        return fail(format!(
            "cannot replace {} — {e}. Stop the service first: ka-agent service stop",
            exe.display()
        ));
    }
    let _ = std::fs::remove_file(&staging);

    // Restart every installed profile's service (they run the old binary).
    // Unix: ask the running daemon to stop; its supervisor restarts it.
    // Windows: the logon task does not respawn on exit, so start it here
    // (the old process exits on its own once its image is gone).
    // `KA_AGENT_UPDATE_SKIP_RESTARTS` marks a child spawned by the
    // daemon's `/update`: it arranges its own restart, so the loop's
    // shutdown request must not race (or kill) it. KA_AGENT_DATA_DIR is
    // the CLI test seam with the same effect.
    let mut restarted = 0;
    if std::env::var_os("KA_AGENT_DATA_DIR").is_none()
        && std::env::var_os("KA_AGENT_UPDATE_SKIP_RESTARTS").is_none()
    {
        for p in ka_agent_core::layout::list_profiles() {
            if !crate::service::is_installed_for(&p) {
                continue;
            }
            #[cfg(unix)]
            {
                let paths =
                    ka_agent_runtime::Paths::new(ka_agent_core::layout::compose_profile_root(
                        &ka_agent_core::layout::base_dir(),
                        &p,
                    ));
                if crate::admin::request(&paths, &serde_json::json!({"op": "shutdown"})).is_ok() {
                    restarted += 1;
                }
            }
            #[cfg(windows)]
            {
                // The same bare task name `service install` created — the
                // argument builder is shared, so the two cannot drift.
                let name = crate::service::windows_task_name(&p);
                let args = crate::service::win_task::task_run_args(&name);
                if std::process::Command::new("schtasks.exe")
                    .args(&args)
                    .output()
                    .is_ok_and(|o| o.status.success())
                {
                    restarted += 1;
                }
            }
        }
    }
    if restarted > 0 {
        println!("✓ installed · service restarting");
    } else {
        println!("✓ installed · ka-agent --version to confirm");
    }
    ExitCode::SUCCESS
}

/// The binary's name inside the release archive.
fn bin_name() -> &'static str {
    if cfg!(windows) {
        "ka-agent.exe"
    } else {
        "ka-agent"
    }
}

/// Unpack the release asset's single binary into `staging`: system `tar`
/// on Unix, PowerShell's `Expand-Archive` on Windows (no archive crates in
/// the dependency set on purpose).
fn write_and_extract(asset: &[u8], staging: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    let archive = staging.join(if cfg!(windows) { "a.zip" } else { "a.tar.gz" });
    std::fs::write(&archive, asset).map_err(|e| format!("{}: {e}", archive.display()))?;
    #[cfg(not(windows))]
    let out = std::process::Command::new("tar")
        .arg("xzf")
        .arg(&archive)
        .arg("-C")
        .arg(staging)
        .arg("ka-agent")
        .output()
        .map_err(|e| format!("cannot run tar: {e}"))?;
    #[cfg(windows)]
    let out = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Expand-Archive -LiteralPath",
        ])
        .arg(&archive)
        .arg("-DestinationPath")
        .arg(staging)
        .arg("-Force")
        .output()
        .map_err(|e| format!("cannot run powershell: {e}"))?;
    let _ = std::fs::remove_file(&archive);
    if !out.status.success() {
        return Err(format!(
            "extract: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn record_check(paths: &Paths, latest: &str) {
    if let Ok(store) = ka_agent_store::Store::open(&paths.state_db()) {
        let _ = store.meta_set(CHECK_META, &format!("{latest}|{}", ka_agent_core::now_ms()));
    }
}

/// `("vX", checked_ms)` from the last check, if recent.
pub fn last_check(paths: &Paths) -> Option<(String, i64)> {
    let store = ka_agent_store::Store::open(&paths.state_db()).ok()?;
    let raw = store.meta_get(CHECK_META).ok().flatten()?;
    let (v, at) = raw.split_once('|')?;
    Some((v.to_string(), at.parse().ok()?))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::path::{Path, PathBuf};

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ka-agent-update-probe-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn probe_passes_on_a_writable_dir_and_leaves_nothing_behind() {
        let dir = scratch_dir("ok");
        assert!(dir_probe_error(&dir).is_none());
        assert!(
            !dir.join(format!(".ka-agent-update-probe-{}", std::process::id()))
                .exists(),
            "the probe cleans up after itself"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The gate `/update` hinges on: a directory the process cannot write
    /// must be reported, never attempted. Root bypasses mode bits, so the
    /// negative expectation self-skips when the restriction does not
    /// actually apply (a sentinel write succeeds).
    #[cfg(unix)]
    #[test]
    fn probe_reports_a_dir_the_process_cannot_write() {
        let dir = scratch_dir("ro");
        let ro = dir.join("ro");
        std::fs::create_dir(&ro).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
        let sentinel = ro.join("sentinel");
        if std::fs::write(&sentinel, b"x").is_ok() {
            eprintln!("skipping: this user bypasses mode bits (root)");
            return;
        }
        let err = gate_error(&ro).unwrap();
        assert!(err.contains("not enough permissions"), "{err}");
        assert!(err.contains("ka-agent update"), "{err}");
        assert!(err.contains(&ro.display().to_string()), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn swap_puts_the_old_image_back_when_the_new_one_cannot_land() {
        let mut calls: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut rename = |from: &Path, to: &Path| -> std::io::Result<()> {
            calls.push((from.to_path_buf(), to.to_path_buf()));
            if calls.len() == 2 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "still locked",
                ));
            }
            Ok(())
        };
        let err = swap_image(
            Path::new("/opt/ka-agent/ka-agent"),
            Path::new("/opt/ka-agent/.ka-agent-update/ka-agent"),
            Path::new("/opt/ka-agent/ka-agent.old.exe"),
            &mut rename,
        )
        .unwrap_err();
        assert!(err.contains("cannot replace"), "{err}");
        assert!(err.contains("still locked"), "{err}");
        assert!(!err.contains("also failed"), "the restore worked: {err}");
        assert_eq!(
            calls,
            [
                (
                    PathBuf::from("/opt/ka-agent/ka-agent"),
                    PathBuf::from("/opt/ka-agent/ka-agent.old.exe"),
                ),
                (
                    PathBuf::from("/opt/ka-agent/.ka-agent-update/ka-agent"),
                    PathBuf::from("/opt/ka-agent/ka-agent"),
                ),
                (
                    PathBuf::from("/opt/ka-agent/ka-agent.old.exe"),
                    PathBuf::from("/opt/ka-agent/ka-agent"),
                ),
            ],
            "aside, new-in, old-back"
        );
    }

    #[test]
    fn swap_reports_a_failed_restore_too() {
        let mut calls = 0;
        let mut rename = |_: &Path, _: &Path| -> std::io::Result<()> {
            calls += 1;
            if calls >= 2 {
                return Err(std::io::Error::other("denied"));
            }
            Ok(())
        };
        let err = swap_image(
            Path::new("/a/ka-agent"),
            Path::new("/a/new/ka-agent"),
            Path::new("/a/ka-agent.old.exe"),
            &mut rename,
        )
        .unwrap_err();
        assert!(err.contains("restoring the old image also failed"), "{err}");
        assert_eq!(calls, 3, "the restore is still attempted");
    }

    #[test]
    fn swap_succeeds_with_two_renames() {
        let mut calls: Vec<(PathBuf, PathBuf)> = Vec::new();
        swap_image(
            Path::new("/b/ka-agent"),
            Path::new("/b/new/ka-agent"),
            Path::new("/b/ka-agent.old.exe"),
            |from: &Path, to: &Path| -> std::io::Result<()> {
                calls.push((from.to_path_buf(), to.to_path_buf()));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            calls,
            [
                (
                    PathBuf::from("/b/ka-agent"),
                    PathBuf::from("/b/ka-agent.old.exe"),
                ),
                (
                    PathBuf::from("/b/new/ka-agent"),
                    PathBuf::from("/b/ka-agent"),
                ),
            ]
        );
    }
}
