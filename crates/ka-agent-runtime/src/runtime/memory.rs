// Memory: remember/forget, proposals, history lookup, compiled refresh.
// Split from runtime.rs (moves only).
use super::*;

/// Meta key: `created_ms` of the newest memory candidate already offered.
const MEMORY_ANNOUNCED_KEY: &str = "memory.announced_ms";

impl Runtime {
    /// `/remember <fact>`: explicit operator acceptance (SEED.md §4.4 path
    /// 1) — append, recompile the engine's MEMORY.md, git-commit.
    pub fn remember(&self, text: &str, source: &str) -> Result<String, Error> {
        let id = self
            .sh
            .memory
            .add(ka_agent_memory::MemoryFile::Facts, text, source, None)?;
        self.refresh_compiled_memory()?;
        self.sh.memory.commit(&format!("memory: accept {id}"))?;
        self.audit(
            audit_actions::MEMORY_ACCEPT,
            &id,
            Some(text.chars().take(80).collect()),
        )?;
        Ok(id)
    }

    /// `/forget <id>`: remove the entry and recompile.
    pub fn forget(&self, id: &str) -> Result<bool, Error> {
        let removed = self.sh.memory.remove(id)?;
        if removed {
            self.refresh_compiled_memory()?;
            self.sh.memory.commit(&format!("memory: forget {id}"))?;
            self.audit(audit_actions::MEMORY_EDIT, id, Some("forget".into()))?;
        }
        Ok(removed)
    }

    /// `/memory`: what is injected and at what budget.
    pub fn memory_report(&self) -> Result<ka_agent_memory::CompileReport, Error> {
        Ok(self
            .sh
            .memory
            .compile(ka_agent_memory::DEFAULT_BUDGET_BYTES)?)
    }

    /// Accepted memory entries, newest first, for the web memory view.
    pub fn memory_entries(&self) -> Result<Vec<ka_agent_memory::Entry>, Error> {
        let mut entries = self.sh.memory.entries()?;
        entries.sort_by_key(|e| std::cmp::Reverse(e.accepted_ms));
        Ok(entries)
    }

    /// The exact text injected into the engine at the current budget.
    pub fn memory_compiled_text(&self) -> Result<String, Error> {
        Ok(self
            .sh
            .memory
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
        self.sh.store.add_candidate(&id, source, text, provenance)?;
        Ok(id)
    }

    /// The raw text of the file an accepted candidate lands in (the web
    /// editor renders the diff against it).
    pub fn memory_facts_text(&self) -> Result<Option<String>, Error> {
        Ok(self
            .sh
            .memory
            .file_text(&ka_agent_memory::MemoryFile::Facts)?)
    }

    /// `/memory review`: pending candidates.
    pub fn pending_candidates(&self) -> Result<Vec<ka_agent_store::CandidateRow>, Error> {
        Ok(self.sh.store.pending_candidates_full()?)
    }

    /// `/memory accept <id>`: candidate → stable memory. The id may be a
    /// unique prefix (like `/cron rm`).
    /// `/memory log`: the most recent memory commits, newest first.
    pub fn memory_log(&self, n: usize) -> Result<Vec<(String, String, String)>, Error> {
        self.sh.memory.log(n).map_err(Error::Memory)
    }

    /// `/memory undo`: revert the most recent memory commit and recompile
    /// MEMORY.md. Returns the reverted commit's subject.
    pub fn memory_undo(&self) -> Result<String, Error> {
        let subject = self.sh.memory.undo_last().map_err(Error::Memory)?;
        self.refresh_compiled_memory()?;
        self.audit(audit_actions::MEMORY_ACCEPT, "undo", Some(subject.clone()))
            .ok();
        Ok(subject)
    }

    pub fn accept_candidate(&self, id: &str) -> Result<bool, Error> {
        let pending = self.sh.store.pending_candidates_full()?;
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
        let mem_id = self.sh.memory.add(file, &row.text, &row.source, None)?;
        self.refresh_compiled_memory()?;
        self.sh
            .memory
            .commit(&format!("memory: accept {mem_id} (candidate)"))?;
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
        let pending = self.sh.store.pending_candidates_full()?;
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
        Ok(self.sh.store.search_episodes(query, limit)?)
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
        self.sh.memory.install_compiled(
            &self.sh.paths.engine_home().join(".config/ka"),
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
    /// cycles with their answers.
    pub fn recent_turns(&self, limit: usize) -> Result<Vec<(ka_agent_core::Turn, String)>, Error> {
        let sid = self.session_id();
        let turns = self.sh.store.latest_turns_for_session(&sid, limit)?;
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
