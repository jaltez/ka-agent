//! Plugin manifest v1 (SEED.md §6.2). Strict TOML: unknown keys are errors,
//! mirroring `ka-agent.toml`. The manifest is the plugin's whole contract —
//! tools, effects, scopes, and what the sandbox must provide. Installing
//! grants nothing: every scope is a separate operator grant (§6.3).

use serde::{Deserialize, Serialize};
use std::path::Path;

/// API major this host implements. The host supports `api = N` and `N-1`
/// (SEED.md §6.5).
pub const PLUGIN_API: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub plugin: PluginSection,
    #[serde(default)]
    pub requires: RequiresSection,
    #[serde(default)]
    pub tools: Vec<ToolSection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSection {
    /// Unique, `[a-z0-9-]+`.
    pub id: String,
    /// Semver of the plugin itself.
    pub version: String,
    /// Plugin API major this targets.
    pub api: u8,
    /// `tool` | `connector` | `skill-pack`.
    #[serde(default = "default_kind")]
    pub kind: String,
    pub description: String,
    pub entry: EntrySection,
}

fn default_kind() -> String {
    "tool".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntrySection {
    /// `mcp-stdio` is the only v1 execution type.
    #[serde(rename = "type")]
    pub entry_type: String,
    /// Command to run inside the sandbox, resolved against the plugin root
    /// when it starts with `/plugin/`.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RequiresSection {
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Egress destinations. **v1 has no plugin egress** (§6.4 note): the
    /// sandbox unshares the network namespace, so a non-empty list is
    /// rejected at validation time — declaring hosts grants nothing yet.
    #[serde(default)]
    pub network: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSection {
    /// Must start with `<plugin-id>.`.
    pub name: String,
    /// `read` | `local_write` | `external_write`. `exec` and `irreversible`
    /// are host effects third-party plugins cannot declare in v1 (they need
    /// an ask round-trip the bridge does not own).
    pub effect: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("read {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("parse: {0}")]
    Parse(String),
    #[error("invalid: {0}")]
    Invalid(String),
}

impl Manifest {
    /// Parse and validate manifest text.
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let m: Manifest = toml::from_str(text).map_err(|e| ManifestError::Parse(e.to_string()))?;
        m.validate()?;
        Ok(m)
    }

    /// Parse and validate the manifest at `path`.
    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        let text = std::fs::read_to_string(path).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text)
    }

    /// Invariants that make a plugin loadable. Every rejection carries the
    /// reason the operator will see in `plugin list`.
    pub fn validate(&self) -> Result<(), ManifestError> {
        let id = &self.plugin.id;
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            || id.starts_with('-')
            || id.ends_with('-')
            || id.contains("--")
        {
            return Err(ManifestError::Invalid(format!(
                "plugin.id {id:?}: [a-z0-9-] segments, no leading/trailing/doubled '-'"
            )));
        }
        if self.plugin.api != PLUGIN_API {
            return Err(ManifestError::Invalid(format!(
                "api {}: this host supports {PLUGIN_API}",
                self.plugin.api
            )));
        }
        if !matches!(
            self.plugin.kind.as_str(),
            "tool" | "connector" | "skill-pack"
        ) {
            return Err(ManifestError::Invalid(format!(
                "kind {:?}: tool | connector | skill-pack",
                self.plugin.kind
            )));
        }
        if semver_is_invalid(&self.plugin.version) {
            return Err(ManifestError::Invalid(format!(
                "version {:?}: expected MAJOR.MINOR.PATCH",
                self.plugin.version
            )));
        }
        if self.plugin.entry.entry_type != "mcp-stdio" {
            return Err(ManifestError::Invalid(format!(
                "entry.type {:?}: mcp-stdio is the only v1 type",
                self.plugin.entry.entry_type
            )));
        }
        if self.plugin.entry.command.trim().is_empty() {
            return Err(ManifestError::Invalid("entry.command is empty".into()));
        }
        for secret in &self.requires.secrets {
            if !valid_name(secret) {
                return Err(ManifestError::Invalid(format!(
                    "requires.secrets {secret:?}: [a-z0-9_.-] only"
                )));
            }
        }
        if !self.requires.network.is_empty() {
            return Err(ManifestError::Invalid(format!(
                "requires.network {:?}: plugins have no egress in this host version \
                 (the sandbox unshares the network); ask the operator to use a \
                 first-party connector instead",
                self.requires.network
            )));
        }
        if self.tools.is_empty() {
            return Err(ManifestError::Invalid(
                "no [[tools]]: a plugin must declare at least one tool".into(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for tool in &self.tools {
            let prefix = format!("{id}.");
            if !tool.name.starts_with(&prefix) {
                return Err(ManifestError::Invalid(format!(
                    "tool {:?}: must start with {prefix:?}",
                    tool.name
                )));
            }
            if tool.name.len() > prefix.len() && !valid_tool_tail(&tool.name[prefix.len()..]) {
                return Err(ManifestError::Invalid(format!(
                    "tool {:?}: [a-z0-9_.] after the plugin prefix",
                    tool.name
                )));
            }
            if !seen.insert(tool.name.clone()) {
                return Err(ManifestError::Invalid(format!(
                    "duplicate tool {:?}",
                    tool.name
                )));
            }
            match tool.effect.as_str() {
                "read" => {}
                "local_write" | "external_write" => {}
                other => {
                    return Err(ManifestError::Invalid(format!(
                        "tool {:?}: effect {other:?} — v1 plugins may declare \
                         read | local_write | external_write",
                        tool.name
                    )));
                }
            }
            for scope in &tool.scopes {
                if !valid_name(scope) {
                    return Err(ManifestError::Invalid(format!(
                        "tool {:?} scope {scope:?}: [a-z0-9_:-] only",
                        tool.name
                    )));
                }
            }
        }
        Ok(())
    }

    /// The tool declaration with this exact name, if any.
    pub fn tool(&self, name: &str) -> Option<&ToolSection> {
        self.tools.iter().find(|t| t.name == name)
    }
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.' | ':')
        })
}

fn valid_tool_tail(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
}

/// `MAJOR.MINOR.PATCH` with numeric parts only (no pre-release in v1).
fn semver_is_invalid(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const VALID: &str = r#"
[plugin]
id = "probe"
version = "0.1.0"
api = 1
kind = "tool"
description = "Sandbox probe"
entry = { type = "mcp-stdio", command = "/plugin/bin/probe", args = [] }

[[tools]]
name = "probe.env"
effect = "read"
scopes = []
"#;

    #[test]
    fn parses_the_documented_shape() {
        let m = Manifest::parse(VALID).unwrap();
        assert_eq!(m.plugin.id, "probe");
        assert_eq!(m.tools.len(), 1);
        assert!(m.tool("probe.env").is_some());
    }

    #[test]
    fn rejects_unknown_keys() {
        let bad = VALID.replace("[plugin]", "[plugin]\nbanana = 1");
        let err = Manifest::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("banana"), "{err}");
    }

    #[test]
    fn rejects_unsupported_api_and_bad_ids() {
        let err = Manifest::parse(&VALID.replace("api = 1", "api = 2"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("api 2"), "{err}");
        for id in ["Probe", "pro_be", "-probe", "probe-", "pro--be", ""] {
            let bad = VALID.replace("id = \"probe\"", &format!("id = \"{id}\""));
            assert!(Manifest::parse(&bad).is_err(), "id {id:?} passed");
        }
    }

    #[test]
    fn rejects_any_declared_egress_in_v1() {
        let bad = VALID.replace(
            "[[tools]]",
            "[requires]\nnetwork = [\"api.example.com:443\"]\n\n[[tools]]",
        );
        let err = Manifest::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("no egress"), "{err}");
    }

    #[test]
    fn tool_names_must_carry_the_plugin_prefix_and_legal_effects() {
        let bad = VALID.replace("\"probe.env\"", "\"env\"");
        assert!(Manifest::parse(&bad).is_err());
        for effect in ["exec", "irreversible", "write"] {
            let bad = VALID.replace("effect = \"read\"", &format!("effect = \"{effect}\""));
            let err = Manifest::parse(&bad).unwrap_err().to_string();
            assert!(err.contains("v1 plugins"), "{err}");
        }
    }

    #[test]
    fn rejects_duplicate_tools_and_bad_semver() {
        let dup = format!("{VALID}\n\n[[tools]]\nname = \"probe.env\"\neffect = \"read\"\n");
        assert!(Manifest::parse(&dup).is_err());
        for v in ["1.2", "1.2.3-rc1", "v1.2.3", "1.2.x"] {
            let bad = VALID.replace("version = \"0.1.0\"", &format!("version = \"{v}\""));
            assert!(Manifest::parse(&bad).is_err(), "version {v:?} passed");
        }
    }

    #[test]
    fn rejects_tools_free_plugins() {
        let bad = VALID.replace(
            "\n[[tools]]\nname = \"probe.env\"\neffect = \"read\"\nscopes = []\n",
            "\n",
        );
        assert!(Manifest::parse(&bad).is_err());
    }
}
