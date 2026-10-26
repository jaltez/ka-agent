//! `notes_*`: the operator's local markdown notes under `<data>/notes`.

use serde_json::{Value, json};

use super::{ToolCtx, ToolFuture};

fn notes_dir(ctx: &ToolCtx) -> Result<std::path::PathBuf, String> {
    let dir = ctx.paths.root().join("notes");
    std::fs::create_dir_all(&dir).map_err(|e| format!("notes dir: {e}"))?;
    Ok(dir)
}

fn sanitize_note_name(name: &str) -> Result<String, String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "bad note name {name:?}: [a-z0-9-] only, max 64 chars"
        ));
    }
    Ok(name.to_string())
}

pub(crate) fn search_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "Search text (case-insensitive substring match)"}
        },
        "required": ["query"]
    })
}

pub(crate) fn search<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let query = args["query"].as_str().ok_or("missing 'query'")?;
        let dir = notes_dir(ctx)?;
        let lower = query.to_lowercase();
        let mut hits = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "md")
                    && let Ok(content) = std::fs::read_to_string(&path)
                {
                    for (i, line) in content.lines().enumerate() {
                        if line.to_lowercase().contains(&lower) {
                            let fname = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
                            hits.push(format!("{fname}:{i}: {line}"));
                            if hits.len() >= 20 {
                                break;
                            }
                        }
                    }
                }
                if hits.len() >= 20 {
                    break;
                }
            }
        }
        if hits.is_empty() {
            Ok(format!("no matches for {query:?}"))
        } else {
            Ok(hits.join("\n"))
        }
    })
}

pub(crate) fn append_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "file": {"type": "string", "description": "Note file name without extension (e.g. 'ideas', 'reading-list')"},
            "text": {"type": "string", "description": "The line to append"}
        },
        "required": ["file", "text"]
    })
}

pub(crate) fn append<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let file = args["file"].as_str().ok_or("missing 'file'")?;
        let text = args["text"].as_str().ok_or("missing 'text'")?;
        let name = sanitize_note_name(file)?;
        let dir = notes_dir(ctx)?;
        let path = dir.join(format!("{name}.md"));
        let mut content = std::fs::read_to_string(&path).unwrap_or_default();
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(text);
        content.push('\n');
        std::fs::write(&path, content).map_err(|e| format!("write: {e}"))?;
        Ok(format!("appended to {name}.md"))
    })
}

pub(crate) fn read_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "file": {"type": "string", "description": "Note file name without extension"}
        },
        "required": ["file"]
    })
}

pub(crate) fn read<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let file = args["file"].as_str().ok_or("missing 'file'")?;
        let name = sanitize_note_name(file)?;
        let dir = notes_dir(ctx)?;
        let path = dir.join(format!("{name}.md"));
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                if content.len() > 8000 {
                    Ok(format!(
                        "{}… (truncated)",
                        ka_agent_core::truncate_at_char(&content, 8000)
                    ))
                } else {
                    Ok(content)
                }
            }
            Err(_) => Ok(format!("{name}.md is empty or does not exist")),
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn note_name_validation() {
        assert!(sanitize_note_name("ideas").is_ok());
        assert!(sanitize_note_name("reading-list").is_ok());
        assert!(sanitize_note_name("").is_err());
        assert!(sanitize_note_name("UPPER").is_err());
        assert!(sanitize_note_name("../etc").is_err());
        assert!(sanitize_note_name("a/b").is_err());
        assert!(sanitize_note_name(&"x".repeat(65)).is_err());
    }
}
