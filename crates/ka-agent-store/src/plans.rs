//! Plan artifacts (v4 F12 early slice): the editable plan a session
//! drafts, the operator approves, and the runtime executes with a
//! bounded verify/repair cycle. Status machine and the approval-arms-
//! verify security line live in `docs/contract/plans.md`.
//!
//! Writes are guarded here, at the store: the body is editable only
//! outside `executing` (the model may rewrite `drafting`/`draft` rows;
//! an operator `/plan set` also un-settles `approved`/`done`/`failed`/
//! `abandoned` back to `draft`), and status moves follow the legal-edge
//! table below — anything else is an [`StoreError::IllegalTransition`].

use super::*;

/// Lifecycle of one plan artifact (snake-case storage form matches the
/// schema CHECK).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanStatus {
    Drafting,
    Draft,
    Approved,
    Executing,
    Done,
    Failed,
    Abandoned,
}

impl PlanStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanStatus::Drafting => "drafting",
            PlanStatus::Draft => "draft",
            PlanStatus::Approved => "approved",
            PlanStatus::Executing => "executing",
            PlanStatus::Done => "done",
            PlanStatus::Failed => "failed",
            PlanStatus::Abandoned => "abandoned",
        }
    }

    fn parse(s: &str) -> Option<PlanStatus> {
        Some(match s {
            "drafting" => PlanStatus::Drafting,
            "draft" => PlanStatus::Draft,
            "approved" => PlanStatus::Approved,
            "executing" => PlanStatus::Executing,
            "done" => PlanStatus::Done,
            "failed" => PlanStatus::Failed,
            "abandoned" => PlanStatus::Abandoned,
            _ => return None,
        })
    }
}

