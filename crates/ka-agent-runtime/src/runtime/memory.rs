// Memory: remember/forget, proposals, history lookup, compiled refresh.
// Split from runtime.rs (moves only).
use super::*;
use ka_agent_core::Principal;

/// Meta key: `created_ms` of the newest memory candidate already offered.
const MEMORY_ANNOUNCED_KEY: &str = "memory.announced_ms";

impl Runtime {
    /// The memory store of one user, opened at their own root on first use.
    pub fn memory_store(&self, username: &str) -> Result<ka_agent_memory::MemoryStore, Error> {
        let mut map = self.sh.memories.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(store) = map.get(username) {
            return Ok(store.clone());
        }
        let store = ka_agent_memory::MemoryStore::open(&self.sh.paths.user_memory(username))?;
        map.insert(username.to_string(), store.clone());
        Ok(store)
    }

    /// `/remember <fact>`: explicit acceptance (SEED.md §4.4 path 1) —
    /// append, recompile the engine's MEMORY.md, git-commit.
    pub fn remember(&self, text: &str, source: &str, who: &Principal) -> Result<String, Error> {
        if self.session_private_for(who) {
            return Err(Error::Refused(
                "this is a private conversation — memory is off; /new starts a normal one".into(),
            ));
        }
        let memory = self.memory_store(&who.username)?;
        let id = memory.add(ka_agent_memory::MemoryFile::Facts, text, source, None)?;
        self.refresh_compiled_memory_for(&who.username)?;
        memory.commit(&format!("memory: accept {id}"))?;
        self.audit_for(
            who,
            audit_actions::MEMORY_ACCEPT,
            &id,
            Some(text.chars().take(80).collect()),
        )?;
        Ok(id)
    }

    /// `/forget <id>`: remove the entry and recompile.
    pub fn forget(&self, id: &str, who: &Principal) -> Result<bool, Error> {
        let memory = self.memory_store(&who.username)?;
        let removed = memory.remove(id)?;
        if removed {
            self.refresh_compiled_memory_for(&who.username)?;
            memory.commit(&format!("memory: forget {id}"))?;
            self.audit_for(who, audit_actions::MEMORY_EDIT, id, Some("forget".into()))?;
        }
        Ok(removed)
    }

    /// `/memory`: what is injected and at what budget.
    pub fn memory_report_for(
        &self,
        who: &Principal,
    ) -> Result<ka_agent_memory::CompileReport, Error> {
        Ok(self
            .memory_store(&who.username)?
            .compile(ka_agent_memory::DEFAULT_BUDGET_BYTES)?)
    }

    /// Accepted memory entries, newest first, for the web memory view.
    pub fn memory_entries(&self, who: &Principal) -> Result<Vec<ka_agent_memory::Entry>, Error> {
        let mut entries = self.memory_store(&who.username)?.entries()?;
        entries.sort_by_key(|e| std::cmp::Reverse(e.accepted_ms));
        Ok(entries)
    }

    /// The exact text injected into the engine at the current budget.
    pub fn memory_compiled_text(&self, who: &Principal) -> Result<String, Error> {
        Ok(self
            .memory_store(&who.username)?
            .compiled_text(ka_agent_memory::DEFAULT_BUDGET_BYTES)?)
    }

    /// Register a memory candidate for review (SEED.md §4.4 path 2).
    pub fn propose_memory(
        &self,
        source: &str,
        text: &str,
        provenance: Option<&str>,
        who: &Principal,
    ) -> Result<ka_agent_core::MemoryId, Error> {
        let id = ka_agent_core::MemoryId::generate();
        self.sh.store.add_candidate_for_user(
            &id,
            source,
            text,
            provenance,
            None,
            &who.role,
            &who.username,
        )?;
        Ok(id)
    }

    /// The raw text of the file an accepted candidate lands in (the web
    /// editor renders the diff against it).
    pub fn memory_facts_text(&self, who: &Principal) -> Result<Option<String>, Error> {
        Ok(self
            .memory_store(&who.username)?
            .file_text(&ka_agent_memory::MemoryFile::Facts)?)
    }

