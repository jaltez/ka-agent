//! Account and knowledge-base administration (ADR-0004, D8/D10).
//!
//! The web panel calls the same store APIs; this CLI is what an admin uses
//! before the panel is reachable (invites, resets) and what scripts use.

use std::process::ExitCode;

use ka_agent_core::DaemonConfig;
use ka_agent_runtime::paths::Paths;
use ka_agent_runtime::user_ops;
use ka_agent_store::{Store, UserRow};

use crate::{KbAction, UserAction};

fn valid_role(role: &str, config: Option<&std::path::Path>) -> Result<(), String> {
    if role == ka_agent_core::ADMIN_ROLE {
        return Ok(());
    }
    let path = config
        .map(|p| p.to_path_buf())
        .unwrap_or_else(ka_agent_core::layout::default_config_path);
    let cfg = DaemonConfig::load(&path)
        .map_err(|e| format!("role {role:?}: cannot read {}: {e}", path.display()))?;
    if cfg.roles.iter().any(|r| r.name == role) {
        Ok(())
    } else {
        Err(format!(
            "role {role:?} is not defined in {} — add a [[role]] with name = {role:?}",
            path.display()
        ))
    }
}

fn random_code() -> String {
    use rand_core::RngCore;
    let mut buf = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Open the store the active profile owns.
fn open_store() -> Result<(Paths, Store), String> {
    let paths = Paths::new(Paths::default_root());
    paths
        .ensure_dirs()
        .map_err(|e| format!("cannot create {}: {e}", paths.root().display()))?;
    let store = Store::open(&paths.state_db())
        .map_err(|e| format!("cannot open {}: {e}", paths.state_db().display()))?;
    Ok((paths, store))
}

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("ka-agent: {msg}");
    ExitCode::FAILURE
}

