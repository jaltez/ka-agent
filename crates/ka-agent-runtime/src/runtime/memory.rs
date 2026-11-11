// Memory: remember/forget, proposals, history lookup, compiled refresh.
// Split from runtime.rs (moves only).
use super::*;

/// Meta key: `created_ms` of the newest memory candidate already offered.
const MEMORY_ANNOUNCED_KEY: &str = "memory.announced_ms";

impl Runtime {
    /// The memory store of one role, opened at its own root on first use.
    pub fn memory_store(&self, role: &str) -> Result<ka_agent_memory::MemoryStore, Error> {
        let mut map = self.sh.memories.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(store) = map.get(role) {
            return Ok(store.clone());
        }
        let store = ka_agent_memory::MemoryStore::open(&self.sh.paths.role_memory(role))?;
        map.insert(role.to_string(), store.clone());
        Ok(store)
    }

    /// The active role's memory store (every `/memory`, `/remember` and
    /// `/forget` acts on it).
    fn active_memory(&self) -> Result<ka_agent_memory::MemoryStore, Error> {
        self.memory_store(&self.active_role())
    }

    /// `/remember <fact>`: explicit operator acceptance (SEED.md §4.4 path
    /// 1) — append, recompile the engine's MEMORY.md, git-commit.
    pub fn remember(&self, text: &str, source: &str) -> Result<String, Error> {
        if self.session_private() {
            return Err(Error::Refused(
                "this is a private conversation — memory is off; /new starts a normal one".into(),
            ));
        }
        let memory = self.active_memory()?;
        let id = memory.add(ka_agent_memory::MemoryFile::Facts, text, source, None)?;
        self.refresh_compiled_memory()?;
        memory.commit(&format!("memory: accept {id}"))?;
        self.audit(
            audit_actions::MEMORY_ACCEPT,
            &id,
            Some(text.chars().take(80).collect()),
        )?;
        Ok(id)
    }

    /// `/forget <id>`: remove the entry and recompile.
    pub fn forget(&self, id: &str) -> Result<bool, Error> {
        let memory = self.active_memory()?;
        let removed = memory.remove(id)?;
        if removed {
            self.refresh_compiled_memory()?;
            memory.commit(&format!("memory: forget {id}"))?;
            self.audit(audit_actions::MEMORY_EDIT, id, Some("forget".into()))?;
        }
        Ok(removed)
    }

    /// `/memory`: what is injected and at what budget.
    pub fn memory_report(&self) -> Result<ka_agent_memory::CompileReport, Error> {
        Ok(self
            .active_memory()?
            .compile(ka_agent_memory::DEFAULT_BUDGET_BYTES)?)
    }

    /// Accepted memory entries, newest first, for the web memory view.
    pub fn memory_entries(&self) -> Result<Vec<ka_agent_memory::Entry>, Error> {
        let mut entries = self.active_memory()?.entries()?;
        entries.sort_by_key(|e| std::cmp::Reverse(e.accepted_ms));
        Ok(entries)
    }

    /// The exact text injected into the engine at the current budget.
    pub fn memory_compiled_text(&self) -> Result<String, Error> {
        Ok(self
            .active_memory()?
            .compiled_text(ka_agent_memory::DEFAULT_BUDGET_BYTES)?)
    }

    /// Register a memory candidate for review (SEED.md §4.4 path 2; the
    /// model-side producer arrives with the connector bridge).
    pub fn propose_memory(
        &self,
        source: &str,
        text: &str,
        provenance: Option<&str>,
    ) -> Result<ka_agent_core::MemoryId, Error> {
        let id = ka_agent_core::MemoryId::generate();
        let role = self.active_role();
        self.sh
            .store
            .add_candidate_for_role(&id, source, text, provenance, None, &role)?;
        Ok(id)
    }

    /// The raw text of the file an accepted candidate lands in (the web
    /// editor renders the diff against it).
    pub fn memory_facts_text(&self) -> Result<Option<String>, Error> {
        Ok(self
            .active_memory()?
            .file_text(&ka_agent_memory::MemoryFile::Facts)?)
    }

    /// `/memory review`: pending candidates. A role sees only the
    /// proposals its own turns made; admin reviews every subject's.
    pub fn pending_candidates(&self) -> Result<Vec<ka_agent_store::CandidateRow>, Error> {
        let role = self.active_role();
        let all = self.sh.store.pending_candidates_full()?;
        Ok(if role == ka_agent_core::ADMIN_ROLE {
            all
        } else {
            all.into_iter().filter(|c| c.role == role).collect()
        })
    }

