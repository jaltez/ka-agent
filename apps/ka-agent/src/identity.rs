//! Identity linking (SEED.md §8.1): which channel addresses are this
//! instance's one operator. CLI-only and audited by design.

use ka_agent_core::DaemonConfig;
use ka_agent_runtime::paths::Paths;
use ka_agent_store::Store;

use crate::IdentityAction;

pub fn identity_action(action: IdentityAction) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    if let Err(e) = paths.ensure_dirs() {
        eprintln!("ka-agent: cannot create {}: {e}", paths.root().display());
        return std::process::ExitCode::FAILURE;
    }
    let Ok(store) = Store::open(&paths.state_db()) else {
        eprintln!("ka-agent: cannot open {}", paths.state_db().display());
        return std::process::ExitCode::FAILURE;
    };
    let audit = |action: &str, subject: &str| {
        let mut rec = ka_agent_core::AuditRecord::new("operator", action, subject, None);
        let _ = store.append_audit(&mut rec);
    };
    match action {
        IdentityAction::List => {
            let ids = match store.list_identities() {
                Ok(ids) => ids,
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            if ids.is_empty() {
                println!("no identities linked (this instance answers nobody)");
            }
            for id in ids {
                println!(
                    "{}:{}  via {} at {}",
                    id.channel,
                    id.address,
                    id.via,
                    ka_agent_channels::commands::utc_compact(id.linked_ms)
                );
            }
            std::process::ExitCode::SUCCESS
        }
        IdentityAction::Link { channel, address } => {
            if !valid_channel_name(&channel) {
                eprintln!("ka-agent: channel name {channel:?}: [a-z0-9-] only");
                return std::process::ExitCode::FAILURE;
            }
            if address.trim().is_empty() || address.contains(char::is_control) {
                eprintln!("ka-agent: address must be non-empty printable text");
                return std::process::ExitCode::FAILURE;
            }
            match store.link_identity(&channel, address.trim(), "cli") {
                Ok(true) => {
                    audit(
                        ka_agent_core::audit::actions::IDENTITY_LINK,
                        &format!("{channel}:{}", address.trim()),
                    );
                    println!("linked {}:{}", channel, address.trim());
                    std::process::ExitCode::SUCCESS
                }
                Ok(false) => {
                    println!("{}:{} was already linked", channel, address.trim());
                    std::process::ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        IdentityAction::Unlink { channel, address } => {
            match store.unlink_identity(&channel, address.trim()) {
                Ok(true) => {
                    audit(
                        ka_agent_core::audit::actions::IDENTITY_UNLINK,
                        &format!("{channel}:{}", address.trim()),
                    );
                    println!("unlinked {}:{}", channel, address.trim());
                    std::process::ExitCode::SUCCESS
                }
                Ok(false) => {
                    eprintln!("ka-agent: no such identity {channel}:{}", address.trim());
                    std::process::ExitCode::FAILURE
                }
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
    }
}

fn valid_channel_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Migrate the deprecated `[telegram] allow_user_ids` into the identities
/// table (idempotent; provenance `config-migration`). Returns how many rows
/// were added, so the caller can tell the operator.
pub fn migrate_config_identities(store: &Store, cfg: &DaemonConfig) -> usize {
    let mut added = 0;
    for uid in &cfg.telegram.allow_user_ids {
        if store
            .link_identity("telegram", &uid.to_string(), "config-migration")
            .unwrap_or(false)
        {
            added += 1;
        }
    }
    added
}
