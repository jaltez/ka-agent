//! Strand files read from the daemon side — no engine involved: the HTML
//! export of one session (`ka-agent export --html`) and the retention
//! sweep of the strand store (`retention.strands_days`). Both are thin
//! wrappers over `ka_strand`, so no ka type crosses the boundary.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// `ka_strand::gc`'s age threshold for snapshot journals, set so that no
/// journal is ever "aged out" from under a session that still lives: the
/// engine undoes and diffs a session from its journal for as long as
/// that session's strand exists, so here a journal goes only as an
/// orphan — once no strand is left for its working directory.
const JOURNALS_NEVER_AGE_OUT_DAYS: u64 = u64::MAX;

/// Render one strand file as a self-contained HTML page — the page `ka
/// export --html` writes: the transcript as Markdown inside an inert
/// island, a vendored renderer, no external asset, titled with the
/// strand's display title. `Err` names the file: a strand that is not on
/// disk (yet), or a file that is not a strand.
pub fn export_html(strand: &Path) -> Result<String, String> {
    if !strand.is_file() {
        return Err(format!("{}: no strand file on disk yet", strand.display()));
    }
    let file =
        ka_strand::StrandFile::open(strand).map_err(|e| format!("{}: {e}", strand.display()))?;
    let records = file.records();
    Ok(ka_strand::render_html(
        &ka_strand::title_of(records),
        &ka_strand::render_markdown(records),
    ))
}

/// What one strand sweep removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StrandGc {
    /// Strand files deleted — the sweep's own step: `ka_strand::gc`
    /// never deletes a strand (`ka session delete` owns that upstream).
    pub strands: usize,
    /// Snapshot journals deleted. `<data>/snapshots/<cwd-key>/` holds the
    /// pre-mutation originals `/undo` restores and `/diff` diffs.
    pub journals: u64,
    /// Strand directories left empty.
    pub empty_dirs: u64,
}

/// Reclaim one user's strand store (`data_root` is its `KA_DATA_DIR`).
/// Every strand file modified before `cutoff_ms` (ms since the epoch) is
/// deleted unless `keep` claims it; then `ka_strand::gc` sweeps what the
/// deletions leave behind — snapshot journals no strand needs any more,
/// and empty strand directories. `keep` sees only aged strands.
///
/// What `keep` claims is the caller's rule (the daemon: a session with a
/// live worker, an open turn or a retained turn keeps its strand); the
/// sweep itself knows files, not sessions. A deletion failure propagates
/// after the strands removed so far are gone: the next pass finishes.
pub fn gc_strands(
    data_root: &Path,
    cutoff_ms: i64,
    keep: &dyn Fn(&Path) -> bool,
) -> std::io::Result<StrandGc> {
    let mut out = StrandGc::default();
    let mut strands = Vec::new();
    collect_strands(&data_root.join("strands"), &mut strands)?;
    for path in strands {
        let old = modified_ms(&path).is_some_and(|mtime| mtime < cutoff_ms);
        if old && path.is_file() && !keep(&path) {
            std::fs::remove_file(&path)?;
            out.strands += 1;
        }
    }
    let report = ka_strand::gc(data_root, &[], JOURNALS_NEVER_AGE_OUT_DAYS, false)?;
    out.journals = report.snapshots_removed;
    out.empty_dirs = report.empty_dirs_removed;
    Ok(out)
}

/// Every `*.jsonl` under `dir`, at any depth (the store keeps them at
/// `strands/<cwd-key>/`). A missing directory is empty; a symlinked
/// directory is not followed, so the sweep stays inside the store.
fn collect_strands(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect_strands(&path, out)?,
            Ok(_) if path.extension().is_some_and(|e| e == "jsonl") => out.push(path),
            _ => {}
        }
    }
    Ok(())
}

