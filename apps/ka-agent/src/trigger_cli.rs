//! `ka-agent trigger …`: list inbound triggers and set a webhook's memory
//! scope. The trigger row lives in the store; the daemon reads it at fire
//! time, so no restart is needed.

use std::process::ExitCode;

use ka_agent_core::ids::{id_matches, short_id};
use ka_agent_core::{TriggerKind, TriggerMemoryScope, TriggerSpec};
use ka_agent_runtime::paths::Paths;
use ka_agent_store::Store;

use crate::cli::TriggerAction;

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("ka-agent: {msg}");
    ExitCode::FAILURE
}

fn open() -> Result<Store, String> {
    let paths = Paths::new(Paths::default_root());
    paths
        .ensure_dirs()
        .map_err(|e| format!("{}: {e}", paths.root().display()))?;
    Store::open(&paths.state_db()).map_err(|e| format!("{}: {e}", paths.state_db().display()))
}

pub fn trigger_action(action: Option<TriggerAction>, json: bool) -> ExitCode {
    let store = match open() {
        Ok(store) => store,
        Err(e) => return fail(e),
    };
    let tasks = match store.trigger_tasks() {
        Ok(tasks) => tasks,
        Err(e) => return fail(e),
    };
    match action.unwrap_or(TriggerAction::List) {
        TriggerAction::List => {
            if json {
                let rows: Vec<serde_json::Value> = tasks
                    .iter()
                    .map(|task| {
                        let spec = task
                            .payload
                            .as_deref()
                            .and_then(TriggerSpec::from_payload);
                        serde_json::json!({
                            "id": task.id.as_str(),
                            "title": task.title,
                            "status": format!("{:?}", task.status).to_lowercase(),
                            "kind": spec.as_ref().map(|s| format!("{:?}", s.kind).to_lowercase()),
                            "memory_scope": spec.as_ref().map(|s| s.effective_memory_scope().as_str()),
                            "hook_path": spec.as_ref().and_then(|s| s.hook_path.as_deref()),
                            "next_ms": task.scheduled_at_ms,
                        })
                    })
                    .collect();
                println!("{}", serde_json::json!({ "ok": true, "triggers": rows }));
            } else if tasks.is_empty() {
                println!("no live triggers");
            } else {
                for task in &tasks {
                    let spec = task.payload.as_deref().and_then(TriggerSpec::from_payload);
                    let kind = spec
                        .as_ref()
                        .map(|s| format!("{:?}", s.kind).to_lowercase())
                        .unwrap_or_else(|| "?".into());
                    let scope = spec
                        .as_ref()
                        .map(|s| s.effective_memory_scope().as_str())
                        .unwrap_or("?");
                    println!(
                        "{} {:<8} {:<9} memory:{:<10} {}",
                        short_id(task.id.as_str()),
                        kind,
                        format!("{:?}", task.status).to_lowercase(),
                        scope,
                        task.title
                    );
                }
            }
            ExitCode::SUCCESS
        }
        TriggerAction::Scope { id, scope } => {
            let Some(scope) = TriggerMemoryScope::parse(&scope.trim().to_ascii_lowercase()) else {
                return fail("scope must be `automation` or `creator`");
            };
            match set_scope(&store, &id, scope) {
                Ok(short) => {
                    println!("trigger {short} memory scope {}", scope.as_str());
                    ExitCode::SUCCESS
                }
                Err(e) => fail(e),
            }
        }
    }
}

fn set_scope(store: &Store, id: &str, scope: TriggerMemoryScope) -> Result<String, String> {
    let tasks = store.trigger_tasks().map_err(|e| e.to_string())?;
    let matches: Vec<_> = tasks
        .iter()
        .filter(|task| id_matches(task.id.as_str(), id))
        .collect();
    let task = match matches.as_slice() {
        [one] => *one,
        [] => return Err(format!("no live trigger matches {id:?}")),
        _ => return Err(format!("ambiguous trigger id {id:?}")),
    };
    let Some(mut spec) = task.payload.as_deref().and_then(TriggerSpec::from_payload) else {
        return Err(format!("trigger {id} has no readable payload"));
    };
    if spec.kind == TriggerKind::Schedule {
        return Err("schedule triggers always run automation memory; scope is webhook-only".into());
    }
    spec.memory_scope = scope;
    store
        .update_task_payload(&task.id, &spec.to_payload())
        .map_err(|e| e.to_string())?;
    Ok(short_id(task.id.as_str()).to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::set_scope;
    use ka_agent_core::{Task, TriggerMemoryScope, TriggerSpec};
    use ka_agent_store::Store;

    fn trigger_task(store: &Store) -> ka_agent_core::TaskId {
        let spec = TriggerSpec::from_payload(
            r#"{"kind":"webhook","text":"triage","source":"operator","session_id":"sess_x"}"#,
        )
        .unwrap();
        let mut task = Task::new("triage".into(), "trigger", false);
        task.payload = Some(spec.to_payload());
        store.insert_task(&task).unwrap();
        task.id
    }

    #[test]
    fn scope_changes_only_a_webhook_trigger() {
        let store = Store::open_in_memory().unwrap();
        let id = trigger_task(&store);
        let short = set_scope(&store, id.as_str(), TriggerMemoryScope::Creator).unwrap();
        assert!(!short.is_empty());
        let task = store.get_task(&id).unwrap().unwrap();
        let spec = TriggerSpec::from_payload(task.payload.as_deref().unwrap()).unwrap();
        assert_eq!(spec.memory_scope, TriggerMemoryScope::Creator);

        let mut schedule = Task::new("schedule".into(), "trigger", false);
        let mut spec = spec;
        spec.kind = ka_agent_core::TriggerKind::Schedule;
        schedule.payload = Some(spec.to_payload());
        store.insert_task(&schedule).unwrap();
        assert!(set_scope(&store, schedule.id.as_str(), TriggerMemoryScope::Creator).is_err());
    }
}
