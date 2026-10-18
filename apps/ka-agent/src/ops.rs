//! Ops commands: backup, restore, export, purge, and the web login token.
//! Everything here is local file work against the data root.

use std::path::PathBuf;

use ka_agent_runtime::paths::{InstanceLock, Paths};
use ka_agent_store::Store;

/// `YYYY-MM-DDTHH:MM:SSZ` for backup manifests (dependency-free).
fn backup_ts(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // days since epoch → civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// `ka-agent backup <dir>`: a restorable snapshot — the SQLite state via
/// `VACUUM INTO` (consistent even while the daemon writes), the memory repo
/// (git history included), notes, and the config. Written to a staging dir
/// and renamed, so a half-written backup is never visible.
/// `ka-agent backup [OUT] [--keep N]`. No `OUT`: `<root>/backups/<ts>`,
/// pruned to the newest `keep` (default 7). An explicit `OUT` is never
/// pruned and must not exist.
pub fn backup_to(out: Option<PathBuf>, keep: usize, config: PathBuf) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    let pruned = out.is_none();
    let out = out.unwrap_or_else(|| {
        let stamp = backup_ts(ka_agent_core::now_ms()).replace(':', "-");
        paths.backups().join(stamp)
    });
    if out.exists() {
        eprintln!("ka-agent: {} already exists", out.display());
        return std::process::ExitCode::FAILURE;
    }
    let store = match Store::open(&paths.state_db()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ka-agent: cannot open {}: {e}", paths.state_db().display());
            return std::process::ExitCode::FAILURE;
        }
    };
    let staging = out.with_extension("staging");
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    if let Err(e) = std::fs::create_dir_all(&staging) {
        eprintln!("ka-agent: {}: {e}", staging.display());
        return std::process::ExitCode::FAILURE;
    }
    if let Err(e) = store.backup_to(&staging.join("state.db")) {
        eprintln!("ka-agent: state backup: {e}");
        return std::process::ExitCode::FAILURE;
    }
    for dir in ["memory", "notes"] {
        let src = paths.root().join(dir);
        if src.exists() {
            let mut files = Vec::new();
            copy_tree(&src, &staging.join(dir), &mut files);
        }
    }
    if config.exists()
        && let Err(e) = std::fs::copy(&config, staging.join("config.toml"))
    {
        eprintln!("ka-agent: copy config: {e}");
        return std::process::ExitCode::FAILURE;
    }
    let manifest = serde_json::json!({
        "version": 1,
        "at": backup_ts(ka_agent_core::now_ms()),
        "ka_agent": env!("CARGO_PKG_VERSION"),
        "ka_pin": ka_agent_port::KA_PIN,
    });
    if let Err(e) = std::fs::write(
        staging.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap_or_default(),
    ) {
        eprintln!("ka-agent: manifest: {e}");
        return std::process::ExitCode::FAILURE;
    }
    if let Err(e) = std::fs::rename(&staging, &out) {
        eprintln!("ka-agent: rename to {}: {e}", out.display());
        return std::process::ExitCode::FAILURE;
    }
    // A pointer so `doctor` can report freshness without copying anything.
    let pointers = paths.root().join("backups");
    let _ = std::fs::create_dir_all(&pointers);
    let _ = std::fs::write(
        pointers.join("latest.json"),
        serde_json::to_string(&serde_json::json!({
            "path": out.display().to_string(),
            "at": manifest["at"],
            "at_ms": ka_agent_core::now_ms(),
        }))
        .unwrap_or_default(),
    );
    println!(
        "backup at {} ({})",
        out.display(),
        manifest["at"].as_str().unwrap_or("?")
    );
    if pruned {
        prune_backups(&paths.backups(), keep.max(1));
    }
    std::process::ExitCode::SUCCESS
}

/// Keep the newest `keep` timestamped backup directories. Only names that
/// look like our `<date>T<time>Z` stamps are pruned; explicit user-chosen
/// paths elsewhere are untouched.
fn prune_backups(dir: &std::path::Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut stamps: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            n.len() == "2026-10-30T18-30-00Z".len()
                && n.as_bytes()[4] == b'-'
                && n.as_bytes()[10] == b'T'
        })
        .collect();
    stamps.sort();
    if stamps.len() > keep {
        for name in &stamps[..stamps.len() - keep] {
            let _ = std::fs::remove_dir_all(dir.join(name));
        }
    }
}

