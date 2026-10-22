//! Format-preserving edits of `ka-agent.toml` (`ka-agent config set`,
//! `/model`, setup). Built on `toml_edit` so the operator's comments and key
//! order survive; every write is validated as a whole [`DaemonConfig`]
//! before it touches disk and lands atomically (temp file + rename).

use std::path::Path;

use toml_edit::{DocumentMut, Item, TableLike, Value};

use crate::config::{DaemonConfig, Error};

const ARRAYS_MSG: &str = "only flat arrays can be set (`[\"a\", \"b\"]`); arrays of tables are edited with `ka-agent config edit`";
const TABLES_MSG: &str = "tables are edited with `ka-agent config edit`";

/// Read the config as an editable document; a missing file is an empty
/// document.
pub fn read_doc(path: &Path) -> Result<DocumentMut, Error> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(Error::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    text.parse::<DocumentMut>().map_err(|e| Error::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })
}

/// Set a dotted `key` (e.g. `budgets.daily_usd`) to `value`, creating
/// intermediate tables. Typing: `true|false` → bool, `-?[0-9][0-9_]*` →
/// integer, `-?[0-9]+.[0-9]+` → float, `"…"` → the inner string, anything
/// else → string. Arrays and tables are not settable. Returns the validated
/// new config; on any error the file is untouched.
pub fn set(path: &Path, key: &str, value: &str) -> Result<DaemonConfig, Error> {
    let segments = split_key(key)?;
    let mut doc = read_doc(path)?;
    let mut new = typed_value(value)?;
    let (last, parents) = segments
        .split_last()
        .ok_or_else(|| Error::Edit(format!("invalid key {key:?}")))?;
    let mut table: &mut dyn TableLike = doc.as_table_mut();
    for (i, seg) in parents.iter().enumerate() {
        let item = table.entry(seg).or_insert(toml_edit::table());
        table = item
            .as_table_like_mut()
            .ok_or_else(|| Error::Edit(format!("{} is not a table", segments[..=i].join("."))))?;
    }
    match table.get_mut(last) {
        Some(Item::Value(old)) => {
            // An array is replaced only by an array: `allow_user_ids = 2`
            // is almost certainly a mistake for `[2]`.
            if old.is_array() != new.is_array() {
                return Err(Error::Edit(ARRAYS_MSG.into()));
            }
            if old.is_inline_table() {
                return Err(Error::Edit(TABLES_MSG.into()));
            }
            // Keep the spacing and any trailing comment of the old value.
            *new.decor_mut() = old.decor().clone();
            *old = new;
        }
        Some(Item::ArrayOfTables(_)) => return Err(Error::Edit(ARRAYS_MSG.into())),
        Some(Item::Table(_)) => return Err(Error::Edit(TABLES_MSG.into())),
        Some(Item::None) | None => {
            table.insert(last, Item::Value(new));
        }
    }
    write_doc(path, &doc)
}

/// Remove a dotted `key`. A key that is not set is a no-op (nothing is
/// written). Returns the validated resulting config.
pub fn unset(path: &Path, key: &str) -> Result<DaemonConfig, Error> {
    let segments = split_key(key)?;
    let mut doc = read_doc(path)?;
    let removed = match segments.split_last() {
        Some((last, parents)) => {
            let mut table: Option<&mut dyn TableLike> = Some(doc.as_table_mut());
            for seg in parents {
                table = table
                    .and_then(|t| t.get_mut(seg))
                    .and_then(Item::as_table_like_mut);
            }
            table.and_then(|t| t.remove(last)).is_some()
        }
        None => false,
    };
    if removed {
        write_doc(path, &doc)
    } else {
        parse_validate(path, &doc)
    }
}

/// The value at a dotted `key`, rendered: strings as their raw contents
/// (no quotes), other values as TOML (`2.5`, `true`, `[1, 2]`), tables as
/// their TOML body. `None` when the key is not set in the file (the
/// built-in default applies).
pub fn get(path: &Path, key: &str) -> Result<Option<String>, Error> {
    let segments = split_key(key)?;
    let doc = read_doc(path)?;
    let mut item: Option<&Item> = Some(doc.as_item());
    for seg in &segments {
        item = item.and_then(Item::as_table_like).and_then(|t| t.get(seg));
    }
    Ok(match item {
        None | Some(Item::None) => None,
        Some(Item::Value(v)) => Some(render_value(v)),
        Some(other) => Some(other.to_string().trim().to_string()),
    })
}