    /// `/memory review`: pending candidates. A user sees only the proposals
    /// their own turns made; the admin reviews every subject's.
    pub fn pending_candidates_for(
        &self,
        who: &Principal,
    ) -> Result<Vec<ka_agent_store::CandidateRow>, Error> {
        let all = self.sh.store.pending_candidates_full()?;
        Ok(if who.is_admin() {
            all
        } else {
            all.into_iter()
                .filter(|c| c.username == who.username)
                .collect()
        })
    }

    /// `/memory log`: the most recent memory commits, newest first.
    pub fn memory_log(
        &self,
        n: usize,
        who: &Principal,
    ) -> Result<Vec<(String, String, String)>, Error> {
        self.memory_store(&who.username)?
            .log(n)
            .map_err(Error::Memory)
    }

    /// `/memory undo`: revert the most recent memory commit and recompile
    /// MEMORY.md. Returns the reverted commit's subject.
    pub fn memory_undo(&self, who: &Principal) -> Result<String, Error> {
        let subject = self
            .memory_store(&who.username)?
            .undo_last()
            .map_err(Error::Memory)?;
        self.refresh_compiled_memory_for(&who.username)?;
        self.audit_for(
            who,
            audit_actions::MEMORY_ACCEPT,
            "undo",
            Some(subject.clone()),
        )
        .ok();
        Ok(subject)
    }

    /// `/memory accept <id>`: candidate → stable memory. The id may be a
    /// unique prefix (like `/cron rm`).
    pub fn accept_candidate(&self, id: &str, who: &Principal) -> Result<bool, Error> {
        let pending = self.pending_candidates_for(who)?;
        let matches: Vec<_> = pending
            .iter()
            .filter(|c| ka_agent_core::ids::id_matches(c.id.as_str(), id))
            .collect();
        let Some(row) = (match matches.as_slice() {
            [one] => Some((**one).clone()),
            [] => None,
            _ => {
                return Err(Error::Refused(
                    "ambiguous candidate id: type more of it".into(),
                ));
            }
        }) else {
            return Ok(false);
        };
        // Screening rules may have tightened since the candidate was
        // filed: re-run the screen on the stored text before anything
        // moves (B15a).
        ka_agent_memory::screen_proposal(&row.text).map_err(Error::Refused)?;
        let file = Self::memory_file_of(row.file.as_deref())?;
        // The proposal lands in the memory root of the user whose turn
        // proposed it, never the decider's by accident.
        let memory = self.memory_store(&row.username)?;
        // Screen → apply → rollback on any failure: the operator's
        // memory never lands half-applied (B15a).
        let snapshot = memory.snapshot_file(&file)?;
        let applied: Result<String, Error> = (|| {
            let mem_id = memory
                .add(file.clone(), &row.text, &row.source, None)
                .map_err(Error::Memory)?;
            self.refresh_compiled_memory_for(&row.username)?;
            memory
                .commit(&format!("memory: accept {mem_id} (candidate)"))
                .map_err(Error::Memory)?;
            Ok(mem_id)
        })();
        let mem_id = match applied {
            Ok(id) => id,
            Err(e) => {
                memory.restore_file(&file, snapshot)?;
                self.refresh_compiled_memory_for(&row.username)?;
                return Err(e);
            }
        };
        self.sh.store.set_candidate_status(&row.id, "accepted")?;
        self.audit_for(
            who,
            audit_actions::MEMORY_ACCEPT,
            &mem_id,
            Some(row.text.chars().take(80).collect()),
        )?;
        Ok(true)
    }

    /// `/memory reject <id>`: the same full-id-or-unique-prefix rule.
    pub fn reject_candidate(&self, id: &str, who: &Principal) -> Result<bool, Error> {
        let pending = self.pending_candidates_for(who)?;
        let matches: Vec<_> = pending
            .iter()
            .filter(|c| ka_agent_core::ids::id_matches(c.id.as_str(), id))
            .collect();
        let row = match matches.as_slice() {
            [one] => (**one).clone(),
            [] => return Ok(false),
            _ => {
                return Err(Error::Refused(
                    "ambiguous candidate id: type more of it".into(),
                ));
            }
        };
        self.sh
            .store
            .set_candidate_status(&row.id.clone(), "rejected")?;
        self.audit_for(
            who,
            audit_actions::MEMORY_EDIT,
            row.id.as_str(),
            Some("candidate rejected".into()),
        )?;
        Ok(true)
    }

