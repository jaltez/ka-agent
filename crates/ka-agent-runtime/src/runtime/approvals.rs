// Approval decisions: explicit and latest-card settles, expiry, the
// session-allow set. Split from runtime.rs (moves only).
use super::events::rt_maybe_idle;
use super::*;

/// The identity of a gated call for "allow for this session": the exact
/// question and detail the operator saw.
pub(super) fn session_allow_key(question: &str, detail: Option<&str>) -> String {
    format!("{question}\n{}", detail.unwrap_or(""))
}

impl Runtime {
    /// Decide a pending approval: answer the engine ask and audit (SEED
    /// §5.5: single-use; "always" is never offered from channels).
    pub async fn decide(&self, approval_id: &str, decision: ApprovalDecision) -> Result<(), Error> {
        let id = ApprovalId(approval_id.to_string());
        let (ask_id, port) = {
            let g = self.lock();
            let Some(live) = g.live.as_ref() else {
                return Err(Error::UnknownApproval(approval_id.to_string()));
            };
            let Some(ask_id) = live.pending_asks.get(&id).cloned() else {
                return Err(Error::UnknownApproval(approval_id.to_string()));
            };
            (ask_id, Arc::clone(&live.port))
        };
        // Deny-on-expiry is an invariant, not a schedule: a decision that
        // arrives between the TTL and the next sweep must not allow the ask.
        if let Ok(row) = self.sh.store.get_approval(&id)
            && row.status == ka_agent_core::ApprovalStatus::Pending
            && ka_agent_core::now_ms() >= row.expires_at_ms
        {
            return Err(Error::UnknownApproval(approval_id.to_string()));
        }
        let status = match decision {
            ApprovalDecision::Allow | ApprovalDecision::Always => {
                ka_agent_core::ApprovalStatus::Approved
            }
            ApprovalDecision::Deny => ka_agent_core::ApprovalStatus::Denied,
        };
        port.answer(ask_id.clone(), decision.ka_choice()).await?;
        self.sh
            .store
            .decide_approval(&id, status, ka_agent_core::now_ms())?;
        self.audit(
            audit_actions::APPROVAL_DECIDE,
            approval_id,
            Some(format!("{decision:?}")),
        )?;
        self.sh.surface.on_approval_settled(approval_id, status);
        let mut g = self.lock();
        if let Some(live) = g.live.as_mut() {
            live.pending_asks.remove(&id);
        }
        if g.session.status == SessionStatus::AwaitingApproval {
            let from = g.session.status;
            g.session
                .transition(SessionStatus::Running)
                .map_err(|t| illegal_session(from, t))?;
        }
        Ok(())
    }

    /// "Allow for this session": allow this approval and every identical
    /// one (same question and detail) until `/new`. Terminal and web only —
    /// the caller enforces the surface.
    pub async fn decide_for_session(&self, approval_id: &str) -> Result<(), Error> {
        let approval = self
            .sh
            .store
            .get_approval(&ApprovalId(approval_id.to_string()))
            .map_err(|_| Error::UnknownApproval(approval_id.to_string()))?;
        if let ApprovalKind::KaAsk {
            question, detail, ..
        } = &approval.kind
        {
            let key = session_allow_key(question, detail.as_deref());
            self.lock().session_allow.insert(key);
        }
        self.decide(approval_id, ApprovalDecision::Allow).await
    }

    /// Decide the most recent pending approval (console `/y` and `/n`).
    /// Returns Ok(false) when nothing is pending.
    pub async fn decide_latest(&self, decision: ApprovalDecision) -> Result<bool, Error> {
        let sid = self.session_id();
        let latest = self
            .sh
            .store
            .pending_approvals(&sid)?
            .into_iter()
            .max_by_key(|a| a.created_ms);
        match latest {
            Some(a) => {
                self.decide(a.id.as_str(), decision).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    // ---- scheduler (SEED.md §7.2; generalized cron M11) -----------------

    /// Expiry sweep (timer-driven): pending approvals past their TTL are
    /// denied, answered to the engine, and audited (fail closed).
    pub async fn expire_approvals(&self) -> Result<Vec<ApprovalId>, Error> {
        let expired = self.sh.store.expire_pending(ka_agent_core::now_ms())?;
        let answers: Vec<String> = {
            let g = self.lock();
            g.live
                .as_ref()
                .map(|l| {
                    expired
                        .iter()
                        .filter_map(|id| l.pending_asks.get(id).cloned())
                        .collect()
                })
                .unwrap_or_default()
        };
        for id in &expired {
            self.audit(audit_actions::APPROVAL_EXPIRE, id.as_str(), None)?;
            self.sh
                .surface
                .on_approval_settled(id.as_str(), ka_agent_core::ApprovalStatus::Expired);
        }
        if let Some(port) = self.live_port() {
            for ask_id in answers {
                port.answer(ask_id, ApprovalDecision::Deny.ka_choice())
                    .await
                    .ok();
            }
        }
        // The engine has its deny; the ask is settled. Leaving it in the map
        // would keep `ask_pending` true forever, which blocks the idle
        // transition and the queue drain — every later message would queue
        // with no engine left to consume it.
        {
            let mut g = self.lock();
            if let Some(live) = g.live.as_mut() {
                for id in &expired {
                    live.pending_asks.remove(id);
                }
            }
        }
        // An expired ask while the session sat in `awaiting_approval` is the
        // end of that wait: with nothing else pending the turn must settle.
        rt_maybe_idle(&self.sh);
        Ok(expired)
    }

    /// Pending approval cards of the live session (`/approvals`, `/y <id8>`).
    pub fn pending_approval_cards(&self) -> Result<Vec<crate::surface::ApprovalCard>, Error> {
        let sid = self.session_id();
        Ok(self
            .sh
            .store
            .pending_approvals(&sid)?
            .iter()
            .filter_map(crate::surface::ApprovalCard::from_approval)
            .collect())
    }
}