/// `ka-agent restore <dir>`: unpack a backup into the data root. Refuses a
/// non-empty root unless `--force` (restoring over a live instance is how
/// data dies).
pub fn restore_from(dir: PathBuf, force: bool) -> std::process::ExitCode {
    for required in ["state.db", "manifest.json"] {
        if !dir.join(required).exists() {
            eprintln!(
                "ka-agent: {} is not a backup (no {required})",
                dir.display()
            );
            return std::process::ExitCode::FAILURE;
        }
    }
    let paths = Paths::new(Paths::default_root());
    let root = paths.root();
    let root_is_empty = std::fs::read_dir(root)
        .map(|mut d| d.next().is_none())
        .unwrap_or(true);
    if !root_is_empty && !force {
        eprintln!(
            "ka-agent: data root {} is not empty — pass --force to restore over it",
            root.display()
        );
        return std::process::ExitCode::FAILURE;
    }
    if let Err(e) = paths.ensure_dirs() {
        eprintln!("ka-agent: {}: {e}", root.display());
        return std::process::ExitCode::FAILURE;
    }
    // A live daemon (or local chat) holds SQLite locks; refuse rather than
    // fight it. The instance lock is the proof; a leftover socket is not.
    if InstanceLock::is_held(&paths) {
        eprintln!("ka-agent: a daemon or chat is running — stop it first");
        return std::process::ExitCode::FAILURE;
    }
    let sock = root.join("admin.sock");
    if sock.exists() {
        println!("removing stale admin socket (no daemon holds the data root)");
        let _ = std::fs::remove_file(&sock);
    }
    if let Err(e) = std::fs::copy(dir.join("state.db"), root.join("state.db")) {
        eprintln!("ka-agent: state: {e}");
        return std::process::ExitCode::FAILURE;
    }
    for name in ["memory", "notes"] {
        let src = dir.join(name);
        if src.exists() {
            let dst = root.join(name);
            if dst.exists() {
                let _ = std::fs::remove_dir_all(&dst);
            }
            let mut files = Vec::new();
            copy_tree(&src, &dst, &mut files);
        }
    }
    // The restored store must open and report a known schema.
    match Store::open(&root.join("state.db")) {
        Ok(s) => {
            let v = s.user_version().unwrap_or(0);
            println!("restored into {} (schema v{v})", root.display());
        }
        Err(e) => {
            eprintln!("ka-agent: restored store does not open: {e}");
            return std::process::ExitCode::FAILURE;
        }
    }
    if dir.join("config.toml").exists() {
        println!(
            "config backup at {} — install it at your config path if needed",
            dir.join("config.toml").display()
        );
    }
    std::process::ExitCode::SUCCESS
}

pub fn export(out: &std::path::Path) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    std::fs::create_dir_all(out).unwrap_or_else(|e| {
        eprintln!("ka-agent: cannot create {out:?}: {e}");
        std::process::exit(1);
    });
    let mut manifest = Vec::new();
    // Memory Markdown
    let mem = paths.root().join("memory");
    if mem.is_dir() {
        let dest = out.join("memory");
        if std::fs::create_dir_all(&dest).is_ok() {
            copy_tree(&mem, &dest, &mut manifest);
        }
    }
    // Notes
    let notes = paths.root().join("notes");
    if notes.is_dir() {
        let dest = out.join("notes");
        if std::fs::create_dir_all(&dest).is_ok() {
            copy_tree(&notes, &dest, &mut manifest);
        }
    }
    // State tables as JSONL
    {
        for (table, file) in [
            ("sessions", "sessions.jsonl"),
            ("turns", "turns.jsonl"),
            ("approvals", "approvals.jsonl"),
            ("intents", "intents.jsonl"),
            ("audit", "audit.jsonl"),
            ("tasks", "tasks.jsonl"),
        ] {
            let sql = format!("SELECT * FROM {table}");
            if let Ok(conn) = rusqlite::Connection::open(paths.state_db())
                && let Ok(mut stmt) = conn.prepare(&sql)
            {
                let cols: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
                let path = out.join(file);
                if let Ok(f) = std::fs::File::create(&path) {
                    let mut w = std::io::BufWriter::new(f);
                    if let Ok(mut rows) = stmt.query([]) {
                        while let Ok(Some(row)) = rows.next() {
                            let obj: std::collections::BTreeMap<String, serde_json::Value> = cols
                                .iter()
                                .map(|c| {
                                    (
                                        c.clone(),
                                        row.get_ref(c.as_str())
                                            .map(cell_to_json)
                                            .unwrap_or(serde_json::Value::Null),
                                    )
                                })
                                .collect();
                            if let Ok(line) = serde_json::to_string(&obj) {
                                writeln!(w, "{line}").ok();
                            }
                        }
                    }
                }
                manifest.push(file.to_string());
            }
        }
    }
    // Manifest
    use std::io::Write;
    let mpath = out.join("manifest.txt");
    if let Ok(mut f) = std::fs::File::create(&mpath) {
        for entry in &manifest {
            writeln!(f, "{entry}").ok();
        }
    }
    println!("exported {} files to {}", manifest.len(), out.display());
    std::process::ExitCode::SUCCESS
}

fn cell_to_json(cell: rusqlite::types::ValueRef<'_>) -> serde_json::Value {
    use rusqlite::types::ValueRef;
    match cell {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(i) => serde_json::Value::Number(i.into()),
        ValueRef::Real(f) => serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        ValueRef::Text(t) => serde_json::Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => serde_json::Value::Array(
            b.iter()
                .map(|&x| serde_json::Value::Number(x.into()))
                .collect(),
        ),
    }
}

fn copy_tree(src: &std::path::Path, dest: &std::path::Path, manifest: &mut Vec<String>) {
    if let Ok(entries) = std::fs::read_dir(src) {
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = path.strip_prefix(src).unwrap_or(&path).to_path_buf();
            let dest_path = dest.join(&rel);
            if path.is_dir() {
                std::fs::create_dir_all(&dest_path).ok();
                copy_tree(&path, &dest_path, manifest);
            } else if std::fs::copy(&path, &dest_path).is_ok() {
                manifest.push(rel.display().to_string());
            }
        }
    }
}

pub fn purge(confirm: bool) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    if !confirm {
        println!(
            "this will delete all ka-agent data in {}\nre-run with --confirm",
            paths.root().display()
        );
        return std::process::ExitCode::FAILURE;
    }
    if InstanceLock::is_held(&paths) {
        eprintln!("ka-agent: a daemon or chat is running — stop it first");
        return std::process::ExitCode::FAILURE;
    }
    match std::fs::remove_dir_all(paths.root()) {
        Ok(()) => {
            println!("purged {}", paths.root().display());
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ka-agent: purge: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
