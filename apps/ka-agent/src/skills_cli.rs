//! `ka-agent skills …` (SEED.md §9.1): the operator's skill library is
//! `~/.ka-agent/skills/<name>/SKILL.md` (a shared library; `enable` provisions it into the active profile); `enable` provisions (copies)
//! it into the engine home where ka discovers it, `disable` removes it.
//! A skill is text, not code — it never changes what the engine may do.

use ka_agent_runtime::paths::Paths;
use ka_agent_runtime::skills::Skills;

use crate::SkillsAction;

pub fn skills_action(action: SkillsAction) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    let skills = Skills::new(&paths.engine_home());
    match action {
        SkillsAction::List => {
            let available = skills.available();
            let enabled = skills.enabled();
            if available.is_empty() && enabled.is_empty() {
                println!(
                    "no skills in {} (one directory per skill, each with a SKILL.md)",
                    ka_agent_runtime::skills::user_skills_dir().display()
                );
                return std::process::ExitCode::SUCCESS;
            }
            let mark = |name: &str| {
                if enabled.iter().any(|e| e.name == name) {
                    " [enabled]"
                } else {
                    ""
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
        SkillsAction::Enable { name } => match skills.enable(&name) {
            Ok(true) => {
                println!("enabled {name} (the engine lists it; invoke with /skill {name} <text>)");
                std::process::ExitCode::SUCCESS
            }
            Ok(false) => {
                eprintln!(
                    "ka-agent: no skill {name:?} in {}",
                    ka_agent_runtime::skills::user_skills_dir().display()
                );
                std::process::ExitCode::FAILURE
            }
            Err(e) => {
                eprintln!("ka-agent: {e}");
                std::process::ExitCode::FAILURE
            }
        },
        SkillsAction::Disable { name } => match skills.disable(&name) {
            Ok(true) => {
                println!("disabled {name}");
                std::process::ExitCode::SUCCESS
            }
            Ok(false) => {
                eprintln!("ka-agent: skill {name:?} is not enabled");
                std::process::ExitCode::FAILURE
            }
            Err(e) => {
                eprintln!("ka-agent: {e}");
                std::process::ExitCode::FAILURE
            }
        },
    }
}