impl std::fmt::Display for PlanStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One plan artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanRow {
    pub id: String,
    pub session: SessionId,
    pub username: String,
    pub title: String,
    pub body: String,
    /// The verify shell command, armed by `/plan approve`. `None` = the
    /// plan runs gateless and settles `done` on a clean turn.
    pub verify: Option<String>,
    /// Repair budget: how many gate-failure re-feeds the plan may spend.
    pub max_iterations: i64,
    /// Repairs spent so far (durable across restarts).
    pub iterations: i64,
    pub status: PlanStatus,
    /// The turn whose answer became the body (the `/plan draft` cycle).
    pub draft_turn: Option<TurnId>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// The status edges a plan may take. `/plan set` un-settles everything
/// but a live row (`drafting` is mid-capture, `executing` is running);
/// only an `executing` plan settles (`done`/`failed`) or stops
/// (`abandoned`).
fn legal_plan_edge(from: &PlanStatus, to: &PlanStatus) -> bool {
    use PlanStatus::*;
    matches!(
        (from, to),
        (Drafting, Draft)
            | (Draft, Approved)
            | (Approved, Draft)
            | (Approved, Executing)
            | (Executing, Done)
            | (Executing, Failed)
            | (Drafting, Abandoned)
            | (Draft, Abandoned)
            | (Approved, Abandoned)
            | (Executing, Abandoned)
    )
}

/// The column list every plan read uses (order is `row_to_plan`'s).
const PLAN_COLS: &str = "id, session, username, title, body, verify, max_iterations, \
                         iterations, status, draft_turn, created_ms, updated_ms";

fn row_to_plan(r: &rusqlite::Row<'_>) -> rusqlite::Result<PlanRow> {
    let status: String = r.get(8)?;
    Ok(PlanRow {
        id: r.get(0)?,
        session: SessionId(r.get(1)?),
        username: r.get(2)?,
        title: r.get(3)?,
        body: r.get(4)?,
        verify: r.get(5)?,
        max_iterations: r.get(6)?,
        iterations: r.get(7)?,
        status: PlanStatus::parse(&status).unwrap_or(PlanStatus::Drafting),
        draft_turn: r.get::<_, Option<String>>(9)?.map(TurnId),
        created_ms: r.get(10)?,
        updated_ms: r.get(11)?,
    })
}

impl Store {
    /// Insert a fresh plan (status `drafting`).
    pub fn insert_plan(&self, row: &PlanRow) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO plans(id, session, username, title, body, verify, max_iterations, \
             iterations, status, draft_turn, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                row.id,
                row.session.0,
                row.username,
                row.title,
                row.body,
                row.verify,
                row.max_iterations,
                row.iterations,
                row.status.as_str(),
                row.draft_turn.as_ref().map(|t| t.0.clone()),
                row.created_ms,
                row.updated_ms,
            ],
        )?;
        Ok(())
    }

    /// One plan by id.
    pub fn get_plan(&self, id: &str) -> Result<Option<PlanRow>> {
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT {PLAN_COLS} FROM plans WHERE id = ?1"),
            params![id],
            row_to_plan,
        )
        .optional()
        .map_err(Into::into)
    }

    /// A user's plans, oldest first (`/plan list` and id resolution —
    /// a plan survives session rotation; `session` is provenance).
    pub fn plans_for_user(&self, username: &str) -> Result<Vec<PlanRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {PLAN_COLS} FROM plans WHERE username = ?1 ORDER BY created_ms, id"
        ))?;
        let rows = stmt.query_map(params![username], row_to_plan)?;
        collect_rows(rows)
    }

    /// Rewrite the body of a `drafting`/`draft` plan (the runtime's
    /// capture and the model's `plan_update`). Anything else is refused:
    /// an approved plan is immutable to the model, and a live
    /// (`executing`) one would race its own verify cycle.
    pub fn set_plan_body(&self, id: &str, body: &str) -> Result<()> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE plans SET body = ?2, updated_ms = ?3
             WHERE id = ?1 AND status IN ('drafting','draft')",
            params![id, body, ka_agent_core::now_ms()],
        )?;
        if n == 0 {
            return Err(StoreError::Refused(format!(
                "plan {id} is not editable (only drafting or draft plans are)"
            )));
        }
        Ok(())
    }

    /// Lift (or clear) the verify command of a `drafting` plan — the
    /// capture step's write. Armed only by an operator `/plan approve`.
    pub fn set_plan_verify(&self, id: &str, verify: Option<&str>) -> Result<()> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE plans SET verify = ?2, updated_ms = ?3
             WHERE id = ?1 AND status = 'drafting'",
            params![id, verify, ka_agent_core::now_ms()],
        )?;
        if n == 0 {
            return Err(StoreError::Refused(format!(
                "plan {id} is not being drafted — its verify line cannot be lifted"
            )));
        }
        Ok(())
    }

    /// Operator `/plan set`: rewrite the body and un-settle the plan back
    /// to `draft` (re-approve to run). Refused while the plan is being
    /// captured (`drafting`) or running (`executing`).
    pub fn revise_plan_body(&self, id: &str, body: &str) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let status: String = tx
            .query_row("SELECT status FROM plans WHERE id = ?1", params![id], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("plan {id}")))?;
        let from = PlanStatus::parse(&status).unwrap_or(PlanStatus::Drafting);
        if matches!(from, PlanStatus::Drafting | PlanStatus::Executing) {
            return Err(StoreError::Refused(format!(
                "plan {id} is {from} — wait for it to settle before editing"
            )));
        }
        tx.execute(
            "UPDATE plans SET body = ?2, status = 'draft', updated_ms = ?3 WHERE id = ?1",
            params![id, body, ka_agent_core::now_ms()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Move a plan along its legal edges; anything else is an
    /// [`StoreError::IllegalTransition`] naming the rejected pair.
    pub fn transition_plan(&self, id: &str, from: PlanStatus, to: PlanStatus) -> Result<()> {
        if !legal_plan_edge(&from, &to) {
            return Err(StoreError::IllegalTransition {
                what: "plan",
                from: from.to_string(),
                to: to.to_string(),
            });
        }
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE plans SET status = ?3, updated_ms = ?4 WHERE id = ?1 AND status = ?2",
            params![id, from.as_str(), to.as_str(), ka_agent_core::now_ms()],
        )?;
        drop(conn);
        if n == 0 {
            // Either the id is unknown or the row moved underneath us;
            // both are the same refusal for the caller, named by where
            // the row actually is.
            let from = self.get_plan(id)?.map(|p| p.status).unwrap_or(from);
            return Err(StoreError::IllegalTransition {
                what: "plan",
                from: from.to_string(),
                to: to.to_string(),
            });
        }
        Ok(())
    }

    /// Spend one repair: `iterations + 1`, but only while the plan is
    /// `executing` and under its cap. `Err(Refused)` at the cap — the
    /// caller settles the plan `failed`.
    pub fn bump_plan_iteration(&self, id: &str) -> Result<i64> {
        let conn = self.lock();
        let bumped = conn.execute(
            "UPDATE plans SET iterations = iterations + 1, updated_ms = ?2
             WHERE id = ?1 AND status = 'executing' AND iterations < max_iterations",
            params![id, ka_agent_core::now_ms()],
        )?;
        if bumped == 0 {
            return Err(StoreError::Refused(format!(
                "plan {id} is not executing or is at its repair cap"
            )));
        }
        let iters: i64 = conn.query_row(
            "SELECT iterations FROM plans WHERE id = ?1",
            params![id],
            |r| r.get::<_, i64>(0),
        )?;
        Ok(iters)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn plan(id: &str, status: PlanStatus) -> PlanRow {
        PlanRow {
            id: id.to_string(),
            session: session().id,
            username: "operator".into(),
            title: "migrate the blog".into(),
            body: String::new(),
            verify: None,
            max_iterations: 2,
            iterations: 0,
            status,
            draft_turn: None,
            created_ms: ka_agent_core::now_ms(),
            updated_ms: ka_agent_core::now_ms(),
        }
    }

    #[test]
    fn capture_and_edges() {
        let st = Store::open_in_memory().unwrap();
        let p = plan("plan_a", PlanStatus::Drafting);
        st.insert_plan(&p).unwrap();
        st.set_plan_body(&p.id, "1. do it").unwrap();
        st.set_plan_verify(&p.id, Some("cargo test")).unwrap();
        st.transition_plan(&p.id, PlanStatus::Drafting, PlanStatus::Draft)
            .unwrap();
        let row = st.get_plan(&p.id).unwrap().unwrap();
        assert_eq!(row.status, PlanStatus::Draft);
        assert_eq!(row.verify.as_deref(), Some("cargo test"));
        // The capture writes are drafting-gated: flipping first refuses.
        let q = plan("plan_b", PlanStatus::Draft);
        st.insert_plan(&q).unwrap();
        assert!(st.set_plan_verify(&q.id, Some("x")).is_err());
        // approve -> run -> settle; a live plan refuses body writes.
        st.transition_plan(&q.id, PlanStatus::Draft, PlanStatus::Approved)
            .unwrap();
        st.transition_plan(&q.id, PlanStatus::Approved, PlanStatus::Executing)
            .unwrap();
        assert!(st.set_plan_body(&q.id, "nope").is_err());
        st.transition_plan(&q.id, PlanStatus::Executing, PlanStatus::Done)
            .unwrap();
    }

    #[test]
    fn illegal_edges_are_named() {
        let st = Store::open_in_memory().unwrap();
        let p = plan("plan_c", PlanStatus::Drafting);
        st.insert_plan(&p).unwrap();
        let err = st
            .transition_plan(&p.id, PlanStatus::Drafting, PlanStatus::Done)
            .unwrap_err();
        assert!(err.to_string().contains("drafting -> done"), "{err}");
        // A moved row reports where it actually is.
        st.transition_plan(&p.id, PlanStatus::Drafting, PlanStatus::Abandoned)
            .unwrap();
        let err = st
            .transition_plan(&p.id, PlanStatus::Draft, PlanStatus::Approved)
            .unwrap_err();
        assert!(err.to_string().contains("abandoned"), "{err}");
    }

    #[test]
    fn revise_unsettles_to_draft() {
        let st = Store::open_in_memory().unwrap();
        let p = plan("plan_d", PlanStatus::Approved);
        st.insert_plan(&p).unwrap();
        st.revise_plan_body(&p.id, "new body").unwrap();
        let row = st.get_plan(&p.id).unwrap().unwrap();
        assert_eq!(row.status, PlanStatus::Draft);
        assert_eq!(row.body, "new body");
        // A live plan refuses.
        st.transition_plan(&p.id, PlanStatus::Draft, PlanStatus::Approved)
            .unwrap();
        st.transition_plan(&p.id, PlanStatus::Approved, PlanStatus::Executing)
            .unwrap();
        assert!(st.revise_plan_body(&p.id, "nope").is_err());
    }

    #[test]
    fn bump_iteration_caps() {
        let st = Store::open_in_memory().unwrap();
        let mut p = plan("plan_e", PlanStatus::Approved);
        p.max_iterations = 2;
        st.insert_plan(&p).unwrap();
        // Not executing: no spend.
        assert!(st.bump_plan_iteration(&p.id).is_err());
        st.transition_plan(&p.id, PlanStatus::Approved, PlanStatus::Executing)
            .unwrap();
        assert_eq!(st.bump_plan_iteration(&p.id).unwrap(), 1);
        assert_eq!(st.bump_plan_iteration(&p.id).unwrap(), 2);
        assert!(st.bump_plan_iteration(&p.id).is_err());
        let row = st.get_plan(&p.id).unwrap().unwrap();
        assert_eq!(row.iterations, 2);
        assert_eq!(row.status, PlanStatus::Executing);
    }

    #[test]
    fn plans_list_per_user() {
        let st = Store::open_in_memory().unwrap();
        let a = plan("plan_f", PlanStatus::Draft);
        st.insert_plan(&a).unwrap();
        let mut b = plan("plan_g", PlanStatus::Done);
        b.username = "someone_else".into();
        b.session = SessionId("sess_other".into());
        st.insert_plan(&b).unwrap();
        let rows = st.plans_for_user("operator").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "plan_f");
    }
}