    /// `/memory accept <id>`: candidate → stable memory. The id may be a
    /// unique prefix (like `/cron rm`).
    /// `/memory log`: the most recent memory commits, newest first.
    pub fn memory_log(&self, n: usize) -> Result<Vec<(String, String, String)>, Error> {
        self.active_memory()?.log(n).map_err(Error::Memory)
    }

    /// `/memory undo`: revert the most recent memory commit and recompile
    /// MEMORY.md. Returns the reverted commit's subject.
    pub fn memory_undo(&self) -> Result<String, Error> {
        let subject = self.active_memory()?.undo_last().map_err(Error::Memory)?;
        self.refresh_compiled_memory()?;
        self.audit(audit_actions::MEMORY_ACCEPT, "undo", Some(subject.clone()))
            .ok();
        Ok(subject)
    }

    pub fn accept_candidate(&self, id: &str) -> Result<bool, Error> {
        let pending = self.pending_candidates()?;
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
        // The proposal names its target file (facts by default).
        let file = match row.file.as_deref() {
            None | Some("facts") => ka_agent_memory::MemoryFile::Facts,
            Some("preferences") => ka_agent_memory::MemoryFile::Preferences,
            Some("environment") => ka_agent_memory::MemoryFile::Environment,
            Some("profile") => ka_agent_memory::MemoryFile::Profile,
            Some(other) => {
                return Err(Error::Refused(format!(
                    "candidate names an unknown memory file {other:?}"
                )));
            }
        };
        // The proposal lands in the memory root of the role whose turn
        // proposed it, never the active role's by accident.
        let memory = self.memory_store(&row.role)?;
        let mem_id = memory.add(file, &row.text, &row.source, None)?;
        self.refresh_compiled_memory_for(&row.role)?;
        memory.commit(&format!("memory: accept {mem_id} (candidate)"))?;
        self.sh.store.set_candidate_status(&row.id, "accepted")?;
        self.audit(
            audit_actions::MEMORY_ACCEPT,
            &mem_id,
            Some(row.text.chars().take(80).collect()),
        )?;
        Ok(true)
    }

    /// `/memory reject <id>`: the same full-id-or-unique-prefix rule.
    pub fn reject_candidate(&self, id: &str) -> Result<bool, Error> {
        let pending = self.pending_candidates()?;
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
        self.audit(
            audit_actions::MEMORY_EDIT,
            row.id.as_str(),
            Some("candidate rejected".into()),
        )?;
        Ok(true)
    }

    /// `/history <query>`: FTS over episodes (SEED.md §4.5).
    pub fn history_search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ka_agent_store::EpisodeHit>, Error> {
        let role = self.active_role();
        self.history_search_for_role(query, limit, &role)
    }

    /// History search scoped to one role's sessions (admin: everything).
    pub fn history_search_for_role(
        &self,
        query: &str,
        limit: usize,
        role: &str,
    ) -> Result<Vec<ka_agent_store::EpisodeHit>, Error> {
        Ok(if role == ka_agent_core::ADMIN_ROLE {
            self.sh.store.search_episodes(query, limit)?
        } else {
            self.sh.store.search_episodes_for_role(query, limit, role)?
        })
    }

    /// `/history show <turn|prefix>`: the stored exchange. The id may be
    /// the full id or a prefix (with or without the `turn_` family).
    pub fn history_show(&self, turn_id: &str) -> Result<Option<(String, String)>, Error> {
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
        // A role only reads turns of its own sessions.
        let role = self.active_role();
        if role != ka_agent_core::ADMIN_ROLE
            && self.sh.store.turn_role(&turn.id)?.as_deref() != Some(role.as_str())
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

    pub(super) fn refresh_compiled_memory(&self) -> Result<(), Error> {
        self.refresh_compiled_memory_for(&self.active_role())
    }

    /// Recompile one role's MEMORY.md into that role's engine home.
    pub(super) fn refresh_compiled_memory_for(&self, role: &str) -> Result<(), Error> {
        let memory = self.memory_store(role)?;
        memory.install_compiled(
            &self.sh.paths.role_engine_home(role).join(".config/ka"),
            ka_agent_memory::DEFAULT_BUDGET_BYTES,
            &memory_preamble(&self.cfg()),
        )?;
        Ok(())
    }

    /// Offer each new `memory_propose` candidate to the operator once (the
    /// bridge writes them from another process; the tick finds them). A
    /// burst shows three cards and a pointer to the rest.
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
            self.sh.emit(AgentEvent::MemoryProposal {
                id: c.id.as_str().to_string(),
                text: c.text.clone(),
            });
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
    /// cycles with their answers, skipping the `offset` newest first
    /// (paging for the history picker).
    pub fn recent_turns(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<(ka_agent_core::Turn, String)>, Error> {
        let sid = self.session_id();
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
