//! Tab completion: `/word` completes command names/aliases; `@word`
//! completes paths relative to the workspace; the word after `/model`
//! completes vendor and model selectors from a table built once at
//! startup (≤ 10 candidates shown, a single match is inserted).

use ka_agent_port::registry::ModelsListing;

/// One completion: the replacement text and a human hint.
pub struct Completion {
    pub insert: String,
    pub hint: String,
}

/// One vendor's slice of the `/model` completion table.
struct VendorEntry {
    /// ka vendor name (the selector prefix).
    name: String,
    /// The vendor row's hint: the provider note, else `(N models)`.
    hint: String,
    /// Full selectors (recommended first) with per-model hints.
    models: Vec<(String, String)>,
}

/// The `/model <partial>` completion table: every vendor in display
/// order with its full selectors, recommended first — the same source
/// the `/model` picker lists. Built once where the Terminal is
/// constructed; empty when there is nothing to offer, and `/model `
/// then completes nothing beyond commands.
#[derive(Default)]
pub struct Models(Vec<VendorEntry>);

impl Models {
    /// From the models.dev registry cache under `root` (no fetch):
    /// catalog vendors in display order, each with its
    /// `registry::merged_preferred` selectors.
    pub fn from_registry_cache(root: &std::path::Path) -> Self {
        let reg = ka_agent_port::registry::read_cache(root)
            .map(|(body, at)| ka_agent_port::registry::snapshot(&body, at, true));
        Self::from_listing(ka_agent_port::registry::listing(reg.as_ref(), None))
    }

    fn from_listing(listing: ModelsListing) -> Self {
        Self(
            listing
                .providers
                .into_iter()
                .map(|p| {
                    let recommended = ka_agent_port::catalog::default_model(&p.name);
                    let models: Vec<(String, String)> = p
                        .models
                        .into_iter()
                        .map(|selector| {
                            let hint = if Some(selector.as_str()) == recommended {
                                "recommended".to_string()
                            } else {
                                String::new()
                            };
                            (selector, hint)
                        })
                        .collect();
                    VendorEntry {
                        hint: if p.note.is_empty() {
                            format!("({} models)", models.len())
                        } else {
                            p.note
                        },
                        name: p.name,
                        models,
                    }
                })
                .collect(),
        )
    }

    /// The word after `/model`: no `/` in the partial → the matching
    /// vendors (inserting `vendor/`); `vendor/…` → that vendor's
    /// matching full selectors. Display order kept, ≤ 10 candidates.
    fn complete_arg(&self, partial: &str) -> Vec<Completion> {
        let mut out = Vec::new();
        if let Some((vendor, _)) = partial.split_once('/') {
            out.extend(
                self.0
                    .iter()
                    .filter(|v| v.name == vendor)
                    .flat_map(|v| v.models.iter())
                    .filter(|(selector, _)| selector.starts_with(partial))
                    .map(|(selector, hint)| Completion {
                        insert: selector.clone(),
                        hint: hint.clone(),
                    }),
            );
        } else {
            out.extend(
                self.0
                    .iter()
                    .filter(|v| v.name.starts_with(partial))
                    .map(|v| Completion {
                        insert: format!("{}/", v.name),
                        hint: v.hint.clone(),
                    }),
            );
        }
        out.truncate(10);
        out
    }
}

/// Complete the word under the cursor.
pub fn complete(input: &str, workspace: &std::path::Path, models: &Models) -> Vec<Completion> {
    let word_start = input.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    let word = &input[word_start..];
    // `/model <partial>`: the command's first argument completes
    // selectors instead of paths; the bare command word still completes
    // as a command below.
    if input[..word_start].split_whitespace().eq(["/model"]) {
        return models.complete_arg(word);
    }
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
        let empty = Models::default();

        let cmds = complete("/sta", &ws, &empty);
        assert!(
            cmds.iter().any(|c| c.insert == "/status"),
            "{:?}",
            cmds.iter().map(|c| &c.insert).collect::<Vec<_>>()
        );
        // Aliases complete too.
        let alias = complete("/q", &ws, &empty);
        assert!(
            alias.iter().any(|c| c.insert == "/quit"),
            "{:?}",
            alias.len()
        );

        let files = complete("@RE", &ws, &empty);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].insert, "@README.md");

        let dirs = complete("@sr", &ws, &empty);
        assert_eq!(dirs[0].insert, "@src/");

        assert!(complete("plain text", &ws, &empty).is_empty());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn completes_model_selectors() {
        let ws = std::env::temp_dir().join(format!("ka-comp-model-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        // No registry cache under `ws`: the table falls back to the
        // catalog, exactly like the picker's listing.
        let models = Models::from_registry_cache(&ws);

        // `/model ` offers vendors in catalog display order.
        let vendors = complete("/model ", &ws, &models);
        assert_eq!(vendors[0].insert, "anthropic/");
        assert!(vendors.iter().all(|c| c.insert.ends_with('/')));

        // `vendor/` offers that vendor's full selectors, the
        // recommended one first and flagged.
        let anthropic = complete("/model anthropic/", &ws, &models);
        assert_eq!(anthropic[0].insert, "anthropic/claude-sonnet-5");
        assert_eq!(anthropic[0].hint, "recommended");
        assert!(anthropic.iter().all(|c| c.insert.starts_with("anthropic/")));

        // Partials filter by prefix, on both halves of the selector.
        let filtered = complete("/model an", &ws, &models);
        assert!(filtered.iter().all(|c| c.insert.starts_with("an")));
        assert!(filtered.iter().any(|c| c.insert == "anthropic/"));
        let ids = complete("/model anthropic/claude-s", &ws, &models);
        assert!(
            ids.iter()
                .all(|c| c.insert.starts_with("anthropic/claude-s"))
        );

        // An empty table offers nothing after `/model `.
        assert!(complete("/model ", &ws, &Models::default()).is_empty());

        // Only the command's first argument completes; everything else
        // keeps today's behavior (the bare command completes as one).
        assert!(complete("/model a b", &ws, &models).is_empty());
        assert!(complete("/memory an", &ws, &models).is_empty());
        let cmd = complete("/model", &ws, &models);
        assert!(cmd.iter().any(|c| c.insert == "/model"));
        let _ = std::fs::remove_dir_all(&ws);
    }
}