/// A file's mtime in ms since the Unix epoch; `None` when it cannot be
/// read or mapped, and such a file is never "old".
fn modified_ms(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let millis = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_millis();
    Some(i64::try_from(millis).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use ka_protocol::{RecordId, StrandId};
    use ka_strand::{Record, Role};

    fn tmp(tag: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ka-strand-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn strand_file(dir: &Path) -> PathBuf {
        dir.join("strands").join("-any").join("s-a.jsonl")
    }

    /// A strand with a header and two messages, as the engine writes it.
    fn write_strand(path: &Path, cwd: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let records = [
            Record::Header {
                id: StrandId("s-export".into()),
                ts: "2026-11-27T10:00:00Z".into(),
                cwd: cwd.into(),
                version: 1,
                repo: None,
                parent: None,
            },
            Record::Message {
                id: RecordId("r1".into()),
                role: Role::User,
                content: "what <b>is</b> the plan?".into(),
                calls: Vec::new(),
                results: Vec::new(),
                thinking: None,
                images: Vec::new(),
            },
            Record::Message {
                id: RecordId("r2".into()),
                role: Role::Assistant,
                content: "ship **it**".into(),
                calls: Vec::new(),
                results: Vec::new(),
                thinking: None,
                images: Vec::new(),
            },
        ];
        let text: String = records
            .iter()
            .map(|r| format!("{}\n", serde_json::to_string(r).unwrap()))
            .collect();
        std::fs::write(path, text).unwrap();
    }

    /// Backdate a file's mtime (the sweep's only notion of age).
    fn age(path: &Path, days_ago: u64) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        let then = SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(days_ago * 86_400))
            .unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(then))
            .unwrap();
    }

    #[test]
    fn export_html_is_a_self_contained_page_of_the_records() {
        let dir = tmp("export");
        let path = strand_file(&dir);
        write_strand(&path, "/w");
        let page = export_html(&path).unwrap();
        assert!(page.starts_with("<!doctype html>"), "{page}");
        // The title is the strand's (its first user message), the
        // transcript rides the inert Markdown island, and the renderer is
        // vendored: nothing is fetched.
        assert!(
            page.contains("<title>what &lt;b&gt;is&lt;/b&gt; the plan?</title>"),
            "{page}"
        );
        assert!(
            page.contains(r#"<script type="text/markdown" id="ka-md">"#),
            "{page}"
        );
        assert!(page.contains("**you**"), "{page}");
        assert!(page.contains("ship **it**"), "{page}");
        assert!(
            page.contains("function esc("),
            "the vendored renderer: {page}"
        );
        assert!(!page.contains("src=\"http"), "no external asset: {page}");
    }

    #[test]
    fn export_html_refuses_a_missing_or_foreign_file() {
        let dir = tmp("export-missing");
        let missing = dir.join("nowhere.jsonl");
        let err = export_html(&missing).unwrap_err();
        assert!(err.contains("no strand file on disk yet"), "{err}");
        assert!(err.contains("nowhere.jsonl"), "{err}");
        let foreign = dir.join("notes.jsonl");
        std::fs::write(&foreign, "{\"not\":\"a strand\"}\n").unwrap();
        let err = export_html(&foreign).unwrap_err();
        // The file is named and ka-strand's own parse error is carried.
        assert!(err.contains("notes.jsonl"), "{err}");
        assert!(err.contains("malformed record"), "{err}");
    }

    #[test]
    fn old_strands_go_fresh_and_kept_ones_stay() {
        let dir = tmp("gc");
        let cutoff = ka_agent_core::now_ms() - 10 * 86_400_000;
        let old = strand_file(&dir);
        write_strand(&old, "/w/old");
        age(&old, 30);
        let fresh = old.with_file_name("s-fresh.jsonl");
        write_strand(&fresh, "/w/old");
        age(&fresh, 1);
        let claimed = old.with_file_name("s-claimed.jsonl");
        write_strand(&claimed, "/w/old");
        age(&claimed, 30);
        let gc = gc_strands(&dir, cutoff, &|p| p == claimed).unwrap();
        assert_eq!(gc.strands, 1, "{gc:?}");
        assert!(!old.exists());
        assert!(fresh.exists(), "a fresh strand is never touched");
        assert!(claimed.exists(), "a claimed strand is never touched");
    }

    #[test]
    fn a_journal_follows_the_last_strand_of_its_directory() {
        let dir = tmp("gc-journal");
        let cutoff = ka_agent_core::now_ms() - 10 * 86_400_000;
        // The engine's snapshot key for a working directory.
        let journal = |cwd: &str| {
            dir.join("snapshots")
                .join(format!("-{}", cwd.replace('/', "-")))
        };
        let (gone_cwd, kept_cwd) = ("/w/gone", "/w/kept");
        for cwd in [gone_cwd, kept_cwd] {
            let strand = dir
                .join("strands")
                .join("-x")
                .join(format!("s-{}.jsonl", cwd.replace('/', "-")));
            write_strand(&strand, cwd);
            age(&strand, 30);
            std::fs::create_dir_all(journal(cwd)).unwrap();
            std::fs::write(journal(cwd).join("manifest.jsonl"), "{}\n").unwrap();
        }
        // Only the first directory's strand may go: its journal is an
        // orphan afterwards. The other strand stays, and so does the
        // journal it still undoes and diffs from — aged or not.
        let gc = gc_strands(&dir, cutoff, &|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().contains("kept"))
        })
        .unwrap();
        assert_eq!(
            gc,
            StrandGc {
                strands: 1,
                journals: 1,
                empty_dirs: 0
            }
        );
        assert!(!journal(gone_cwd).exists(), "an orphaned journal goes");
        assert!(
            journal(kept_cwd).exists(),
            "a kept strand keeps its journal"
        );
    }

    #[test]
    fn an_empty_store_sweeps_nothing() {
        let dir = tmp("gc-empty");
        assert_eq!(
            gc_strands(&dir, i64::MAX, &|_| false).unwrap(),
            StrandGc::default()
        );
    }
}