    /// `/history <query>`: FTS over the caller's own episodes (SEED.md §4.5).
    pub fn history_search(
        &self,
        query: &str,
        limit: usize,
        who: &Principal,
    ) -> Result<Vec<ka_agent_store::EpisodeHit>, Error> {
        self.history_search_filtered(query, limit, &ka_agent_store::EpisodeFilter::default(), who)
    }

    /// [`history_search`](Self::history_search) with the leading-position
    /// `/history` filters (`model:`/`after:`/`status:`) applied as bound
    /// SQL predicates on the `turns` join.
    pub fn history_search_filtered(
        &self,
        query: &str,
        limit: usize,
        filter: &ka_agent_store::EpisodeFilter,
        who: &Principal,
    ) -> Result<Vec<ka_agent_store::EpisodeHit>, Error> {
        Ok(if who.is_admin() {
            self.sh
                .store
                .search_episodes_filtered(query, limit, filter)?
        } else {
            self.sh
                .store
                .search_episodes_for_user_filtered(query, limit, &who.username, filter)?
        })
    }

    /// `/history` with filters but no search text: newest-first turn
    /// listing, the caller's own turns (every user's for the admin).
    pub fn history_list_filtered(
        &self,
        filter: &ka_agent_store::EpisodeFilter,
        limit: usize,
        who: &Principal,
    ) -> Result<Vec<ka_agent_store::EpisodeHit>, Error> {
        let scope = (!who.is_admin()).then_some(who.username.as_str());
        Ok(self.sh.store.list_turns_filtered(filter, scope, limit)?)
    }

    /// Recorded turns in the user's live session — the `memory_propose`
    /// warm-up gate (B15c); `None` before their first message.
    pub fn session_turn_count_for(&self, username: &str) -> Option<i64> {
        let sid = self.user_handle(username)?.lock().session.id.clone();
        self.sh.store.count_turns_for_session(&sid).ok()
    }

    /// `/history show <turn|prefix>`: the stored exchange. The id may be the
    /// full id or a prefix (with or without the `turn_` family).
    pub fn history_show(
        &self,
        turn_id: &str,
        who: &Principal,
    ) -> Result<Option<(String, String)>, Error> {
        let turn = match self
            .sh
            .store
            .get_turn(&ka_agent_core::TurnId(turn_id.to_string()))?
        {
            Some(t) => t,
            None => match self.sh.store.find_turn_by_prefix(turn_id.trim())? {
                Some(t) => t,
                None => return Ok(None),
            },
        };
        // A user only reads turns of their own sessions (the operator's
        // history view keeps seeing everything).
        if !who.is_admin()
            && self.sh.store.turn_username(&turn.id)?.as_deref() != Some(who.username.as_str())
        {
            return Ok(None);
        }
        let answer = self
            .sh
            .store
            .chunks_for_turn(&turn.id)?
            .into_iter()
            .map(|c| c.text)
            .collect::<Vec<_>>()
            .join("");
        Ok(Some((turn.inbound_text, answer)))
    }

    /// Recompile one user's MEMORY.md into their engine home.
    pub(super) fn refresh_compiled_memory_for(&self, username: &str) -> Result<(), Error> {
        let memory = self.memory_store(username)?;
        memory.install_compiled(
            &self.sh.paths.user_engine_home(username).join(".config/ka"),
            ka_agent_memory::DEFAULT_BUDGET_BYTES,
            &memory_preamble(&self.cfg()),
        )?;
        Ok(())
    }

    /// Install the automation compile (machine facts only, B15b) for a
    /// scheduled turn — the lean variant the daemon swaps in before a
    /// cron/briefing submit and swaps out at every cycle exit.
    pub(super) fn install_automation_memory_for(&self, username: &str) -> Result<(), Error> {
        let memory = self.memory_store(username)?;
        memory.install_compiled_scoped(
            &self.sh.paths.user_engine_home(username).join(".config/ka"),
            ka_agent_memory::DEFAULT_BUDGET_BYTES,
            &memory_preamble(&self.cfg()),
            ka_agent_memory::CompileScope::Automation,
        )?;
        Ok(())
    }

