// Approval decisions: D9 routing, explicit and latest-card settles, expiry,
// the session-allow set. Split from runtime.rs (moves only).
use super::events::rt_maybe_idle;
use super::*;
use ka_agent_core::{ApprovalDecision, ApprovalId, ApprovalKind, ApprovalStatus, Principal};

/// The identity of a gated call for "allow for this session": the exact
/// question and detail the operator saw.
pub(super) fn session_allow_key(question: &str, detail: Option<&str>) -> String {
    format!("{question}\n{}", detail.unwrap_or(""))
}

impl Runtime {
    /// Whether `who` may decide `approval`: the admin always; the acting user
    /// iff the card is theirs and was not escalated (D9); nobody else.
    pub fn may_decide(&self, approval: &Approval, who: &Principal) -> Result<(), String> {
        if who.is_admin() {
            return Ok(());
        }
        if approval.escalated {
            return Err("that action was escalated to the admin".to_string());
        }
        if approval.username.as_deref() != Some(who.username.as_str()) {
            return Err("that approval belongs to another user".to_string());
        }
        Ok(())
    }

    /// Re-authorize at execute time (D9): the user still exists, is enabled,
    /// and their role is still configured. A revoked actor turns an approved
    /// intent into a denial with an `approval.stale` audit row.
    fn reauthorize(&self, approval: &Approval) -> Result<(), String> {
        let Some(username) = approval.username.as_deref() else {
            return Ok(());
        };
        let user = self
            .sh
            .store
            .get_user(username)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("user {username} no longer exists"))?;
        if user.disabled {
            return Err(format!("{username} is disabled"));
        }
        if user.role != ka_agent_core::ADMIN_ROLE
            && self.role_spec(&user.role).is_none()
        {
            return Err(format!("role {:?} is no longer configured", user.role));
        }
        Ok(())
    }

    /// Decide a pending approval: answer the engine ask and audit (SEED
    /// §5.5: single-use).
    pub async fn decide(
        &self,
        approval_id: &str,
        decision: ApprovalDecision,
        source: &str,
    ) -> Result<(), Error> {
        let who = self
            .principal_for_source(source)
            .ok_or_else(|| Error::Refused("unlinked sender".into()))?;
        let id = ApprovalId(approval_id.to_string());
        let row = self
            .sh
            .store
            .get_approval(&id)
            .map_err(|_| Error::UnknownApproval(approval_id.to_string()))?;
        if let Err(reason) = self.may_decide(&row, &who) {
            let mut rec = AuditRecord::new(
                source,
                audit_actions::APPROVAL_DECIDE,
                approval_id,
                Some(format!("refused: {reason}")),
            )
            .with_role(&who.role)
            .with_user(&who.username);
            let _ = self.sh.store.append_audit(&mut rec);
            return Err(Error::Refused(if who.is_admin() {
                "approvals are decided by the admin role".into()
            } else {
                format!("approvals are decided by the admin role ({reason})")
            }));
        }
        // Re-authorization at execute time (D9): withdrawn access voids a
        // queued approval instead of executing it. The asking worker gets its
        // deny too, or its turn never settles (pending asks disarm the
        // watchdog and the idle transition).
        if let Err(reason) = self.reauthorize(&row) {
            let asking = row.username.clone().unwrap_or_else(|| who.username.clone());
            if let Some(st) = self.user_handle(&asking)
                && let Some((ask_id, port)) = {
                    let g = st.lock();
                    g.live.as_ref().and_then(|l| {
                        l.pending_asks
                            .get(&id)
                            .map(|a| (a.clone(), Arc::clone(&l.port)))
                    })
                }
            {
                port.answer(ask_id, ApprovalDecision::Deny.ka_choice())
                    .await
                    .ok();
                let mut g = st.lock();
                if let Some(live) = g.live.as_mut() {
                    live.pending_asks.remove(&id);
                }
                if g.session.status == SessionStatus::AwaitingApproval {
                    let from = g.session.status;
                    g.session
                        .transition(SessionStatus::Running)
                        .map_err(|t| illegal_session(from, t))
                        .ok();
                }
            }
            self.sh
                .store
                .decide_approval(&id, ApprovalStatus::Denied, ka_agent_core::now_ms())?;
            let mut rec = AuditRecord::new(
                "runtime",
                audit_actions::APPROVAL_STALE,
                approval_id,
                Some(reason.clone()),
            )
            .with_role(&who.role)
            .with_user(&who.username);
            let _ = self.sh.store.append_audit(&mut rec);
            let mut note = AuditRecord::new(
                "runtime",
                audit_actions::APPROVAL_DECIDE,
                approval_id,
                Some(format!("denied: {reason}")),
            )
            .with_user(&who.username);
            let _ = self.sh.store.append_audit(&mut note);
            self.sh.emit(AgentEvent::ApprovalSettled {
                id: approval_id.to_string(),
                status: format!("{:?}", ApprovalStatus::Denied),
            });
            return Err(Error::Refused(format!(
                "denied: {reason} — the approval no longer applies"
            )));
        }
        // The ask lives in the asking user's live worker.
        let asking = row.username.clone().unwrap_or_else(|| who.username.clone());
        let st = match self.user_handle(&asking) {
            Some(st) => st,
            None => return Err(Error::UnknownApproval(approval_id.to_string())),
        };
        let (ask_id, port) = {
            let g = st.lock();
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
        if row.status == ApprovalStatus::Pending && ka_agent_core::now_ms() >= row.expires_at_ms {
            return Err(Error::UnknownApproval(approval_id.to_string()));
        }
        let status = match decision {
            ApprovalDecision::Allow | ApprovalDecision::Always => ApprovalStatus::Approved,
            ApprovalDecision::Deny => ApprovalStatus::Denied,
        };
        port.answer(ask_id.clone(), decision.ka_choice()).await?;
        self.sh
            .store
            .decide_approval(&id, status, ka_agent_core::now_ms())?;
        self.audit_for(
            &who,
            audit_actions::APPROVAL_DECIDE,
            approval_id,
            Some(format!("{decision:?}")),
        )?;
        self.sh.emit(AgentEvent::ApprovalSettled {
            id: approval_id.to_string(),
            status: format!("{status:?}"),
        });
        let mut g = st.lock();
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

    /// "Allow for this session": allow this approval and every identical one
    /// (same question and detail) until `/new`. Terminal and web only — the
    /// caller enforces the surface.
    pub async fn decide_for_session(&self, approval_id: &str, source: &str) -> Result<(), Error> {
        let who = self
            .principal_for_source(source)
            .ok_or_else(|| Error::Refused("unlinked sender".into()))?;
        let approval = self
            .sh
            .store
            .get_approval(&ApprovalId(approval_id.to_string()))
            .map_err(|_| Error::UnknownApproval(approval_id.to_string()))?;
        if let ApprovalKind::KaAsk {
            question, detail, ..
        } = &approval.kind
            && let Some(st) = self.user_handle(&who.username)
        {
            let key = session_allow_key(question, detail.as_deref());
            st.lock().session_allow.insert(key);
        }
        self.decide(approval_id, ApprovalDecision::Allow, source)
            .await
    }

    /// Decide the most recent pending approval of the caller's own session
    /// (console `/y` and `/n`). Returns Ok(false) when nothing is pending.
    pub async fn decide_latest(
        &self,
        decision: ApprovalDecision,
        source: &str,
    ) -> Result<bool, Error> {
        let who = self
            .principal_for_source(source)
            .ok_or_else(|| Error::Refused("unlinked sender".into()))?;
        let sid = match self.session_id_for(&who) {
            Some(sid) => sid,
            None => return Ok(false),
        };
        let latest = self
            .sh
            .store
            .pending_approvals(&sid)?
            .into_iter()
            .max_by_key(|a| a.created_ms);
        match latest {
            Some(a) => {
                self.decide(a.id.as_str(), decision, source).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    // ---- lifecycle ------------------------------------------------------

    /// Expiry sweep (timer-driven): pending approvals past their TTL are
    /// denied, answered to the engine, and audited (fail closed).
    pub async fn expire_approvals(&self) -> Result<Vec<ApprovalId>, Error> {
        let expired = self.sh.store.expire_pending(ka_agent_core::now_ms())?;
        if expired.is_empty() {
            return Ok(expired);
        }
        // Each expired ask belongs to one user's live worker: answer there.
        for (_, st) in self.users_by_activity() {
            let answers: Vec<(String, Arc<PortSession>, String)> = {
                let g = st.lock();
                g.live
                    .as_ref()
                    .map(|l| {
                        expired
                            .iter()
                            .filter_map(|id| {
                                l.pending_asks.get(id).map(|ask| {
                                    (id.as_str().to_string(), Arc::clone(&l.port), ask.clone())
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            if answers.is_empty() {
                continue;
            }
            for (id, port, ask_id) in answers {
                self.audit_for(
                    &Principal::new(
                        st.lock().session.username.as_str(),
                        st.lock().role.as_str(),
                    ),
                    audit_actions::APPROVAL_EXPIRE,
                    &id,
                    None,
                )?;
                port.answer(ask_id, ApprovalDecision::Deny.ka_choice())
                    .await
                    .ok();
                self.sh.emit(AgentEvent::ApprovalSettled {
                    id: id.clone(),
                    status: format!("{:?}", ApprovalStatus::Expired),
                });
                let mut g = st.lock();
                if let Some(live) = g.live.as_mut() {
                    live.pending_asks.remove(&ApprovalId(id));
                }
            }
            // An expired ask while the session sat in `awaiting_approval` is
            // the end of that wait: with nothing else pending the turn must
            // settle.
            rt_maybe_idle(&self.sh, &st);
        }
        Ok(expired)
    }

    /// Pending approval cards the caller can act on: their own session's
    /// cards, plus — for the admin — the escalated queue (D9), so channel
    /// `/approvals`/`/y` reach cards raised in another user's session.
    pub fn pending_approval_cards(
        &self,
        who: &Principal,
    ) -> Result<Vec<crate::surface::ApprovalCard>, Error> {
        let mut out: Vec<crate::surface::ApprovalCard> = match self.session_id_for(who) {
            Some(sid) => self
                .sh
                .store
                .pending_approvals(&sid)?
                .iter()
                .filter_map(crate::surface::ApprovalCard::from_approval)
                .collect(),
            None => Vec::new(),
        };
        if who.is_admin() {
            let known: Vec<String> = out.iter().map(|c| c.approval_id.clone()).collect();
            for card in self.escalated_cards(50)? {
                if !known.contains(&card.approval_id) {
                    out.push(card);
                }
            }
            for card in self.cards_for_user(&who.username)? {
                if !out.iter().any(|c| c.approval_id == card.approval_id) {
                    out.push(card);
                }
            }
        }
        Ok(out)
    }

    /// The admin queue: every card escalated by D9 (plus pre-users cards).
    pub fn escalated_cards(
        &self,
        limit: usize,
    ) -> Result<Vec<crate::surface::ApprovalCard>, Error> {
        Ok(self
            .sh
            .store
            .escalated_approvals(limit)?
            .iter()
            .filter_map(crate::surface::ApprovalCard::from_approval)
            .collect())
    }

    /// The cards addressed to one user (their self-scope inbox).
    pub fn cards_for_user(
        &self,
        username: &str,
    ) -> Result<Vec<crate::surface::ApprovalCard>, Error> {
        Ok(self
            .sh
            .store
            .pending_approvals_for_user(username)?
            .iter()
            .filter_map(crate::surface::ApprovalCard::from_approval)
            .collect())
    }

}
