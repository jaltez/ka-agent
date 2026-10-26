//! Approval / proposal cards for the inline client: printed as permanent
//! blocks above the live area (the card's one-letter shortcuts are handled
//! by the chat loop: y allow · n deny · s allow for this session; memory
//! and skill proposals: y keep · n discard).

use ka_agent_runtime::{AgentEvent, Envelope};

/// Print the card an envelope carries, when it carries one.
pub fn print_card(env: &Envelope) {
    match &env.ev {
        AgentEvent::Approval {
            id,
            question,
            detail,
            expires_at_ms,
            ..
        } => {
            let left = (expires_at_ms - ka_agent_core::now_ms()).max(0) / 60_000;
            println!(
                "\x1b[2m┌\x1b[0m approval {} \x1b[2m(expires in {left}m)\x1b[0m",
                ka_agent_core::ids::short_id(id)
            );
            println!("│ {}", question);
            if let Some(detail) = detail
                && !detail.is_empty()
            {
                println!("\x1b[2m│ {detail}\x1b[0m");
            }
            println!("\x1b[2m└ y allow · n deny · s allow for this session\x1b[0m");
        }
        AgentEvent::MemoryProposal { id, text } => {
            println!(
                "\x1b[2m┌\x1b[0m remember this? \x1b[2m{}\x1b[0m\n│ {text}\n\x1b[2m└ y keep · n discard\x1b[0m",
                ka_agent_core::ids::short_id(id)
            );
        }
        AgentEvent::SkillProposal {
            id,
            name,
            description,
        } => {
            println!(
                "\x1b[2m┌\x1b[0m skill proposal {} · {name}\n│ {description}\n\x1b[2m└ y install · n discard\x1b[0m",
                ka_agent_core::ids::short_id(id)
            );
        }
        _ => {}
    }
}
