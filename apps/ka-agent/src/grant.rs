//! Tool grants (SEED.md §8.2): explicit, revocable scopes that connector
//! tools check before any external effect.

use ka_agent_channels::grants::Grants;
use ka_agent_runtime::paths::Paths;
use ka_agent_store::Store;

use crate::GrantAction;

pub fn grant_action(action: GrantAction) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    let store = match Store::open(&paths.state_db()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ka-agent: store: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let mut grants = match Grants::load(&store) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("ka-agent: grants: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match action {
        GrantAction::Add { scope } => {
            grants.add(&scope);
            if let Err(e) = grants.save(&store) {
                eprintln!("ka-agent: {e}");
                return std::process::ExitCode::FAILURE;
            }
            println!("grant added: {scope}");
        }
        GrantAction::Remove { scope } => {
            if grants.remove(&scope) {
                if let Err(e) = grants.save(&store) {
                    eprintln!("ka-agent: {e}");
                    return std::process::ExitCode::FAILURE;
                }
                println!("grant removed: {scope}");
            } else {
                println!("grant not found: {scope}");
            }
        }
        GrantAction::List => {
            if grants.scopes.is_empty() {
                println!("no grants");
            } else {
                for s in &grants.scopes {
                    println!("{s}");
                }
            }
        }
    }
    std::process::ExitCode::SUCCESS
}
