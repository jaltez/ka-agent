//! Tab completion: `/word` completes command names/aliases; `@word`
//! completes paths relative to the workspace (≤ 10 candidates shown, a
//! single match is inserted).

/// One completion: the replacement text and a human hint.
pub struct Completion {
    pub insert: String,
    pub hint: String,
}

/// Complete the word under the cursor.
pub fn complete(input: &str, workspace: &std::path::Path) -> Vec<Completion> {
    let word_start = input.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    let word = &input[word_start..];
    if let Some(stem) = word.strip_prefix('/') {
        let stem = stem.split_whitespace().next().unwrap_or("");
        let mut out = Vec::new();
        for spec in ka_agent_channels::commands::COMMANDS {
            let mut names = vec![spec.name];
            names.extend(spec.aliases.iter().copied());
            for name in names {
                if name.starts_with(stem) {
                    out.push(Completion {
                        insert: format!("/{name}"),
                        hint: spec.summary.to_string(),
                    });
                }
            }
        }
        out.sort_by(|a, b| a.insert.cmp(&b.insert));
        out.dedup_by(|a, b| a.insert == b.insert);
        return out;
    }
    if let Some(stem) = word.strip_prefix('@') {
        let (dir_part, file_stem) = match stem.rfind('/') {
            Some(i) => (&stem[..=i], &stem[i + 1..]),
            None => ("", stem),
        };
        let base = if dir_part.is_empty() {
            workspace.to_path_buf()
        } else {
            workspace.join(dir_part)
        };
        let Ok(entries) = std::fs::read_dir(&base) else {
            return Vec::new();
        };
        let mut out: Vec<Completion> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if !name.starts_with(file_stem) || name.starts_with('.') {
                    return None;
                }
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                Some(Completion {
                    insert: format!("@{dir_part}{name}{}", if is_dir { "/" } else { "" }),
                    hint: if is_dir {
                        "dir".to_string()
                    } else {
                        String::new()
                    },
                })
            })
            .collect();
        out.sort_by(|a, b| a.insert.cmp(&b.insert));
        out.truncate(10);
        return out;
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn completes_commands_and_paths() {
        let ws = std::env::temp_dir().join(format!("ka-comp-{}", std::process::id()));
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(ws.join("README.md"), "x").unwrap();
        std::fs::write(ws.join("report.txt"), "x").unwrap();

        let cmds = complete("/sta", &ws);
        assert!(
            cmds.iter().any(|c| c.insert == "/status"),
            "{:?}",
            cmds.iter().map(|c| &c.insert).collect::<Vec<_>>()
        );
        // Aliases complete too.
        let alias = complete("/q", &ws);
        assert!(
            alias.iter().any(|c| c.insert == "/quit"),
            "{:?}",
            alias.len()
        );

        let files = complete("@RE", &ws);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].insert, "@README.md");

        let dirs = complete("@sr", &ws);
        assert_eq!(dirs[0].insert, "@src/");

        assert!(complete("plain text", &ws).is_empty());
        let _ = std::fs::remove_dir_all(&ws);
    }
}