pub fn user_action(action: UserAction) -> ExitCode {
    let (paths, store) = match open_store() {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let audit = |action: &str, subject: &str, detail: Option<String>| {
        let mut rec = ka_agent_core::AuditRecord::new("operator", action, subject, detail)
            .with_user(ka_agent_core::OPERATOR_USER);
        let _ = store.append_audit(&mut rec);
    };
    match action {
        UserAction::List => {
            let users = match store.list_users() {
                Ok(u) => u,
                Err(e) => return fail(e),
            };
            if users.is_empty() {
                println!("no accounts");
            }
            for u in users {
                let groups = store.groups_of(&u.username).unwrap_or_default();
                let identities = store
                    .identities_for_user(&u.username)
                    .map(|v| {
                        v.iter()
                            .map(|i| format!("{}:{}", i.channel, i.address))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                println!(
                    "{:<16} role {:<10} {:<9} quota {:<8} groups [{}] identities [{}]{}",
                    u.username,
                    u.role,
                    if u.disabled { "disabled" } else { "active" },
                    u.quota_daily_usd
                        .map(|q| format!("${q:.2}"))
                        .unwrap_or_else(|| "role".into()),
                    groups.join(","),
                    identities,
                    if u.pw_hash.is_some() {
                        ""
                    } else {
                        "  (no password: waiting for an invite)"
                    }
                );
            }
            ExitCode::SUCCESS
        }
        UserAction::Invite {
            role,
            groups,
            days,
            config,
        } => {
            if let Err(e) = valid_role(&role, config.as_deref()) {
                return fail(e);
            }
            for g in &groups {
                if let Err(e) = store.ensure_group(g) {
                    return fail(format!("group {g}: {e}"));
                }
            }
            let code = random_code();
            let now = ka_agent_core::now_ms();
            let invite = ka_agent_store::InviteRow {
                code_hash: sha256_hex(&code),
                role: role.clone(),
                username: None,
                groups: groups.clone(),
                created_ms: now,
                expires_ms: now + (days.max(1) as i64) * 24 * 60 * 60 * 1000,
                used_ms: None,
                created_by: ka_agent_core::OPERATOR_USER.to_string(),
            };
            if let Err(e) = store.insert_invite(&invite) {
                return fail(e);
            }
            audit(
                "user.invite",
                &role,
                Some(format!("groups {}", groups.join(","))),
            );
            println!("invite code: {code}");
            println!(
                "role {role}{} · valid {} day(s) · single use",
                if groups.is_empty() {
                    String::new()
                } else {
                    format!(" · groups {}", groups.join(","))
                },
                days.max(1)
            );
            println!(
                "redeem at {}/invite (or: the person picks a username and password)",
                base_url(&store)
            );
            ExitCode::SUCCESS
        }
        UserAction::Disable { username } => {
            let user = match store.get_user(&username) {
                Ok(Some(u)) => u,
                Ok(None) => return fail(format!("no account {username}")),
                Err(e) => return fail(e),
            };
            if user.username == ka_agent_core::OPERATOR_USER {
                return fail("refusing to disable the operator's own account");
            }
            if let Err(e) = store.set_user_disabled(&username, true) {
                return fail(e);
            }
            let _ = store.delete_web_sessions_for(&username);
            let paused = store
                .cron_tasks_for_user(&username)
                .map(|tasks| {
                    let n = tasks.len();
                    for t in tasks {
                        let _ = store.set_task_status(&t.id, "interrupted");
                    }
                    n
                })
                .unwrap_or(0);
            audit("user.disable", &username, Some(format!("{paused} cron(s) paused")));
            println!(
                "disabled {username}: web sessions dropped, identities silent, {paused} cron(s) paused; its memory stays (export/purge decide its fate)"
            );
            ExitCode::SUCCESS
        }
        UserAction::Enable { username } => {
            if store.get_user(&username).ok().flatten().is_none() {
                return fail(format!("no account {username} — `ka-agent user add {username}` first"));
            }
            if let Err(e) = store.set_user_disabled(&username, false) {
                return fail(e);
            }
            let resumed = store
                .resume_user_crons(&username)
                .map_err(fail)
                .unwrap_or(0);
            audit("user.enable", &username, Some(format!("{resumed} cron(s) resumed")));
            println!(
                "enabled {username}{}",
                if resumed > 0 {
                    format!(": {resumed} paused cron(s) resumed")
                } else {
                    String::new()
                }
            );
            ExitCode::SUCCESS
        }
        UserAction::Set {
            username,
            role,
            quota,
            groups,
        } => {
            let Some(user) = store.get_user(&username).ok().flatten() else {
                return fail(format!("no account {username}"));
            };
            if let Some(role) = &role {
                if let Err(e) = valid_role(role, None) {
                    return fail(e);
                }
                if let Err(e) = store.set_user_role(&username, role) {
                    return fail(e);
                }
            }
            if let Some(quota) = quota {
                let value = (quota > 0.0).then_some(quota);
                if let Err(e) = store.set_user_quota(&username, value) {
                    return fail(e);
                }
            }
            if let Some(groups) = &groups {
                for g in groups.iter() {
                    if let Err(e) = store.ensure_group(g) {
                        return fail(format!("group {g}: {e}"));
                    }
                }
                if let Err(e) = store.set_user_groups(&username, groups) {
                    return fail(e);
                }
            }
            audit("user.edit", &username, None);
            let updated = store.get_user(&username).ok().flatten().unwrap_or(user);
            println!(
                "{username}: role {} · quota {} · groups [{}]",
                updated.role,
                updated
                    .quota_daily_usd
                    .map(|q| format!("${q:.2}"))
                    .unwrap_or_else(|| "role".into()),
                store.groups_of(&username).unwrap_or_default().join(",")
            );
            ExitCode::SUCCESS
        }
        UserAction::Reset { username } => {
            let Some(user) = store.get_user(&username).ok().flatten() else {
                return fail(format!("no account {username}"));
            };
            if let Err(e) = store.set_user_password(&username, None) {
                return fail(e);
            }
            let _ = store.delete_web_sessions_for(&username);
            // A reset re-opens the account through a fresh invite; the
            // existing row is deleted so the code can create it again with
            // the same username (an invite never overwrites an account).
            // A reset code reopens this one account (never creates another).
            let code = random_code();
            let now = ka_agent_core::now_ms();
            let invite = ka_agent_store::InviteRow {
                code_hash: sha256_hex(&code),
                role: user.role.clone(),
                username: Some(username.clone()),
                groups: Vec::new(),
                created_ms: now,
                expires_ms: now + 24 * 60 * 60 * 1000,
                used_ms: None,
                created_by: ka_agent_core::OPERATOR_USER.to_string(),
            };
            if let Err(e) = store.insert_invite(&invite) {
                return fail(e);
            }
            audit("user.reset", &username, None);
            println!("password cleared for {username}; its web sessions are gone");
            println!("reset code: {code}");
            println!(
                "{username} opens {}/invite, pastes the code and chooses a new password (24 h, single use)",
                base_url(&store)
            );
            ExitCode::SUCCESS
        }
        UserAction::Export { username } => match user_ops::export_user(&paths, &username) {
            Ok(path) => {
                audit("user.export", &username, Some(path.display().to_string()));
                println!("exported {username} to {}", path.display());
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        },
        UserAction::Purge { username, yes } => {
            if !yes {
                return fail(format!(
                    "refusing without --yes: `ka-agent user purge {username} --yes` deletes their memory, notes, attachments and workspace"
                ));
            }
            if user_ops::latest_export(&paths, &username).is_none() {
                eprintln!("ka-agent: warning: no export on record for {username} — this is irreversible");
            }
            match user_ops::purge_user(&paths, &username) {
                Ok(removed) => {
                    audit("user.purge", &username, Some(removed.join(",")));
                    println!("purged {}: {}", username, removed.join(", "));
                    ExitCode::SUCCESS
                }
                Err(e) => fail(e),
            }
        }
    }
}

/// The panel URL the store knows (written by the server), else loopback.
fn base_url(store: &Store) -> String {
    store
        .meta_get("web.url")
        .ok()
        .flatten()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8480".into())
}

/// The document formats M1 indexes (text-like; extraction engines are a
/// later, separate decision).
const KB_FORMATS: &[&str] = &["md", "txt", "csv", "json"];

pub fn kb_action(action: KbAction) -> ExitCode {
    let (_, store) = match open_store() {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    match action {
        KbAction::List => {
            let kbs = match store.list_kbs() {
                Ok(k) => k,
                Err(e) => return fail(e),
            };
            if kbs.is_empty() {
                println!("no knowledge bases");
            }
            for kb in kbs {
                let access = store.kb_access(&kb.name).unwrap_or_default();
                let docs = store.kb_doc_count(&kb.name).unwrap_or(0);
                println!(
                    "{:<16} {:<24} owner {:<12} docs {:<4} groups [{}]",
                    kb.name,
                    kb.display_name,
                    kb.owner_username,
                    docs,
                    access.join(",")
                );
            }
            ExitCode::SUCCESS
        }
        KbAction::Create {
            name,
            display_name,
            groups,
        } => {
            let name = name.to_ascii_lowercase();
            if !ka_agent_store::valid_username(&name) {
                return fail("kb name: lowercase letters, digits, dot, dash or underscore");
            }
            let display = if display_name.is_empty() {
                name.clone()
            } else {
                display_name
            };
            match store.create_kb(&name, &display, ka_agent_core::OPERATOR_USER) {
                Ok(true) => {}
                Ok(false) => return fail(format!("knowledge base {name} already exists")),
                Err(e) => return fail(e),
            }
            for g in &groups {
                let _ = store.ensure_group(g);
            }
            if let Err(e) = store.set_kb_access(&name, &groups) {
                return fail(e);
            }
            println!("created {name}{}", if groups.is_empty() {
                " (no groups yet: only the owner and admins read it)".to_string()
            } else {
                format!(" readable by {}", groups.join(","))
            });
            ExitCode::SUCCESS
        }
        KbAction::Add {
            kb,
            path,
            as_path,
            title,
        } => {
            if store.get_kb(&kb).ok().flatten().is_none() {
                return fail(format!("no knowledge base {kb} — `ka-agent kb create {kb}`"));
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !KB_FORMATS.contains(&ext.as_str()) {
                return fail(format!(
                    "{}: only .md .txt .csv .json are indexed in this version (PDF and binary extraction is a later milestone)",
                    path.display()
                ));
            }
            let meta = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(e) => return fail(format!("{}: {e}", path.display())),
            };
            if meta.len() > 10 * 1024 * 1024 {
                return fail(format!("{} exceeds 10 MiB", path.display()));
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => return fail(format!("{}: {e}", path.display())),
            };
            let doc_path = as_path.unwrap_or_else(|| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("document")
                    .to_string()
            });
            let title = title.unwrap_or_else(|| doc_path.clone());
            let digest = sha256_hex(&content);
            let mtime = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or_else(ka_agent_core::now_ms);
            match store.kb_upsert_doc(&kb, &doc_path, &title, &content, &digest, "upload", mtime) {
                Ok(created) => {
                    println!(
                        "{} {} → {kb}:{doc_path} ({} bytes)",
                        if created { "added" } else { "replaced" },
                        title,
                        content.len()
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => fail(e),
            }
        }
        KbAction::Rm { kb, path } => match store.kb_remove_doc(&kb, &path) {
            Ok(true) => {
                println!("removed {kb}:{path}");
                ExitCode::SUCCESS
            }
            Ok(false) => fail(format!("no document {path} in {kb}")),
            Err(e) => fail(e),
        },
        KbAction::Delete { kb } => match store.delete_kb(&kb) {
            Ok(true) => {
                println!("deleted knowledge base {kb}");
                ExitCode::SUCCESS
            }
            Ok(false) => fail(format!("no knowledge base {kb}")),
            Err(e) => fail(e),
        },
        KbAction::Source { action } => kb_source_action(action),
        KbAction::Grant { kb, groups } => {
            if store.get_kb(&kb).ok().flatten().is_none() {
                return fail(format!("no knowledge base {kb}"));
            }
            for g in &groups {
                let _ = store.ensure_group(g);
            }
            match store.set_kb_access(&kb, &groups) {
                Ok(()) => {
                    println!("{kb} readable by [{}]", groups.join(","));
                    ExitCode::SUCCESS
                }
                Err(e) => fail(e),
            }
        }
    }
}

/// `ka-agent kb source …`: the sync configuration (the daemon runs due
/// sources from its maintenance tick).
fn kb_source_action(action: crate::KbSourceAction) -> ExitCode {
    let (paths, store) = match open_store() {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    match action {
        crate::KbSourceAction::List => {
            let sources = match store.list_kb_sources() {
                Ok(s) => s,
                Err(e) => return fail(e),
            };
            if sources.is_empty() {
                println!("no knowledge sources");
            }
            for src in sources {
                let last = store
                    .meta_get(&format!(
                        "kbsync:last:{}:{}:{}",
                        src.kb_name, src.kind, src.target
                    ))
                    .ok()
                    .flatten()
                    .and_then(|v| v.parse::<i64>().ok())
                    .map(|ms| {
                        jiff::Timestamp::from_millisecond(ms)
                            .map(|t| t.strftime("%Y-%m-%d %H:%M").to_string())
                            .unwrap_or_default()
                    })
                    .unwrap_or_else(|| "never".into());
                let error = store
                    .meta_get(&format!(
                        "kbsync:last:{}:{}:{}:error",
                        src.kb_name, src.kind, src.target
                    ))
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                println!(
                    "{:<12} {:<8} {:<32} every {:>3}m  last {}{}",
                    src.kb_name,
                    src.kind,
                    src.target,
                    src.schedule_ms / 60_000,
                    last,
                    if error.is_empty() {
                        String::new()
                    } else {
                        format!("  PAUSED: {error}")
                    }
                );
            }
            ExitCode::SUCCESS
        }
        crate::KbSourceAction::Add {
            kb,
            kind,
            target,
            token,
            include,
            every_minutes,
        } => {
            let src = ka_agent_store::KbSourceRow {
                kb_name: kb.clone(),
                kind: kind.clone(),
                target: target.clone(),
                token_secret: token,
                include_globs: include,
                cursor: None,
                schedule_ms: (every_minutes.max(1) as i64) * 60_000,
                created_ms: ka_agent_core::now_ms(),
            };
            match store.put_kb_source(&src) {
                Ok(()) => {
                    println!(
                        "{kb}: {kind} source {target} every {}m",
                        every_minutes.max(1)
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => fail(e),
            }
        }
        crate::KbSourceAction::Rm { kb, kind, target } => {
            match store.delete_kb_source(&kb, &kind, &target) {
                Ok(true) => {
                    println!("removed {kb}: {kind} {target}");
                    ExitCode::SUCCESS
                }
                Ok(false) => fail(format!("no such source {kb}: {kind} {target}")),
                Err(e) => fail(e),
            }
        }
        crate::KbSourceAction::Sync { kb, kind, target } => {
            let sources = match store.kb_sources_for(&kb) {
                Ok(s) => s,
                Err(e) => return fail(e),
            };
            let selected: Vec<_> = sources
                .into_iter()
                .filter(|s| match (&kind, &target) {
                    (Some(k), Some(t)) => &s.kind == k && &s.target == t,
                    (Some(k), None) => &s.kind == k,
                    _ => true,
                })
                .collect();
            if selected.is_empty() {
                return fail(format!("no matching sources for {kb}"));
            }
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let Ok(rt) = rt else {
                return fail("cannot start the async runtime");
            };
            let secrets = paths.root().join("secrets");
            let mut failed = false;
            for src in selected {
                let key = format!(
                    "kbsync:last:{}:{}:{}",
                    src.kb_name, src.kind, src.target
                );
                // A manual sync clears a prior failure.
                let _ = store.meta_set(&format!("{key}:error"), "");
                let _ = store.meta_set(&key, &ka_agent_core::now_ms().to_string());
                match rt.block_on(ka_agent_channels::kbsync::sync(&store, &src, &secrets)) {
                    Ok(report) => println!(
                        "{}: {} indexed, {} unchanged, {} removed",
                        report.kb, report.indexed, report.unchanged, report.removed
                    ),
                    Err(e) => {
                        let _ = store.meta_set(&format!("{key}:error"), &e);
                        eprintln!("ka-agent: {} {}: {e}", src.kb_name, src.kind);
                        failed = true;
                    }
                }
            }
            if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
    }
}

/// `UserRow` is re-exported for tests of these handlers.
#[allow(dead_code)]
fn user_row_roundtrip(u: &UserRow) -> String {
    u.username.clone()
}
