//! `ka-agent skills …` (SEED.md §9.1): the operator's skill library is the
//! active profile's `<data>/skills/<name>/SKILL.md`, with `~/.ka-agent/skills`
//! as the global fallback; `enable` provisions (copies) a skill into one
//! engine home where ka discovers it (`--role` picks a role's, default the
//! admin's), `disable` removes it. A skill is text, not code — it never
//! changes what the engine may do.

use ka_agent_core::DaemonConfig;
use ka_agent_runtime::paths::Paths;
use ka_agent_runtime::skills::Skills;

use crate::SkillsAction;

/// The engine home one role's skills are provisioned into (`admin` keeps
/// the profile's own engine home).
fn engine_home(paths: &Paths, role: &str) -> std::path::PathBuf {
    if role == ka_agent_core::ADMIN_ROLE {
        paths.engine_home()
    } else {
        paths.role_engine_home(role)
    }
}

/// The profile's role names (empty when there is no config yet).
fn configured_roles() -> Vec<String> {
    let path = ka_agent_core::layout::default_config_path();
    DaemonConfig::load(&path)
        .map(|cfg| cfg.roles.into_iter().map(|r| r.name).collect())
        .unwrap_or_default()
}

fn valid_role(role: &str, roles: &[String]) -> Result<(), String> {
    if role == ka_agent_core::ADMIN_ROLE || roles.iter().any(|r| r == role) {
        Ok(())
    } else {
        Err(format!(
            "role {role:?} is not defined in {}",
            ka_agent_core::layout::default_config_path().display()
        ))
    }
}

pub fn skills_action(action: SkillsAction) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    let admin = Skills::new(&paths.engine_home());
    match action {
        SkillsAction::List => {
            let available = admin.available();
            let roles = configured_roles();
            // Every engine home a skill may be enabled in, admin first.
            let mut homes: Vec<(String, Skills)> =
                vec![("".to_string(), Skills::new(&paths.engine_home()))];
            for role in &roles {
                homes.push((role.clone(), Skills::new(&engine_home(&paths, role))));
            }
            let any_enabled = homes.iter().any(|(_, s)| !s.enabled().is_empty());
            if available.is_empty() && !any_enabled {
                println!(
                    "no skills in {} or {} (one directory per skill, each with a SKILL.md)",
                    ka_agent_runtime::skills::user_skills_dirs()[0].display(),
                    ka_agent_runtime::skills::user_skills_dirs()[1].display()
                );
                return std::process::ExitCode::SUCCESS;
            }
            let mark = |name: &str| -> String {
                let mut marks = Vec::new();
                for (role, skills) in &homes {
                    if skills.enabled().iter().any(|e| e.name == name) {
                        marks.push(if role.is_empty() {
                            "[enabled]".to_string()
                        } else {
                            format!("[enabled@{role}]")
                        });
                    }
                }
                if marks.is_empty() {
                    String::new()
                } else {
                    format!(" {}", marks.join(" "))
                }
            };
            for s in &available {
                if s.description.is_empty() {
                    println!("{}{}", s.name, mark(&s.name));
                } else {
                    println!("{}{} — {}", s.name, mark(&s.name), s.description);
                }
            }
            std::process::ExitCode::SUCCESS
        }
        SkillsAction::Enable { name, role } => {
            let role = role.unwrap_or_else(|| ka_agent_core::ADMIN_ROLE.to_string());
            if let Err(e) = valid_role(&role, &configured_roles()) {
                eprintln!("ka-agent: {e}");
                return std::process::ExitCode::FAILURE;
            }
            let skills = Skills::new(&engine_home(&paths, &role));
            match skills.enable(&name) {
                Ok(true) => {
                    println!(
                        "enabled {name} for role {role} (the engine lists it; invoke with /skill {name} <text>)"
                    );
                    std::process::ExitCode::SUCCESS
                }
                Ok(false) => {
                    eprintln!(
                        "ka-agent: no skill {name:?} in {}",
                        ka_agent_runtime::skills::user_skills_dirs()[0].display()
                    );
                    std::process::ExitCode::FAILURE
                }
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        SkillsAction::Disable { name, role } => {
            let role = role.unwrap_or_else(|| ka_agent_core::ADMIN_ROLE.to_string());
            if let Err(e) = valid_role(&role, &configured_roles()) {
                eprintln!("ka-agent: {e}");
                return std::process::ExitCode::FAILURE;
            }
            let skills = Skills::new(&engine_home(&paths, &role));
            match skills.disable(&name) {
                Ok(true) => {
                    println!("disabled {name} for role {role}");
                    std::process::ExitCode::SUCCESS
                }
                Ok(false) => {
                    eprintln!("ka-agent: skill {name:?} is not enabled for role {role}");
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