/// Validate `doc` as a full config and atomically replace `path` with it
/// (parent dirs created, `path.tmp` + rename, mode 0600 on Unix). On a
/// validation error nothing is written.
pub fn write_doc(path: &Path, doc: &DocumentMut) -> Result<DaemonConfig, Error> {
    let cfg = parse_validate(path, doc)?;
    let text = doc.to_string();
    let write_err = |e: std::io::Error| Error::Edit(format!("write {}: {e}", path.display()));
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(write_err)?;
    }
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp_name);
    let result = write_private(&tmp, text.as_bytes()).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(write_err(e));
    }
    Ok(cfg)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    // A leftover temp file keeps its old mode; force 0600 regardless.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.sync_all()
}

fn parse_validate(path: &Path, doc: &DocumentMut) -> Result<DaemonConfig, Error> {
    let cfg: DaemonConfig = toml::from_str(&doc.to_string()).map_err(|e| Error::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    cfg.validate()?;
    Ok(cfg)
}

fn split_key(key: &str) -> Result<Vec<&str>, Error> {
    let segments: Vec<&str> = key.trim().split('.').collect();
    let valid = segments.iter().all(|s| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    });
    if valid {
        Ok(segments)
    } else {
        Err(Error::Edit(format!(
            "invalid key {key:?} — use dotted names like budgets.daily_usd"
        )))
    }
}

fn typed_value(raw: &str) -> Result<Value, Error> {
    let s = raw.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        return Ok(Value::from(&s[1..s.len() - 1]));
    }
    match s {
        "true" => return Ok(Value::from(true)),
        "false" => return Ok(Value::from(false)),
        _ => {}
    }
    if s.starts_with('[') {
        // Flat arrays of scalars only (`fallback = ["a/b"]`); nested
        // arrays and inline tables stay with `config edit`.
        let v: Value = s
            .parse()
            .map_err(|e| Error::Edit(format!("array {s}: {e}")))?;
        let flat = v
            .as_array()
            .is_some_and(|a| a.iter().all(|x| !x.is_array() && !x.is_inline_table()));
        if !flat {
            return Err(Error::Edit(ARRAYS_MSG.into()));
        }
        return Ok(v);
    }
    if s.starts_with('{') {
        return Err(Error::Edit(TABLES_MSG.into()));
    }
    let digits = s.strip_prefix('-').unwrap_or(s);
    let first_digit = digits.bytes().next().is_some_and(|b| b.is_ascii_digit());
    if first_digit && digits.bytes().all(|b| b.is_ascii_digit() || b == b'_') {
        let clean: String = s.chars().filter(|c| *c != '_').collect();
        let n: i64 = clean
            .parse()
            .map_err(|_| Error::Edit(format!("integer {s} is out of range")))?;
        return Ok(Value::from(n));
    }
    if let Some((int, frac)) = digits.split_once('.') {
        let all_digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
        if all_digits(int) && all_digits(frac) {
            let f: f64 = s
                .parse()
                .map_err(|_| Error::Edit(format!("invalid number {s}")))?;
            return Ok(Value::from(f));
        }
    }
    Ok(Value::from(s))
}

