//! Approval / proposal cards for the inline client: printed as permanent
//! blocks above the live area (the card's one-letter shortcuts are handled
//! by the chat loop: y allow · n deny · s allow for this session; memory
//! and skill proposals: y keep · n discard).

use super::paint;
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
                "{} approval {} {}",
                paint::dim("┌"),
                ka_agent_core::ids::short_id(id),
                paint::dim(&format!("(expires in {left}m)"))
            );
            println!("│ {question}");
            if let Some(detail) = detail
                && !detail.is_empty()
            {
                println!("{}", paint::dim(&format!("│ {detail}")));
            }
            println!(
                "{}",
                paint::dim("└ y allow · n deny · s allow for this session")
            );
        }
        AgentEvent::MemoryProposal { id, text } => {
            println!(
                "{} remember this? {}\n│ {text}\n{}",
                paint::dim("┌"),
                paint::dim(ka_agent_core::ids::short_id(id)),
                paint::dim("└ y keep · n discard")
            );
        }
        AgentEvent::SkillProposal {
            id,
            name,
            description,
        } => {
            println!(
                "{} skill proposal {} · {name}\n│ {description}\n{}",
                paint::dim("┌"),
                ka_agent_core::ids::short_id(id),
                paint::dim("└ y install · n discard")
            );
        }
        _ => {}
    }
}
