//! The persona: free-form instructions to the assistant (identity, tone,
//! house rules). First line `# <Name>`, then the body. Compiled into the
//! engine's `IDENTITY.md` (re-read per turn) — never into `MEMORY.md`,
//! which is for facts about the operator.

use super::{Error, MemoryStore};

/// The default persona, written on first open and on `ka-agent persona
/// reset` (reset keeps the current name: only the heading is swapped).
pub const DEFAULT_PERSONA: &str = "\
# Ka

The ka is the vital spark that stays with a person. Be that for one person: steady, useful, a little wry.

- Lead with the answer; add detail only when it earns its place.
- Warm, never gushing: no flattery, no \"Great question!\", no apologizing for existing.
- Dry wit when it lands — never at the operator's expense, never in bad news.
- Have opinions: recommend one option and say why; disagree plainly when the facts say so.
- Say \"I don't know\" and \"I got that wrong\" without ceremony. Never claim an action you did not take.
- Emoji only after the operator uses them.
- Remember what matters to them, ask when unsure, keep their secrets.
";

/// Hard cap on the persona file (bytes).
const MAX_PERSONA_BYTES: usize = 4096;

/// The persona's display name: its first `# ` heading, trimmed, at most 32
/// characters; `Ka` when absent or empty.
pub fn persona_name(persona: &str) -> String {
    let heading = persona
        .lines()
        .find_map(|l| l.strip_prefix("# "))
        .map(str::trim)
        .unwrap_or("");
    let mut name: String = heading.chars().take(32).collect();
    if name.trim().is_empty() {
        name = "Ka".to_string();
    }
    name
}

/// The persona body: the text without its leading `# <Name>` heading
/// (trailing whitespace trimmed).
pub fn persona_body(persona: &str) -> String {
    let mut lines = persona.lines().peekable();
    // Skip the heading (and anything before it is nothing, by contract).
    if lines.peek().is_some_and(|l| l.starts_with("# ")) {
        lines.next();
    }
    lines.collect::<Vec<_>>().join("\n").trim().to_string()
}

/// Reject invisible or direction-changing characters (the same set
/// [`screen_proposal`](super::screen_proposal) refuses): a persona is read
/// by the operator and compiled into every system prompt.
fn check_visible(text: &str) -> Result<(), Error> {
    const INVISIBLE: &[char] = &[
        '\u{200B}', '\u{200C}', '\u{200D}', '\u{2060}', '\u{FEFF}', '\u{00AD}',
    ];
    if let Some(c) = text.chars().find(|c| {
        INVISIBLE.contains(c)
            || ('\u{202A}'..='\u{202E}').contains(c)
            || ('\u{2066}'..='\u{2069}').contains(c)
            || (c.is_control() && *c != '\n')
    }) {
        return Err(Error::Persona(format!(
            "contains an invisible or direction-changing character (U+{:04X})",
            c as u32
        )));
    }
    Ok(())
}

impl MemoryStore {
    /// The path of the persona file.
    pub fn persona_path(&self) -> std::path::PathBuf {
        self.root().join("persona.md")
    }

