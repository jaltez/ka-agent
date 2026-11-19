//! Skill attach counts (B12): one row per (username, skill), bumped when a
//! turn actually attaches the skill. `/skill-doctor` reads the rows; the
//! counts are the only skill state the store owns (the text stays in the
//! operator library).

use super::*;

/// One `skill_usage` row: attach counts by route and the latest attach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillUsage {
    pub username: String,
    pub skill: String,
    pub explicit: u32,
    pub trigger: u32,
    pub paths: u32,
    pub last_used_ms: i64,
}

fn row_to_skill_usage(r: &rusqlite::Row<'_>) -> rusqlite::Result<SkillUsage> {
    let count = |i: usize| -> rusqlite::Result<u32> { Ok(r.get::<_, i64>(i)?.max(0) as u32) };
    Ok(SkillUsage {
        username: r.get(0)?,
        skill: r.get(1)?,
        explicit: count(2)?,
        trigger: count(3)?,
        paths: count(4)?,
        last_used_ms: r.get(5)?,
    })
}

impl Store {
    /// Count one attach. The three flags are independent routes: an
    /// explicit `/skill` turn that also matched a trigger counts both.
    pub fn bump_skill_usage(
        &self,
        username: &str,
        skill: &str,
        explicit: bool,
        trigger: bool,
        paths: bool,
    ) -> Result<()> {
        let now = ka_agent_core::now_ms();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO skill_usage(username, skill, explicit, trigger, paths, last_used_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(username, skill) DO UPDATE SET
                 explicit = explicit + excluded.explicit,
                 trigger = trigger + excluded.trigger,
                 paths = paths + excluded.paths,
                 last_used_ms = excluded.last_used_ms",
            params![
                username,
                skill,
                i64::from(explicit),
                i64::from(trigger),
                i64::from(paths),
                now
            ],
        )?;
        Ok(())
    }

    /// One user's rows, name-ordered (`/skill-doctor`).
    pub fn skill_usage(&self, username: &str) -> Result<Vec<SkillUsage>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT username, skill, explicit, trigger, paths, last_used_ms
             FROM skill_usage WHERE username = ?1 ORDER BY skill",
        )?;
        let rows = stmt.query_map(params![username], row_to_skill_usage)?;
        collect_rows(rows)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn counts_accumulate_per_route_and_stay_per_user() {
        let store = Store::open_in_memory().unwrap();
        store
            .bump_skill_usage("ana", "deploy", false, true, false)
            .unwrap();
        store
            .bump_skill_usage("ana", "deploy", false, true, true)
            .unwrap();
        store
            .bump_skill_usage("ana", "review", true, false, false)
            .unwrap();
        store
            .bump_skill_usage("bo", "deploy", false, false, true)
            .unwrap();
        let ana = store.skill_usage("ana").unwrap();
        assert_eq!(ana.len(), 2);
        let deploy = ana.iter().find(|r| r.skill == "deploy").unwrap();
        assert_eq!((deploy.explicit, deploy.trigger, deploy.paths), (0, 2, 1));
        let review = ana.iter().find(|r| r.skill == "review").unwrap();
        assert_eq!((review.explicit, review.trigger, review.paths), (1, 0, 0));
        assert!(deploy.last_used_ms > 0);
        assert_eq!(store.skill_usage("bo").unwrap()[0].paths, 1);
    }
}