fn render_value(v: &Value) -> String {
    match v.as_str() {
        Some(s) => s.to_string(),
        None => {
            let mut bare = v.clone();
            bare.decor_mut().clear();
            bare.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn tmp_config(name: &str, content: Option<&str>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ka-agent-core-cfgfile-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ka-agent.toml");
        if let Some(c) = content {
            std::fs::write(&p, c).unwrap();
        }
        p
    }

    const BASE: &str = "# keep me\nworkspace = \"/tmp/ws\" # where\nmode = \"guarded\"\n\n[budgets]\n# cap\ndaily_usd = 5.0\n";

    #[test]
    fn set_preserves_comments_and_order() {
        let p = tmp_config("comments", Some(BASE));
        let cfg = set(&p, "budgets.daily_usd", "2.5").unwrap();
        assert_eq!(cfg.budgets.daily_usd, Some(2.5));
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("# keep me"), "{text}");
        assert!(text.contains("# cap"), "{text}");
        assert!(text.contains("daily_usd = 2.5"), "{text}");
        assert!(text.find("workspace").unwrap() < text.find("mode").unwrap());
        // Trailing comment of a replaced value survives.
        set(&p, "workspace", "/tmp/other").unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("workspace = \"/tmp/other\" # where"),
            "{text}"
        );
        assert_eq!(
            get(&p, "budgets.daily_usd").unwrap().as_deref(),
            Some("2.5")
        );
    }

    #[test]
    fn invalid_value_leaves_file_untouched() {
        let p = tmp_config("invalid", Some(BASE));
        let err = set(&p, "mode", "yolo").unwrap_err();
        assert!(err.to_string().contains("unknown mode"), "{err}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), BASE);
        // Unknown keys fail strict parsing, also without writing.
        assert!(set(&p, "no_such_key", "1").is_err());
        // Type mismatch (string for an integer field).
        assert!(set(&p, "approval_ttl_secs", "soon").is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), BASE);
        assert!(!p.with_extension("toml.tmp").exists());
    }

    #[test]
    fn typed_values() {
        let p = tmp_config("typed", Some("workspace = \"/tmp/ws\"\n"));
        let cfg = set(&p, "web.enabled", "false").unwrap();
        assert!(!cfg.web.enabled);
        let cfg = set(&p, "approval_ttl_secs", "1_200").unwrap();
        assert_eq!(cfg.approval_ttl_secs, 1200);
        let cfg = set(&p, "budgets.daily_usd", "0.75").unwrap();
        assert_eq!(cfg.budgets.daily_usd, Some(0.75));
        let cfg = set(&p, "model", "\"anthropic/claude-sonnet-5\"").unwrap();
        assert_eq!(cfg.model.as_deref(), Some("anthropic/claude-sonnet-5"));
        let cfg = set(&p, "calendar.digest_time", "07:15").unwrap();
        assert_eq!(cfg.calendar.digest_time, "07:15");
        let doc = read_doc(&p).unwrap();
        assert_eq!(doc["web"]["enabled"].as_bool(), Some(false));
        // Quoting forces a string even when it looks like a bool.
        assert_eq!(typed_value("\"true\"").unwrap().as_str(), Some("true"));
        assert_eq!(typed_value("-3").unwrap().as_integer(), Some(-3));
        assert_eq!(typed_value("1.5").unwrap().as_float(), Some(1.5));
        assert_eq!(typed_value("1.5.3").unwrap().as_str(), Some("1.5.3"));
        assert_eq!(
            get(&p, "model").unwrap().as_deref(),
            Some("anthropic/claude-sonnet-5")
        );
        assert_eq!(get(&p, "web.enabled").unwrap().as_deref(), Some("false"));
        assert_eq!(
            get(&p, "approval_ttl_secs").unwrap().as_deref(),
            Some("1200")
        );
        assert_eq!(get(&p, "turn_grace_secs").unwrap(), None);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("[budgets]"), "{text}");
    }

    #[test]
    fn dotted_key_on_missing_file_creates_tables_and_parents() {
        let dir = tmp_config("fresh", None);
        let p = dir.parent().unwrap().join("nested/dir/ka-agent.toml");
        set(&p, "budgets.daily_usd", "-1").unwrap_err(); // must be positive
        assert!(!p.exists());
        let cfg = set(&p, "budgets.daily_tokens", "1000").unwrap();
        assert_eq!(cfg.budgets.daily_tokens, Some(1000));
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("[budgets]"), "{text}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn flat_arrays_are_settable_tables_and_nested_arrays_are_not() {
        let p = tmp_config(
            "arrays",
            Some("workspace = \"/tmp/ws\"\n[telegram]\nallow_user_ids = [1]\n"),
        );
        // A scalar never silently replaces an array.
        let e = set(&p, "telegram.allow_user_ids", "2").unwrap_err();
        assert!(e.to_string().contains("only flat arrays"), "{e}");
        let e = set(&p, "telegram", "x").unwrap_err();
        assert!(e.to_string().contains("tables are edited with"), "{e}");
        let e = set(&p, "fallback", "[[\"a\"]]").unwrap_err();
        assert!(e.to_string().contains("only flat arrays"), "{e}");
        // A wrong-typed array still fails schema validation.
        assert!(set(&p, "model", "[1, 2]").is_err());
        let cfg = set(&p, "fallback", "[\"openai/gpt-5.1\", \"zai/glm-4.6\"]").unwrap();
        assert_eq!(cfg.fallback, vec!["openai/gpt-5.1", "zai/glm-4.6"]);
        let cfg = set(&p, "telegram.allow_user_ids", "[1, 2]").unwrap();
        assert_eq!(cfg.telegram.allow_user_ids, vec![1, 2]);
        let e = set(&p, "workspace.inner", "1").unwrap_err();
        assert!(e.to_string().contains("workspace is not a table"), "{e}");
        assert!(set(&p, "budgets..x", "1").is_err());
        assert_eq!(
            get(&p, "telegram.allow_user_ids").unwrap().as_deref(),
            Some("[1, 2]")
        );
    }

    #[test]
    fn unset_removes_and_missing_is_noop() {
        let p = tmp_config("unset", Some(BASE));
        let cfg = unset(&p, "budgets.daily_usd").unwrap();
        assert_eq!(cfg.budgets.daily_usd, None);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains("daily_usd"), "{text}");
        assert!(text.contains("# keep me"));
        let before = std::fs::read_to_string(&p).unwrap();
        unset(&p, "model").unwrap();
        unset(&p, "web.port").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
        // Missing file: no-op, nothing created.
        let dir = tmp_config("unset-missing", None);
        unset(&dir, "model").unwrap();
        assert!(!dir.exists());
    }
}
