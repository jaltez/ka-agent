//! The compiled identity: persona + house rules, written to
//! `<engine-home>/.config/ka/IDENTITY.md` and re-read by the engine every
//! turn (`[prompt] identity_file`, ka-harness ≥ 0.4.1).

/// The house rules appended after the persona. The `kagent__*` tools
/// arrive with the MCP bridge; naming them here keeps the assistant
/// pointing the operator at them even before they exist.
const HOW_YOU_WORK: &str = "\
- Every message starts with a `<ctx …/>` line (local time, weekday, timezone, channel) — use it for dates, never echo it.
- Durable facts -> `kagent__memory_propose` (the operator approves), never claim to remember what you did not propose.
- \"remind me...\" -> `kagent__remind` (a notification, no model turn).
- Recurring work that needs thinking -> `kagent__cron_manage`.
- Files for the operator -> `kagent__send_file`.
- Earlier conversations -> `kagent__history_search`.
- A reusable procedure you just worked out -> offer it with `kagent__skill_propose`.
- Scheduled runs (`<ctx … scheduled=\"…\"/>`): if nothing deserves attention reply exactly `[SILENT]`.
- Phone replies <= 6 lines unless asked, terminal and web may use Markdown.
- Ask before external effects (posting, sending, paying, deleting outside the workspace) and never invent results of actions you did not take.";

/// Compile the persona into the engine's identity text: an opening block
/// naming the assistant, the persona body (leading `# <Name>` heading
/// removed), and the house rules.
pub fn compile_identity(persona: &str) -> String {
    let name = ka_agent_memory::persona::persona_name(persona);
    let body = ka_agent_memory::persona::persona_body(persona);
    format!(
        "You are {name}, the resident personal assistant of one person — \"the operator\" — \
running inside ka-agent on the Ka engine. Terminal, phone (Telegram/Matrix) and the web \
panel are one continuous conversation with them.\n\n\
## Persona\n\n{body}\n\n\
## How you work\n\n{HOW_YOU_WORK}\n"
    )
}

/// sha256 of the `AGENTS.md` this runtime used to seed into workspaces
/// (removed in 2.1: identity moved to IDENTITY.md). A workspace file that
/// still hashes to this is ours — deleted on startup; a modified copy is
/// the operator's and left alone.
pub const LEGACY_AGENTS_MD_SHA256: &str =
    "1c6be7ff86af3264d6ce63a89be3c43101f2141f8b1bf2bc8ca9dc3a31e23bad";

/// A leftover seeded `AGENTS.md` in `workspace`, when its hash is exactly
/// the one we used to write. `None` when absent or operator-modified.
pub fn legacy_agents_md_status(workspace: &std::path::Path) -> Option<std::path::PathBuf> {
    use sha2::{Digest, Sha256};
    let path = workspace.join("AGENTS.md");
    let bytes = std::fs::read(&path).ok()?;
    let digest = Sha256::digest(&bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    (hex == LEGACY_AGENTS_MD_SHA256).then_some(path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn compiled_identity_names_the_persona_and_keeps_its_body() {
        let persona = "# Eve\nsharp, brief, on your side.\n- bullet one";
        let id = compile_identity(persona);
        assert!(
            id.starts_with("You are Eve, the resident personal assistant"),
            "{id}"
        );
        assert!(
            id.contains("## Persona\n\nsharp, brief, on your side.\n- bullet one"),
            "{id}"
        );
        assert!(!id.contains("# Eve\n"), "{id}");
    }

    #[test]
    fn compiled_identity_carries_every_house_rule_and_no_coding_agent_text() {
        let id = compile_identity(ka_agent_memory::persona::DEFAULT_PERSONA);
        for needle in [
            "`<ctx …/>`",
            "kagent__memory_propose",
            "kagent__remind",
            "kagent__cron_manage",
            "kagent__send_file",
            "kagent__history_search",
            "kagent__skill_propose",
            "[SILENT]",
            "Phone replies <= 6 lines",
            "Ask before external effects",
        ] {
            assert!(id.contains(needle), "missing {needle:?}:\n{id}");
        }
        assert!(!id.to_lowercase().contains("coding agent"), "{id}");
        assert!(id.contains("You are Ka,"), "{id}");
    }

    #[test]
    fn legacy_agents_md_detected_only_when_unmodified() {
        let dir = std::env::temp_dir().join(format!("ka-agents-md-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // The exact text we used to seed (hash-pinned above).
        let legacy = "# Ka Agent\n\nThis workspace is served by ka-agent, a resident personal assistant running\non the Ka engine. One operator per instance; everything is auditable.\n\n- Memory: durable facts live under `<data>/memory/` and are compiled into the\n  engine's MEMORY.md. Propose new facts with the `kagent__memory_propose`\n  tool for operator review; never store them yourself.\n- Operator commands: /help lists them all (/status /new /stop /resume\n  /model /remember /memory /history /remind /reminders /cron /calendar\n  /approvals /skills /y /n).\n- Scheduling: reminders and crons use the operator's local time. One-shot\n  `/cron add in 10m <text>`; recurring `/cron add every 1h <text>`,\n  `/cron add every day at 08:00 <text>` or\n  `/cron add every mon at 09:00 <text>`. Reminders accept `in 2h30m`,\n  `at 18:30`, `tomorrow 9am`, `fri 17:00` or `2026-11-03 10:00`. The\n  `cron_manage` tool does the same from the engine ('at' takes RFC 3339\n  with the operator's UTC offset).\n- References: `@<file>`, `@<dir>/`, `@turn:<id-prefix>`, `@https://…` inline\n  in a message inject that context into the turn.\n- External effects (HTTP writes, GitHub) require operator grants and\n  approvals; a daily budget applies.\n- Health and audit: `ka-agent doctor`, `ka-agent history`; backups under\n  `<data>/backups/`.\n";
        std::fs::write(dir.join("AGENTS.md"), legacy).unwrap();
        assert!(legacy_agents_md_status(&dir).is_some());
        std::fs::write(dir.join("AGENTS.md"), "OPERATOR-MARKER\n").unwrap();
        assert!(legacy_agents_md_status(&dir).is_none());
        std::fs::remove_file(dir.join("AGENTS.md")).unwrap();
        assert!(legacy_agents_md_status(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
