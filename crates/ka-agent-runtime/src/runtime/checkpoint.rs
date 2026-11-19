//! Shadow-git checkpoints (B1): one workspace snapshot per Allow-settled
//! write, restorable with `/rewind`. The repo lives under the user's
//! `checkpoints/<short session id>` partition and never touches the
//! workspace's own `.git`; every operation goes through
//! `--git-dir/--work-tree` (the bounded-invocation shape of the memory
//! crate's git backing).
//!
//! Failure policy: a failed snapshot (disk full, git absent) never
//! vetoes an allowed write — the gap is audited (`checkpoint.failed`),
//! recorded as a sha-less row, and shows as a hole in `/rewind`. The
//! approvals are the security boundary; the checkpoint is the net.

use std::path::Path;

use super::*;
use ka_agent_core::ids::short_id;
use ka_agent_store::CheckpointRow;

/// Fixed identity for snapshot commits (like the memory crate's).
const GIT_NAME: &str = "ka-agent checkpoint";
const GIT_EMAIL: &str = "checkpoint@ka-agent.invalid";

/// One bounded git invocation against the shadow repo.
fn git(git_dir: &Path, work_tree: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        // cwd = the worktree, so pathspecs like `.` in `checkout -- .`
        // resolve against it, not the daemon's own cwd.
        .current_dir(work_tree)
        .arg("--git-dir")
        .arg(git_dir)
        .arg("--work-tree")
        .arg(work_tree)
        .args(args)
        // The workspace's own git environment must not leak in.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .map_err(|e| format!("git {}: {e}", args[0]))?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Create the shadow repo on first use and pin its worktree. Idempotent.
fn ensure_repo(git_dir: &Path, work_tree: &Path) -> Result<(), String> {
    if !git_dir.join("HEAD").exists() {
        std::fs::create_dir_all(git_dir).map_err(|e| e.to_string())?;
        // `git init` does not honor the `--git-dir` flag — pass the
        // target as its argument (bare; the worktree is pinned via
        // `core.worktree` below).
        let out = std::process::Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(git_dir)
            .output()
            .map_err(|e| format!("git init: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git init: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    git(
        git_dir,
        work_tree,
        &["config", "core.worktree", &work_tree.to_string_lossy()],
    )?;
    Ok(())
}

/// `add -A` + commit; returns the commit sha. `--allow-empty` keeps the
/// first snapshot of an empty workspace a real, restorable tree. The
/// worktree's own `.gitignore`s apply (`target/`-class trees stay out).
fn snapshot(git_dir: &Path, work_tree: &Path) -> Result<String, String> {
    ensure_repo(git_dir, work_tree)?;
    git(git_dir, work_tree, &["add", "-A", "."])?;
    git(
        git_dir,
        work_tree,
        &[
            "-c",
            &format!("user.name={GIT_NAME}"),
            "-c",
            &format!("user.email={GIT_EMAIL}"),
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "checkpoint",
        ],
    )?;
    let sha = git(git_dir, work_tree, &["rev-parse", "HEAD"])?;
    Ok(sha.trim().to_string())
}

/// Restore one snapshot into the worktree: `checkout -f <sha> -- .` and
/// nothing else — no `git clean`, so files created after the checkpoint
/// are left alone (documented limit, kept deliberate).
fn restore(git_dir: &Path, work_tree: &Path, sha: &str) -> Result<(), String> {
    ensure_repo(git_dir, work_tree)?;
    git(git_dir, work_tree, &["checkout", "-f", sha, "--", "."])?;
    Ok(())
}

impl Runtime {
    /// The checkpoint net at the single Allow-settle site: snapshot the
    /// acting user's workspace before the write this Allow releases. The
    /// engine is blocked on the ask, so the workspace is quiescent
    /// (ka-port §4) and the snapshot cannot race the write it precedes.
    /// A failed snapshot never vetoes the write.
    pub(in crate::runtime) fn checkpoint_before_allow(
        &self,
        who: &ka_agent_core::Principal,
        session: &Session,
        turn: &TurnId,
        approval: Option<&Approval>,
    ) {
        if !self.cfg().checkpoint.enabled {
            return;
        }
        let question = approval.and_then(|a| match &a.kind {
            ApprovalKind::KaAsk { question, .. } => Some(question.clone()),
            _ => None,
        });
        let git_dir = self
            .sh
            .paths
            .user_checkpoints(&session.username)
            .join(short_id(session.id.as_str()));
        let sha = snapshot(&git_dir, &session.workspace).ok();
        let created_ms = ka_agent_core::now_ms();
        let row = CheckpointRow {
            id: format!("{}/{}", turn.0, created_ms),
            session: session.id.clone(),
            turn: turn.clone(),
            username: session.username.clone(),
            approval: approval.map(|a| a.id.0.clone()),
            question,
            sha: sha.clone(),
            created_ms,
        };
        if let Err(e) = self.sh.store.insert_checkpoint(&row) {
            // A row we could not record is itself a hole in the net.
            let mut rec = AuditRecord::new(
                "runtime",
                "checkpoint.failed",
                &row.id,
                Some(format!("row persist: {e}")),
            )
            .with_user(&who.username);
            let _ = self.sh.store.append_audit(&mut rec);
            return;
        }
        if sha.is_none() {
            let mut rec = AuditRecord::new(
                "runtime",
                "checkpoint.failed",
                &row.id,
                Some(format!(
                    "snapshot of {} failed — the allowed write ran without a net",
                    short_id(session.id.as_str())
                )),
            )
            .with_user(&who.username);
            let _ = self.sh.store.append_audit(&mut rec);
        }
    }

    /// `/rewind` (B1): with no argument, this session's checkpoints,
    /// newest first; with `<n>`, restore that snapshot (code only — no
    /// `git clean`, no conversation rewind). Refuses while a cycle is
    /// open; the reply carries the stored question and the
    /// checkpointed turn's inbound text so the operator can re-propose.
    pub async fn rewind(
        &self,
        who: &ka_agent_core::Principal,
        n: Option<usize>,
    ) -> Result<String, Error> {
        let Some(st) = self.user_handle(&who.username) else {
            return Err(Error::Refused("no session yet — nothing to rewind".into()));
        };
        let (sid, workspace, status) = {
            let g = st.lock();
            (
                g.session.id.clone(),
                g.session.workspace.clone(),
                g.session.status,
            )
        };
        let rows = self.sh.store.checkpoints_for_session(&sid)?;
        if rows.is_empty() {
            return Ok("no checkpoints".into());
        }
        let hhmm = |ms: i64| -> String {
            jiff::Timestamp::from_millisecond(ms)
                .map(|t| {
                    t.to_zoned(self.cfg().time_zone())
                        .strftime("%H:%M")
                        .to_string()
                })
                .unwrap_or_else(|_| "??:??".into())
        };
        let Some(n) = n else {
            let mut out = String::from("checkpoints (newest first; /rewind <n> restores):");
            for (i, r) in rows.iter().enumerate() {
                let label = match (&r.sha, r.question.as_deref()) {
                    (Some(_), Some(q)) => format!("before {}", q.lines().next().unwrap_or("")),
                    (Some(_), None) => "before an unnamed ask".into(),
                    (None, _) => "✗ snapshot failed (hole — not restorable)".into(),
                };
                out.push_str(&format!(
                    "\n{}. {} · {} (turn {})",
                    i + 1,
                    hhmm(r.created_ms),
                    label,
                    short_id(r.turn.as_str())
                ));
            }
            return Ok(out);
        };
        if matches!(
            status,
            SessionStatus::Running | SessionStatus::AwaitingApproval | SessionStatus::Stopping
        ) {
            return Err(Error::Refused(
                "the session has an open cycle — settle or stop it before rewinding".into(),
            ));
        }
        let Some(row) = rows.get(n.wrapping_sub(1)) else {
            return Err(Error::Refused(format!(
                "no checkpoint {n} — /rewind lists them"
            )));
        };
        let Some(sha) = row.sha.clone() else {
            return Err(Error::Refused(
                "that checkpoint's snapshot failed — nothing to restore".into(),
            ));
        };
        let git_dir = self
            .sh
            .paths
            .user_checkpoints(&row.username)
            .join(short_id(row.session.as_str()));
        restore(&git_dir, &workspace, &sha).map_err(Error::Refused)?;
        let mut rec = AuditRecord::new("runtime", "checkpoint.rewind", &row.id, Some(sha))
            .with_role(&who.role)
            .with_user(&who.username);
        let _ = self.sh.store.append_audit(&mut rec);
        let inbound = self
            .sh
            .store
            .get_turn(&row.turn)?
            .map(|t| t.inbound_text)
            .unwrap_or_default();
        Ok(format!(
            "rewound to {} — before {}\nre-propose: {}",
            hhmm(row.created_ms),
            row.question.as_deref().unwrap_or("an unnamed ask"),
            inbound
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ka-checkpoint-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The memory-crate precedent shape: init, commit, rev-parse — and
    /// the repo is reused, not re-initialized (both trees stay
    /// reachable).
    #[test]
    fn snapshot_commits_and_reuses_the_repo() {
        let ws = tmp("snap");
        let repo = ws.parent().unwrap().join("ck-snap");
        let sha1 = snapshot(&repo, &ws).unwrap();
        std::fs::write(ws.join("a.txt"), "one").unwrap();
        let sha2 = snapshot(&repo, &ws).unwrap();
        assert_ne!(sha1, sha2);
        assert!(git(&repo, &ws, &["cat-file", "-e", &format!("{sha1}^{{tree}}")]).is_ok());
    }

    /// Restore semantics (documented limits, all deliberate): gitignored
    /// files, post-checkpoint files and nested-repo contents survive;
    /// tracked content reverts. The nested repo was recorded as a
    /// contentless gitlink, so restore reverts nothing inside it (and
    /// deletes nothing — checkout-only, no clean).
    #[test]
    fn restore_reverts_tracked_files_and_leaves_the_rest() {
        let ws = tmp("restore");
        let repo = ws.parent().unwrap().join("ck-restore");
        std::fs::write(ws.join("keep.txt"), "v1").unwrap();
        std::fs::write(ws.join(".gitignore"), "ignored.txt\n").unwrap();
        let nested = ws.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&nested)
            .args(["init", "-q"])
            .output()
            .unwrap();
        std::fs::write(nested.join("inside.txt"), "nested content").unwrap();
        // A commit-less nested repo makes the outer `add -A` fatal —
        // give it one commit, as any real clone has.
        std::process::Command::new("git")
            .arg("-C")
            .arg(&nested)
            .args(["add", "inside.txt"])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&nested)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "n",
            ])
            .output()
            .unwrap();
        let sha = snapshot(&repo, &ws).unwrap();
        // Post-checkpoint changes: a tracked edit, a new untracked file,
        // a gitignored file, and content inside the nested repo.
        std::fs::write(ws.join("keep.txt"), "v2").unwrap();
        std::fs::write(ws.join("new.txt"), "after").unwrap();
        std::fs::write(ws.join("ignored.txt"), "ignored").unwrap();
        std::fs::write(nested.join("inside.txt"), "changed").unwrap();
        restore(&repo, &ws, &sha).unwrap();
        assert_eq!(std::fs::read_to_string(ws.join("keep.txt")).unwrap(), "v1");
        // No `git clean`: post-checkpoint and ignored files stay.
        assert_eq!(
            std::fs::read_to_string(ws.join("new.txt")).unwrap(),
            "after"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join("ignored.txt")).unwrap(),
            "ignored"
        );
        // Gitlink content was never captured.
        assert_eq!(
            std::fs::read_to_string(nested.join("inside.txt")).unwrap(),
            "changed"
        );
    }

    /// A failed snapshot (here: the repo path is blocked by a regular
    /// file — deterministic for every user, unlike mode bits under a
    /// privileged test runner) surfaces as an `Err` — the caller's
    /// failure policy turns it into an audit row, never a veto of the
    /// allowed write.
    #[test]
    fn failed_snapshot_surfaces_as_an_error_not_a_panic() {
        let ws = tmp("fail");
        let repo = ws.parent().unwrap().join("ck-fail-blocked");
        std::fs::write(&repo, "not a directory").unwrap();
        let result = snapshot(&repo, &ws);
        assert!(
            result.is_err(),
            "a blocked checkpoint root must fail cleanly"
        );
    }
}
