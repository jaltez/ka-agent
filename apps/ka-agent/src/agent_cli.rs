//! Shared agents (M3): create, list, share and remove a definition that
//! several users can chat with. Run state stays per user.

use std::process::ExitCode;

use ka_agent_runtime::paths::Paths;
use ka_agent_store::{AgentAccess, AgentRow, Store};

use crate::AgentAction;

fn open_store() -> Result<Store, String> {
    let paths = Paths::new(Paths::default_root());
    paths
        .ensure_dirs()
        .map_err(|e| format!("cannot create {}: {e}", paths.root().display()))?;
    Store::open(&paths.state_db()).map_err(|e| format!("cannot open state.db: {e}"))
}

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("ka-agent: {msg}");
    ExitCode::FAILURE
}

fn audit(store: &Store, action: &str, subject: &str, detail: Option<String>) {
    let mut rec = ka_agent_core::AuditRecord::new("operator", action, subject, detail)
        .with_user(ka_agent_core::OPERATOR_USER);
    let _ = store.append_audit(&mut rec);
}

pub fn agent_action(action: AgentAction) -> ExitCode {
    let store = match open_store() {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    match action {
        AgentAction::List => {
            let agents = match store.list_agents() {
                Ok(a) => a,
                Err(e) => return fail(e),
            };
            if agents.is_empty() {
                println!("no shared agents");
            }
            for a in agents {
                let grants: Vec<String> = store
                    .agent_access(&a.name)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|g| format!("{}={}", g.group_name, g.level))
                    .collect();
                println!(
                    "{:<20} owner {:<12} model {:<24} skills [{}] kbs [{}] groups [{}]",
                    a.name,
                    a.owner_username,
                    a.model.as_deref().unwrap_or("user default"),
                    a.skills.join(","),
                    a.kbs.join(","),
                    grants.join(",")
                );
            }
            ExitCode::SUCCESS
        }
        AgentAction::Create {
            name,
            persona,
            model,
            skills,
            kbs,
            owner,
        } => {
            let name = name
                .unwrap_or_else(|| "agent".to_string())
                .to_ascii_lowercase();
            if !ka_agent_store::valid_username(&name) {
                return fail("agent name: lowercase letters, digits, dot, dash or underscore");
            }
            let persona = persona.unwrap_or_else(|| {
                "You are a shared team agent. Answer carefully and cite the knowledge you use."
                    .to_string()
            });
            let agent = AgentRow {
                name: name.clone(),
                persona,
                model,
                skills,
                kbs,
                owner_username: owner.clone(),
                created_ms: ka_agent_core::now_ms(),
            };
            match store.create_agent(&agent) {
                Ok(()) => {
                    audit(&store, "agent.create", &name, Some(format!("owner {owner}")));
                    println!("created agent {name}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(e),
            }
        }
        AgentAction::Share {
            name,
            group,
            level,
        } => {
            if !matches!(level.as_str(), "viewer" | "editor" | "owner") {
                return fail("level must be viewer, editor or owner");
            }
            let mut grants = store.agent_access(&name).unwrap_or_default();
            grants.retain(|g| g.group_name != group);
            grants.push(AgentAccess {
                group_name: group.clone(),
                level,
            });
            match store.set_agent_access(&name, &grants) {
                Ok(()) => {
                    audit(&store, "agent.share", &name, Some(group));
                    println!("shared {name}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(e),
            }
        }
        AgentAction::Unshare { name, group } => {
            let mut grants = store.agent_access(&name).unwrap_or_default();
            grants.retain(|g| g.group_name != group);
            match store.set_agent_access(&name, &grants) {
                Ok(()) => {
                    println!("unshared {name} from {group}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(e),
            }
        }
        AgentAction::Transfer { name, owner } => match store.transfer_agent(&name, &owner) {
            Ok(()) => {
                audit(&store, "agent.transfer", &name, Some(owner.clone()));
                println!("{name} now belongs to {owner}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        },
        AgentAction::Rm { name } => match store.delete_agent(&name) {
            Ok(true) => {
                audit(&store, "agent.delete", &name, None);
                println!("deleted {name}");
                ExitCode::SUCCESS
            }
            Ok(false) => fail(format!("no agent {name}")),
            Err(e) => fail(e),
        },
    }
}
