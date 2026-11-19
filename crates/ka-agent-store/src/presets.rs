//! `/preset` slots: one numbered model+mode+effort combo per user
//! (username-partitioned like `sessions` — another user's slot is never
//! visible). Saving composes the live pickers; applying goes through the
//! runtime setters, so model ACLs and role mode ceilings are inherited,
//! never lifted by a preset.

use super::*;

/// One saved slot.
#[derive(Debug, Clone, PartialEq)]
pub struct Preset {
    pub username: String,
    /// 1-based slot number (`/preset save <1-3>`).
    pub slot: u8,
    /// The model selector, `None` = the config's model.
    pub model: Option<String>,
    /// The permission mode name, `None` = the profile default.
    pub mode: Option<String>,
    /// The reasoning effort, `None` = the model default.
    pub effort: Option<String>,
}

impl Store {
    /// Save (overwrite) one slot for one user.
    pub fn save_preset(
        &self,
        username: &str,
        slot: u8,
        model: Option<&str>,
        mode: Option<&str>,
        effort: Option<&str>,
    ) -> Result<()> {
        self.lock().execute(
            "INSERT INTO presets(username, slot, model, mode, effort)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(username, slot) DO UPDATE SET
               model = excluded.model, mode = excluded.mode, effort = excluded.effort",
            params![username, slot as i64, model, mode, effort],
        )?;
        Ok(())
    }

    /// One slot of one user (`/preset <n>`).
    pub fn preset(&self, username: &str, slot: u8) -> Result<Option<Preset>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT username, slot, model, mode, effort FROM presets
             WHERE username = ?1 AND slot = ?2",
            params![username, slot as i64],
            |r| {
                Ok(Preset {
                    username: r.get(0)?,
                    slot: r.get::<_, i64>(1)? as u8,
                    model: r.get(2)?,
                    mode: r.get(3)?,
                    effort: r.get(4)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Every saved slot of one user, slot order (`/preset` alone).
    pub fn list_presets(&self, username: &str) -> Result<Vec<Preset>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT username, slot, model, mode, effort FROM presets
             WHERE username = ?1 ORDER BY slot",
        )?;
        let rows = stmt.query_map(params![username], |r| {
            Ok(Preset {
                username: r.get(0)?,
                slot: r.get::<_, i64>(1)? as u8,
                model: r.get(2)?,
                mode: r.get(3)?,
                effort: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn presets_are_username_partitioned_and_overwritten() {
        let store = Store::open_in_memory().unwrap();
        store
            .save_preset("ada", 1, Some("openai/gpt-5.1"), Some("ask"), Some("high"))
            .unwrap();
        store
            .save_preset("bob", 1, Some("zai/glm-5.3"), None, None)
            .unwrap();

        // Slots are partitioned by username.
        let ada = store.preset("ada", 1).unwrap().unwrap();
        assert_eq!(ada.model.as_deref(), Some("openai/gpt-5.1"));
        assert_eq!(ada.mode.as_deref(), Some("ask"));
        assert_eq!(ada.effort.as_deref(), Some("high"));
        assert_eq!(
            store.preset("bob", 1).unwrap().unwrap().model.as_deref(),
            Some("zai/glm-5.3")
        );
        assert!(store.preset("carol", 1).unwrap().is_none());

        // Saving the same slot again overwrites, preserving other slots.
        store
            .save_preset("ada", 1, None, Some("readonly"), None)
            .unwrap();
        let ada = store.preset("ada", 1).unwrap().unwrap();
        assert_eq!(ada.model, None);
        assert_eq!(ada.mode.as_deref(), Some("readonly"));

        let listed = store.list_presets("ada").unwrap();
        assert_eq!(listed.len(), 1);
        assert!(store.list_presets("nobody").unwrap().is_empty());
    }
}
