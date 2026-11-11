//! Identity linking (SEED.md §8.1): which channel addresses are this
//! instance's one operator. CLI-only and audited by design.

use ka_agent_core::DaemonConfig;
use ka_agent_runtime::paths::Paths;
use ka_agent_store::Store;

use crate::IdentityAction;

/// A role an identity may act as: `admin` (implicit) or a name the active
/// profile's config defines. Naming the config file makes a typo fixable.
fn valid_role(role: &str) -> Result<(), String> {
    if role == ka_agent_core::ADMIN_ROLE {
        return Ok(());
    }
    let path = ka_agent_core::layout::default_config_path();
    if !path.exists() {
        return Err(format!(
            "role {role:?}: {} does not exist — define [[role]] name = {role:?} first",
            path.display()
        ));
    }
    let cfg = DaemonConfig::load(&path)
        .map_err(|e| format!("role {role:?}: cannot read {}: {e}", path.display()))?;
    if cfg.roles.iter().any(|r| r.name == role) {
        Ok(())
    } else {
        Err(format!(
            "role {role:?} is not defined in {} (add a [[role]] with name = {role:?})",
            path.display()
        ))
    }
}

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
    let audit = |action: &str, subject: &str, role: &str, detail: Option<String>| {
        let mut rec =
            ka_agent_core::AuditRecord::new("operator", action, subject, detail).with_role(role);
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
                    "{}:{}  role {}  via {} at {}",
                    id.channel,
                    id.address,
                    id.role,
                    id.via,
                    {
                        let z = jiff::Timestamp::from_millisecond(id.linked_ms)
                            .map(|t| t.to_zoned(jiff::tz::TimeZone::system()))
                            .unwrap_or_else(|_| {
                                jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::system())
                            });
                        format!(
                            "{:04}-{:02}-{:02} {:02}:{:02}",
                            z.year(),
                            z.month() as u8,
                            z.day(),
                            z.hour(),
                            z.minute()
                        )
                    }
                );
            }
            std::process::ExitCode::SUCCESS
        }
        IdentityAction::Link {
            channel,
            address,
            role,
        } => {
            if !valid_channel_name(&channel) {
                eprintln!("ka-agent: channel name {channel:?}: [a-z0-9-] only");
                return std::process::ExitCode::FAILURE;
            }
            if address.trim().is_empty() || address.contains(char::is_control) {
                eprintln!("ka-agent: address must be non-empty printable text");
                return std::process::ExitCode::FAILURE;
            }
            if let Err(e) = valid_role(&role) {
                eprintln!("ka-agent: {e}");
                return std::process::ExitCode::FAILURE;
            }
            match store.link_identity_as(&channel, address.trim(), "cli", &role) {
                Ok(true) => {
                    audit(
                        ka_agent_core::audit::actions::IDENTITY_LINK,
                        &format!("{channel}:{}", address.trim()),
                        &role,
                        Some(format!("role {role}")),
                    );
                    println!("linked {}:{} as role {role}", channel, address.trim());
                    std::process::ExitCode::SUCCESS
                }
                Ok(false) => {
                    println!(
                        "{}:{} was already linked (its role is unchanged; use `identity role` to reassign)",
                        channel,
                        address.trim()
                    );
                    std::process::ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        IdentityAction::Pending => {
            let contacts = ka_agent_channels::pairing::unauthorized_contacts(&store, 50);
            if contacts.is_empty() {
                println!("no unknown contacts");
                return std::process::ExitCode::SUCCESS;
            }
            let now = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::system());
            println!("channel  address  last seen  attempts");
            for c in &contacts {
                println!(
                    "{}  {}  {}  {}",
                    c.channel,
                    c.address,
                    ka_agent_core::when::describe(c.last_ms, &now),
                    c.count
                );
            }
            for c in &contacts {
                println!("  link: ka-agent identity link {} {}", c.channel, c.address);
            }
            std::process::ExitCode::SUCCESS
        }
        IdentityAction::Role {
            channel,
            address,
            role,
        } => {
            if let Err(e) = valid_role(&role) {
                eprintln!("ka-agent: {e}");
                return std::process::ExitCode::FAILURE;
            }
            let previous = match store.identity_role(&channel, address.trim()) {
                Ok(Some(role)) => role,
                Ok(None) => {
                    eprintln!("ka-agent: no such identity {channel}:{}", address.trim());
                    return std::process::ExitCode::FAILURE;
                }
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            match store.set_identity_role(&channel, address.trim(), &role) {
                Ok(_) => {
                    audit(
                        ka_agent_core::audit::actions::IDENTITY_ROLE,
                        &format!("{channel}:{}", address.trim()),
                        &role,
                        Some(format!("{previous} → {role}")),
                    );
                    println!("{}:{} role: {previous} → {role}", channel, address.trim());
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
                        ka_agent_core::ADMIN_ROLE,
                        None,
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
