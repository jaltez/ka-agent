//! `ka-agent __guard`: the engine's pre_tool_use hook that keeps the
//! assistant out of ka-agent's own files. The engine matches rules on the
//! raw path argument, so `write { path: "../config.toml" }` from the
//! workspace never hits a pattern rule; this hook normalizes the path
//! (`~`, `.`, `..`, symlinks) and refuses when it lands inside a
//! protected list.
//!
//! Contract (ka engine hooks): the tool call arrives as JSON on stdin
//! `{"tool","arguments","cwd"}`; exit 2 blocks the call and stderr is the
//! reason the model sees. Any other exit allows it.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// Protected lists, from the worker env (set by the daemon).
fn protected(var: &str) -> Vec<PathBuf> {
    std::env::var_os(var)
        .map(|joined| std::env::split_paths(&joined).collect())
        .unwrap_or_default()
}

/// Lexical normalization: resolve `.`/`..` without touching the disk.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The real path: canonicalize the nearest existing ancestor and re-append
/// the rest, so a symlinked directory cannot smuggle a protected path.
fn resolve(path: &Path) -> PathBuf {
    let lexical = lexical(path);
    let mut probe = lexical.clone();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    while !probe.exists() {
        let Some(name) = probe.file_name().map(|n| n.to_os_string()) else {
            break;
        };
        rest.push(name);
        if !probe.pop() {
            break;
        }
    }
    let mut base = std::fs::canonicalize(&probe).unwrap_or(probe);
    for name in rest.into_iter().rev() {
        base.push(name);
    }
    base
}

/// Is `path` equal to or beneath any protected entry?
fn inside(path: &Path, list: &[PathBuf]) -> Option<PathBuf> {
    // Both sides go through `resolve`: a candidate built from a symlinked
    // spelling of the same directory (macOS `/var` → `/private/var`) must
    // match the resolved root, or the guard stops protecting its files.
    let path = resolve(path);
    for entry in list {
        let entry = resolve(entry);
        if path == entry || path.starts_with(&entry) {
            return Some(entry);
        }
    }
    None
}

/// The path argument of a file tool (`grep` may omit it: then `cwd`).
fn target_path(tool: &str, args: &Value, cwd: &Path) -> Option<PathBuf> {
    let raw = args
        .get("path")
        .and_then(Value::as_str)
        .or_else(|| args.get("file").and_then(Value::as_str))
        .map(str::to_string);
    let raw = match raw {
        Some(r) => r,
        // grep/glob legitimately search the whole cwd; glob never writes
        None if tool == "grep" || tool == "glob" => cwd.display().to_string(),
        None => return None,
    };
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        home.join(rest).display().to_string()
    } else {
        raw
    };
    let p = PathBuf::from(expanded);
    Some(if p.is_absolute() { p } else { cwd.join(p) })
}

/// Grant decisions from the worker env (`KA_AGENT_GUARD_GRANTS`,
/// serialized by `WorkerEnv::sanitize`). Absent env = all-false: every
/// grant check below stays off.
#[derive(Debug, Clone, Default, PartialEq)]
struct Grants {
    install: bool,
    full: bool,
    hard: bool,
    deny: Vec<PathBuf>,
    allow: Vec<PathBuf>,
}

