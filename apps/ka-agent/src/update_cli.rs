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
    let mut buf = Vec::new();
    resp.read_to_end(&mut buf).map_err(|e| format!("{e}"))?;
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

/// The one non-empty line of `<tarball>.sha256`, hex.
fn sha256_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
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
    let name = format!("ka-agent-{triple}.tar.gz");
    let Some(asset) = release.assets.iter().find(|a| a.name == name) else {
        return fail(format!("release {latest} has no {name}"));
    };
    let tarball = match http_get(&asset.browser_download_url) {
        Ok(b) => b,
        Err(e) => return fail(e),
    };
    let expected = match http_get(&format!("{}.sha256", asset.browser_download_url)) {
        Ok(b) => String::from_utf8_lossy(&b)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
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
    let new_bin = staging.join("ka-agent");
    if let Ok(meta) = std::fs::metadata(&exe) {
        let _ = std::fs::set_permissions(&new_bin, meta.permissions());
    }
    if let Err(e) = std::fs::rename(&new_bin, &exe) {
        let _ = std::fs::remove_file(&staging);
        return fail(format!(
            "cannot replace {} — {e}. Stop the service first: ka-agent service stop",
            exe.display()
        ));
    }
    let _ = std::fs::remove_file(&staging);

    // Restart every installed profile's service (they run the old binary).
    let mut restarted = 0;
    if std::env::var_os("KA_AGENT_DATA_DIR").is_none() {
        for p in ka_agent_core::layout::list_profiles() {
            if !crate::service::is_installed_for(&p) {
                continue;
            }
            let paths = ka_agent_runtime::Paths::new(ka_agent_core::layout::compose_profile_root(
                &ka_agent_core::layout::base_dir(),
                &p,
            ));
            if crate::admin::request(&paths, &serde_json::json!({"op": "shutdown"})).is_ok() {
                restarted += 1;
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

fn write_and_extract(tarball: &[u8], staging: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    let gz = staging.join("t.tar.gz");
    std::fs::write(&gz, tarball).map_err(|e| format!("{}: {e}", gz.display()))?;
    let out = std::process::Command::new("tar")
        .arg("xzf")
        .arg(&gz)
        .arg("-C")
        .arg(staging)
        .arg("ka-agent")
        .output()
        .map_err(|e| format!("cannot run tar: {e}"))?;
    let _ = std::fs::remove_file(&gz);
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
