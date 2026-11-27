//! The pure skill rules (SEED.md §9.1): no I/O, so every surface —
//! provisioning, attach, import, proposal accept — screens a `SKILL.md`
//! body the same way.

/// The 1-based numbers of the `SKILL.md` lines the engine would run
/// through `sh -c` when the skill is invoked, without a permission ask.
///
/// The pinned engine (`run_skill_shell_lines`) is line-based — fenced
/// code blocks count — and runs a line iff, with `t = line.trim_start()`:
/// `t` starts with `!`, the rest trimmed starts with a backtick, and it
/// ends with a backtick around a non-empty command. This mirrors that
/// predicate exactly; a drift on either side is a silent gap.
pub fn shell_lines(body: &str) -> Vec<usize> {
    body.lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let is_command = line
                .trim_start()
                .strip_prefix('!')
                .and_then(|rest| rest.trim().strip_prefix('`'))
                .and_then(|rest| rest.strip_suffix('`'))
                .is_some_and(|command| !command.trim().is_empty());
            is_command.then_some(i + 1)
        })
        .collect()
}

/// Why a skill with shell lines is refused, in every surface's words.
pub fn shell_lines_reason(name: &str, lines: &[usize]) -> String {
    let lines = lines
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "skill \"{name}\" runs shell commands without approval (lines {lines}): remove the !`…` lines"
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{shell_lines, shell_lines_reason};

    #[test]
    fn a_shell_line_is_a_command_with_or_without_a_space() {
        assert_eq!(shell_lines("intro\n!`git status`\n"), [2]);
        assert_eq!(shell_lines("intro\n! `git status`\n"), [2]);
        assert_eq!(shell_lines("intro\n  !\t`git status`  \n"), [2]);
        // Fenced blocks count: the engine is line-based.
        assert_eq!(shell_lines("text\n```md\n!`git status`\n```\n"), [3]);
        // Every offending line, in order.
        assert_eq!(shell_lines("!`a`\n! `b`\n!`c`\n"), [1, 2, 3]);
    }

    #[test]
    fn everything_else_is_text() {
        assert!(shell_lines("!note\n").is_empty());
        assert!(
            shell_lines("!`git status\n").is_empty(),
            "no closing backtick"
        );
        assert!(shell_lines("!``\n").is_empty(), "empty command");
        assert!(shell_lines("!`   `\n").is_empty(), "blank command");
        assert!(shell_lines("`git status`\n").is_empty(), "no bang");
        assert!(
            shell_lines("run !`git status` now\n").is_empty(),
            "trailing text"
        );
        assert!(shell_lines("").is_empty());
    }

    #[test]
    fn reason_names_the_skill_and_the_lines() {
        assert_eq!(
            shell_lines_reason("deploy", &[12, 40]),
            "skill \"deploy\" runs shell commands without approval (lines 12, 40): remove the !`…` lines"
        );
    }
}