    /// The persona text. A missing file is written with
    /// [`DEFAULT_PERSONA`] (commit `persona: default`) and returned.
    pub fn persona_text(&self) -> String {
        let path = self.persona_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => {
                if std::fs::write(&path, DEFAULT_PERSONA).is_ok() {
                    let _ = self.commit("persona: default");
                }
                DEFAULT_PERSONA.to_string()
            }
        }
    }

    /// The persona's display name ([`persona_name`] of its text).
    pub fn persona_name(&self) -> String {
        persona_name(&self.persona_text())
    }

    /// Write the persona: at most 4096 bytes, no invisible or
    /// direction-changing characters. Commit `persona: edit`.
    pub fn write_persona(&self, text: &str) -> Result<(), Error> {
        let text = text.trim_end_matches('\n');
        if text.len() > MAX_PERSONA_BYTES {
            return Err(Error::Persona(format!(
                "too large ({} bytes; the cap is {MAX_PERSONA_BYTES})",
                text.len()
            )));
        }
        check_visible(text)?;
        if !text.starts_with("# ") {
            return Err(Error::Persona(
                "the first line must be a `# <Name>` heading".to_string(),
            ));
        }
        std::fs::write(self.persona_path(), format!("{text}\n"))?;
        self.commit("persona: edit")?;
        Ok(())
    }

    /// One-time migration from the persona-as-memory-tier era: a file that
    /// is only `# Persona` becomes [`DEFAULT_PERSONA`] (commit
    /// `persona: default`); a file with entries keeps them as free-form
    /// body under the default persona, with its `# Persona` heading
    /// removed and provenance comments stripped (commit
    /// `persona: migrate`). A missing file gets the default. Anything
    /// already migrated (any other first heading) is left alone.
    pub(super) fn migrate_persona(&self) -> Result<(), Error> {
        let path = self.persona_path();
        let Ok(old) = std::fs::read_to_string(&path) else {
            std::fs::write(&path, DEFAULT_PERSONA)?;
            self.commit("persona: default")?;
            return Ok(());
        };
        if !old.starts_with("# Persona") {
            return Ok(()); // fresh tree (default written via persona_text) or already migrated
        }
        // Body: drop the heading, drop `<!-- … -->` provenance comments.
        let body: Vec<&str> = old
            .lines()
            .skip_while(|l| l.starts_with("# Persona"))
            .filter(|l| !(l.trim_start().starts_with("<!--") && l.trim_end().ends_with("-->")))
            .collect();
        if body.iter().all(|l| l.trim().is_empty()) {
            std::fs::write(&path, DEFAULT_PERSONA)?;
            self.commit("persona: default")?;
            return Ok(());
        }
        let migrated = format!("{DEFAULT_PERSONA}\n{}\n", body.join("\n").trim_end());
        std::fs::write(&path, migrated)?;
        self.commit("persona: migrate")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn store(tag: &str) -> (MemoryStore, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("ka-persona-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        (MemoryStore::open(&root).unwrap(), root)
    }

    #[test]
    fn missing_persona_gets_the_default_and_a_commit() {
        let (s, root) = store("missing");
        assert_eq!(s.persona_text(), DEFAULT_PERSONA);
        assert_eq!(s.persona_name(), "Ka");
        let log = MemoryStore::git(&root, &["log", "--format=%s"]).unwrap();
        assert!(log.contains("persona: default"), "{log}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn name_comes_from_the_first_heading_and_caps_at_32_chars() {
        assert_eq!(persona_name("# Ka\nbody"), "Ka");
        assert_eq!(persona_name("# María la Curiosa\nbody"), "María la Curiosa");
        let long: String = "x".repeat(50);
        assert_eq!(
            persona_name(&format!("# {long}\n")),
            "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
        );
        assert_eq!(persona_name("no heading at all"), "Ka");
        assert_eq!(persona_name("#   \nbody"), "Ka");
    }

    #[test]
    fn write_persona_rejects_invisible_chars_and_oversize() {
        let (s, root) = store("write");
        let err = s.write_persona("# Ka\nhas zero\u{200B}width").unwrap_err();
        assert!(err.to_string().contains("invisible"), "{err}");
        let err = s
            .write_persona("# Ka\nhas bidi\u{202E}override")
            .unwrap_err();
        assert!(err.to_string().contains("invisible"), "{err}");
        let big = format!("# Ka\n{}", "x".repeat(5000));
        let err = s.write_persona(&big).unwrap_err();
        assert!(err.to_string().contains("4096"), "{err}");
        // Valid write commits.
        s.write_persona("# Eve\nsharp, brief").unwrap();
        assert_eq!(s.persona_name(), "Eve");
        let log = MemoryStore::git(&root, &["log", "--format=%s"]).unwrap();
        assert!(log.contains("persona: edit"), "{log}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn header_only_legacy_persona_becomes_the_default() {
        let root = std::env::temp_dir().join(format!("ka-persona-hdr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("persona.md"), "# Persona\n\n   \n").unwrap();
        let _s = MemoryStore::open(&root).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("persona.md")).unwrap(),
            DEFAULT_PERSONA
        );
        let log = MemoryStore::git(&root, &["log", "--format=%s"]).unwrap();
        assert!(log.contains("persona: default"), "{log}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn legacy_persona_entries_survive_as_freeform_body() {
        let root = std::env::temp_dir().join(format!("ka-persona-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("persona.md"),
            "# Persona\n- prefers Spanish\n<!-- id: mem_1; source: operator; accepted: 2026-01-01T00:00:00Z; valid_until: - -->\n",
        )
        .unwrap();
        let s = MemoryStore::open(&root).unwrap();
        let text = std::fs::read_to_string(root.join("persona.md")).unwrap();
        assert!(text.starts_with("# Ka\n"), "{text}");
        assert!(text.contains("The ka is the vital spark"), "{text}");
        assert!(text.ends_with("- prefers Spanish\n"), "{text}");
        assert!(!text.contains("mem_1"), "{text}");
        assert!(!text.contains("<!--"), "{text}");
        assert_eq!(s.persona_name(), "Ka");
        let log = MemoryStore::git(&root, &["log", "--format=%s"]).unwrap();
        assert!(log.contains("persona: migrate"), "{log}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn persona_never_enters_compiled_memory() {
        let (s, root) = store("compile");
        s.write_persona("# Eve\nsecret-persona-marker").unwrap();
        let compiled = s.compiled_text(1024).unwrap();
        assert!(!compiled.contains("secret-persona-marker"), "{compiled}");
        assert!(!compiled.contains("## Persona"), "{compiled}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn persona_body_strips_the_heading() {
        assert_eq!(
            persona_body("# Ka\n\nline one\nline two\n"),
            "line one\nline two"
        );
        assert_eq!(persona_body("no heading"), "no heading");
    }
}