    /// The memory file a candidate names (`facts` by default).
    fn memory_file_of(name: Option<&str>) -> Result<ka_agent_memory::MemoryFile, Error> {
        Ok(match name {
            None | Some("facts") => ka_agent_memory::MemoryFile::Facts,
            Some("preferences") => ka_agent_memory::MemoryFile::Preferences,
            Some("environment") => ka_agent_memory::MemoryFile::Environment,
            Some("profile") => ka_agent_memory::MemoryFile::Profile,
            Some(other) => {
                return Err(Error::Refused(format!(
                    "candidate names an unknown memory file {other:?}"
                )));
            }
        })
    }

    /// The bullet diff + dry-run compile impact one pending candidate
    /// would have if accepted now (B15a): what `/memory review` renders.
    /// Returns the preview, the byte total today, and the report after.
    pub fn candidate_preview(
        &self,
        row: &ka_agent_store::CandidateRow,
    ) -> Result<
        (
            ka_agent_memory::DiffPreview,
            usize,
            ka_agent_memory::CompileReport,
        ),
        Error,
    > {
        let file = Self::memory_file_of(row.file.as_deref())?;
        let memory = self.memory_store(&row.username)?;
        let before = memory.compile(ka_agent_memory::DEFAULT_BUDGET_BYTES)?;
        let (preview, after) =
            memory.dry_run_add(file, &row.text, ka_agent_memory::DEFAULT_BUDGET_BYTES)?;
        Ok((preview, before.total_bytes, after))
    }

    /// Offer each new `memory_propose` candidate to its user once (the bridge
    /// writes them from another process; the tick finds them). A burst shows
    /// three cards and a pointer to the rest.
    pub fn announce_memory_proposals(&self) -> Result<usize, Error> {
        const MAX_CARDS: usize = 3;
        let since: i64 = self
            .sh
            .store
            .meta_get(MEMORY_ANNOUNCED_KEY)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let fresh: Vec<_> = self
            .sh
            .store
            .pending_candidates_full()?
            .into_iter()
            .filter(|c| c.created_ms > since)
            .collect();
        let Some(newest) = fresh.iter().map(|c| c.created_ms).max() else {
            return Ok(0);
        };
        for c in fresh.iter().take(MAX_CARDS) {
            // The card goes to the proposing user's own surfaces; the
            // operator also sees it (the admin reviews every subject's).
            let source = format!("web:{}", c.username);
            let ev = AgentEvent::MemoryProposal {
                id: c.id.as_str().to_string(),
                text: c.text.clone(),
            };
            self.sh.emit_from(ev.clone(), Some(&source));
            if !c.username.is_empty() && c.username != ka_agent_core::OPERATOR_USER {
                self.sh.emit_from(ev, None);
            }
        }
        if fresh.len() > MAX_CARDS {
            self.sh.emit(AgentEvent::Notice {
                text: format!(
                    "{} more memory proposals — /memory review",
                    fresh.len() - MAX_CARDS
                ),
            });
        }
        self.sh
            .store
            .meta_set(MEMORY_ANNOUNCED_KEY, &newest.to_string())?;
        Ok(fresh.len())
    }

    /// The `id8` history line listing: the session's last `limit` prompt
    /// cycles with their answers, skipping the `offset` newest first.
    pub fn recent_turns(
        &self,
        limit: usize,
        offset: usize,
        who: &Principal,
    ) -> Result<Vec<(ka_agent_core::Turn, String)>, Error> {
        let Some(sid) = self.session_id_for(who) else {
            return Ok(Vec::new());
        };
        let turns = self
            .sh
            .store
            .latest_turns_for_session(&sid, limit, offset)?;
        let mut out = Vec::new();
        for t in turns {
            let answer: String = self
                .sh
                .store
                .chunks_for_turn(&t.id)?
                .into_iter()
                .map(|c| c.text)
                .collect();
            out.push((t, answer));
        }
        Ok(out)
    }
}