fn grants_env() -> Grants {
    let Some(text) = std::env::var("KA_AGENT_GUARD_GRANTS").ok() else {
        return Grants::default();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return Grants::default();
    };
    let dirs = |key: &str| -> Vec<PathBuf> {
        v[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|d| d.as_str())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    Grants {
        install: v["install"].as_bool().unwrap_or(false),
        full: v["full"].as_bool().unwrap_or(false),
        hard: v["hard"].as_bool().unwrap_or(false),
        deny: dirs("deny"),
        allow: dirs("allow"),
    }
}

/// Bash self-protection beyond the file-level list. In every mode the
/// enumerated own files (config, store, secrets, …) are denied; when
/// `hard` (allow mode or `grants.full`) no operator gate remains for
/// bash, so the whole data root and the binary are protected too — the
/// workspace (the hook's `cwd`) and the attachments dir stay writable.
fn bash_protects(resolved: &Path, cwd: &Path, hard: bool) -> Option<String> {
    if let Some(hit) = inside(resolved, &protected("KA_AGENT_GUARD_WRITE")) {
        return Some(format!(
            "protected: ka-agent's own files ({} is inside {})",
            resolved.display(),
            hit.display()
        ));
    }
    if !hard {
        return None;
    }
    let root = std::env::var_os("KA_AGENT_DATA_DIR").map(PathBuf::from)?;
    let hit = inside(resolved, std::slice::from_ref(&root))?;
    let r = resolve(resolved);
    let open = [resolve(cwd), resolve(&root.join("attachments"))];
    if open.iter().any(|ok| r == *ok || r.starts_with(ok)) {
        return None;
    }
    if let Ok(exe) = std::env::current_exe()
        && resolve(&exe) == r
    {
        return Some(format!(
            "protected: ka-agent's own files ({} is the ka-agent binary)",
            r.display()
        ));
    }
    Some(format!(
        "protected: ka-agent's own files ({} is inside {})",
        r.display(),
        hit.display()
    ))
}

/// Strip one level of quotes when they wrap the whole word.
fn unquote(word: &str) -> &str {
    let bytes = word.as_bytes();
    if bytes.len() >= 2 {
        let (a, b) = (bytes[0], bytes[bytes.len() - 1]);
        if (a == b'"' || a == b'\'') && a == b {
            return &word[1..word.len() - 1];
        }
    }
    word
}

/// True when the word is path-shaped: absolute, home- or
/// dot-relative.
fn path_shaped(word: &str) -> bool {
    word.starts_with('/')
        || word.starts_with("~/")
        || word.starts_with("./")
        || word.starts_with("../")
}

/// Path-shaped tokens of a bash command, conservatively tokenized on
/// whitespace: words that look like paths, `>`/`>>`/`2>` redirect
/// targets (glued or separate), and `path=` argument values. Trailing
/// shell punctuation is dropped so `touch /a/b;` still yields `/a/b`.
fn bash_path_tokens(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in command.split_whitespace() {
        let word = unquote(word);
        // redirect targets, glued (">file", "2>>file") or separate ("> file")
        let word = word
            .strip_prefix("2>>")
            .or_else(|| word.strip_prefix("2>"))
            .or_else(|| word.strip_prefix("1>>"))
            .or_else(|| word.strip_prefix("1>"))
            .or_else(|| word.strip_prefix("&>>"))
            .or_else(|| word.strip_prefix("&>"))
            .or_else(|| word.strip_prefix(">>"))
            .or_else(|| word.strip_prefix('>'))
            .unwrap_or(word);
        if word == ">" || word == ">>" {
            continue; // the target is its own word
        }
        let value = word.strip_prefix("path=").unwrap_or(word);
        let trimmed = value.trim_end_matches([';', ',', '&', '|']);
        if path_shaped(trimmed) {
            out.push(trimmed.to_string());
        }
    }
    out
}

/// The first program word of one shell segment and the words after it,
/// with sudo/doas (and their flags, including `-u <user>`-style pairs)
/// stripped. `None` when the segment names nothing.
fn split_program(segment: &str) -> Option<(String, Vec<String>)> {
    let words: Vec<String> = segment
        .split_whitespace()
        .map(|w| unquote(w).trim_end_matches([';', ',']).to_string())
        .collect();
    let mut i = 0;
    if words.first().is_some_and(|w| w == "sudo" || w == "doas") {
        i = 1;
        while i < words.len() {
            let w = &words[i];
            if w == "-u" || w == "-g" {
                i += 2; // the flag and its value
            } else if w.starts_with('-') {
                i += 1;
            } else {
                break;
            }
        }
    }
    let prog = words.get(i)?;
    let base = prog.rsplit('/').next().unwrap_or(prog).to_string();
    Some((base, words[i + 1..].to_vec()))
}

/// True when the command installs software. Conservative by design:
/// plain `apt list`, `pip show` or `npm install` (local) never match —
/// only install subcommands (and `-g` for npm) do.
fn install_class(command: &str) -> bool {
    for segment in command.split([';', '\n']).flat_map(|s| {
        s.split("&&")
            .flat_map(|s| s.split("||"))
            .flat_map(|s| s.split('|'))
    }) {
        let Some((prog, rest)) = split_program(segment) else {
            continue;
        };
        // first word after the program that is not a flag
        let sub = rest.iter().find(|w| !w.starts_with('-'));
        let any = |needle: &str| rest.iter().any(|w| w == needle);
        let installs = match prog.as_str() {
            "apt" | "apt-get" | "dnf" | "yum" | "pip" | "pip3" | "pipx" | "cargo" | "go" => {
                sub.is_some_and(|s| s == "install")
            }
            "dpkg" => any("-i") || any("--install"),
            "pacman" => ["-S", "-Sy", "-Su", "-Syu", "-Syy", "-Syyu", "--sync"]
                .iter()
                .any(|f| any(f)),
            "npm" => {
                sub.is_some_and(|s| s == "install" || s == "i" || s == "add")
                    && (any("-g") || any("--global"))
            }
            "nix-env" => any("-i") || any("-iA") || any("--install"),
            _ => false,
        };
        if installs {
            return true;
        }
    }
    false
}

/// Resolve a token the way the file tools' paths resolve (`~` then
/// cwd-relative), then run the bash checks.
fn bash_guard(command: &str, cwd: &Path, g: &Grants) -> std::process::ExitCode {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    for token in bash_path_tokens(command) {
        let expanded = if let Some(rest) = token.strip_prefix("~/") {
            match &home {
                Some(h) => h.join(rest),
                None => continue,
            }
        } else {
            PathBuf::from(&token)
        };
        let resolved = resolve(&expanded);
        // Self-protection always holds, whatever the grants.
        if let Some(reason) = bash_protects(&resolved, cwd, g.hard) {
            eprintln!("{reason}");
            return std::process::ExitCode::from(2);
        }
        if !g.full
            && let Some(hit) = inside(&resolved, &g.deny)
        {
            eprintln!(
                "denied by grant for {} — remove or change the [[grants.path]] entry to allow it",
                hit.display()
            );
            return std::process::ExitCode::from(2);
        }
    }
    if g.full {
        // No gate remains: pre-approve so ask mode never asks.
        println!("{{\"decision\":\"allow\"}}");
        return std::process::ExitCode::SUCCESS;
    }
    if install_class(command) {
        if g.install {
            // Pre-approve: the hook's allow downgrades the permission
            // ask to allow (hardstops still apply engine-side).
            println!("{{\"decision\":\"allow\"}}");
        } else {
            eprintln!("app installs are denied — set grants.install = true to allow them");
            return std::process::ExitCode::from(2);
        }
    }
    std::process::ExitCode::SUCCESS
}

pub fn run() -> std::process::ExitCode {
    let mut input = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).is_err() {
        return std::process::ExitCode::SUCCESS;
    }
    let Ok(payload) = serde_json::from_str::<Value>(input.trim()) else {
        return std::process::ExitCode::SUCCESS;
    };
    let tool = payload["tool"].as_str().unwrap_or("");
    let args = payload.get("arguments").cloned().unwrap_or(Value::Null);
    let cwd = payload["cwd"]
        .as_str()
        .filter(|c| !c.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));
    let g = grants_env();
    if tool == "bash" {
        let command = args["command"].as_str().unwrap_or("");
        return bash_guard(command, &cwd, &g);
    }
    let Some(target) = target_path(tool, &args, &cwd) else {
        return std::process::ExitCode::SUCCESS;
    };
    let resolved = resolve(&target);
    let list = match tool {
        "write" | "edit" => protected("KA_AGENT_GUARD_WRITE"),
        "read" | "grep" => protected("KA_AGENT_GUARD_READ"),
        _ => return std::process::ExitCode::SUCCESS,
    };
    if let Some(hit) = inside(&resolved, &list) {
        eprintln!(
            "protected: ka-agent's own files ({} is inside {})",
            resolved.display(),
            hit.display()
        );
        return std::process::ExitCode::from(2);
    }
    if !g.full
        && let Some(hit) = inside(&resolved, &g.deny)
    {
        eprintln!(
            "denied by grant for {} — remove or change the [[grants.path]] entry to allow it",
            hit.display()
        );
        return std::process::ExitCode::from(2);
    }
    // A granted dir pre-approves the write/edit call (the hook's allow
    // only downgrades an ask: engine deny rules still refuse). This is
    // the mechanism for N allow dirs — no first-rule-per-tool quirk.
    if matches!(tool, "write" | "edit") && (g.full || inside(&resolved, &g.allow).is_some()) {
        println!("{{\"decision\":\"allow\"}}");
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn lexical_resolves_dotdot_without_disk() {
        assert_eq!(lexical(Path::new("/a/b/../c")), PathBuf::from("/a/c"));
        assert_eq!(lexical(Path::new("a/./b")), PathBuf::from("a/b"));
        assert_eq!(lexical(Path::new("../x")), PathBuf::from("../x"));
    }

    #[test]
    fn inside_matches_equal_and_beneath() {
        let dir = std::env::temp_dir().join(format!("ka-guard-{}", std::process::id()));
        let secret = dir.join("secrets");
        std::fs::create_dir_all(&secret).unwrap();
        assert!(inside(&secret.join("x"), std::slice::from_ref(&secret)).is_some());
        assert!(inside(&secret, std::slice::from_ref(&secret)).is_some());
        assert!(inside(&dir.join("other"), &[secret]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dotdot_escape_from_the_workspace_is_caught() {
        let root = std::env::temp_dir().join(format!("ka-guard-esc-{}", std::process::id()));
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(root.join("config.toml"), "x").unwrap();
        let target = target_path("write", &serde_json::json!({"path": "../config.toml"}), &ws)
            .expect("path");
        let resolved = resolve(&target);
        assert!(
            inside(&resolved, &[root.join("config.toml")]).is_some(),
            "{}",
            resolved.display()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_without_a_path_checks_the_cwd() {
        let ws = std::env::temp_dir().join(format!("ka-guard-grep-{}", std::process::id()));
        std::fs::create_dir_all(ws.join("secrets")).unwrap();
        // From the workspace root a bare grep is fine; run from inside the
        // protected dir it is refused.
        let target = target_path("grep", &serde_json::json!({"pattern": "x"}), &ws).expect("cwd");
        assert!(inside(&resolve(&target), &[ws.join("secrets")]).is_none());
        let inside_dir = target_path(
            "grep",
            &serde_json::json!({"pattern": "x"}),
            &ws.join("secrets"),
        )
        .expect("cwd");
        assert!(
            inside(&resolve(&inside_dir), &[ws.join("secrets")]).is_some(),
            "a cwd search inside a protected dir is refused"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn scanner_finds_paths_redirects_and_path_args() {
        assert_eq!(
            bash_path_tokens("touch /tmp/outside-grants"),
            vec!["/tmp/outside-grants"]
        );
        assert_eq!(
            bash_path_tokens("echo x > /srv/log.txt 2>>/srv/err.txt"),
            vec!["/srv/log.txt", "/srv/err.txt"]
        );
        assert_eq!(
            bash_path_tokens("cat ~/notes ./here ../up;"),
            vec!["~/notes", "./here", "../up"]
        );
        assert_eq!(
            bash_path_tokens("tool --flag path=/etc/app.conf plain"),
            vec!["/etc/app.conf"]
        );
        // words that are not paths, redirects without targets and
        // quotes around non-paths never appear
        assert!(bash_path_tokens("echo hi && ls -la").is_empty());
        // a quoted path is unquoted when the quotes wrap the whole
        // word; internal spaces stay conservative (two words, neither
        // path-shaped)
        assert_eq!(bash_path_tokens("rm '/root/secret'"), vec!["/root/secret"]);
        assert!(bash_path_tokens("rm '/root/secret file'").is_empty());
    }

    #[test]
    fn classifier_matches_install_commands_only() {
        for yes in [
            "sudo apt-get install -y cowsay",
            "sudo apt install cowsay",
            "apt-get -y install htop",
            "doas dnf install ripgrep",
            "yum install git",
            "dpkg -i pkg.deb",
            "dpkg --install pkg.deb",
            "pacman -Syu",
            "npm install -g typescript",
            "npm i -g prettier",
            "pip install requests",
            "pip3 install --user httpie",
            "pipx install black",
            "cargo install ripgrep",
            "go install golang.org/x/tools/cmd/goimports@latest",
            "nix-env -i hello",
            "nix-env -iA nixpkgs.hello",
            "cd /tmp && sudo apt-get install -y sl",
            "sudo -u root apt-get install -y sl",
        ] {
            assert!(install_class(yes), "should match: {yes}");
        }
        for no in [
            "apt list --upgradable",
            "apt-get update",
            "pip show requests",
            "pip3 --version",
            "npm install", // local project install
            "npm list -g",
            "cargo build --release",
            "go build ./...",
            "dpkg -l",
            "pacman -Q",
            "nix-env -q",
            "echo apt-get install -y cowsay",
            "cat /var/log/apt-get install notes",
            "sudo ls /root",
        ] {
            assert!(!install_class(no), "must not match: {no}");
        }
    }

    #[test]
    fn split_program_strips_sudo_flags_and_paths() {
        let (prog, rest) = split_program("sudo apt-get install -y cowsay").unwrap();
        assert_eq!(prog, "apt-get");
        assert_eq!(rest, vec!["install", "-y", "cowsay"]);
        let (prog, rest) = split_program("doas -u root ls -la").unwrap();
        assert_eq!(prog, "ls");
        assert_eq!(rest, vec!["-la"]);
        assert_eq!(split_program("/usr/bin/env whoami").unwrap().0, "env");
        assert_eq!(split_program("apt list").unwrap().0, "apt");
        assert_eq!(split_program(""), None);
    }
}
