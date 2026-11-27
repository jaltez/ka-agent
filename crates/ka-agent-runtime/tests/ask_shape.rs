//! Asks are answered by their shape (core `is_permission_ask`): only the
//! engine's fixed `allow / always / deny` triple is a permission ask. Any
//! other ask on the same `ask` event — the engine's `continue / stop`
//! session guards, an MCP elicitation — is decided card by card on its own
//! options: allow takes the first, deny answers the decline (the last),
//! and no grant (`always`, allow-for-session, a durable rule) ever
//! answers it.
//!
//! A scripted worker speaks the wire here (the engine has no seam for an
//! ask with arbitrary options): it poses the asks a file lists, one per
//! prompt, and logs every line it reads after an ask — so the test reads
//! exactly the index the engine would have got.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ka_agent_core::approval::NOT_A_PERMISSION_ASK;
use ka_agent_core::{ApprovalDecision, DaemonConfig, Principal, SessionStatus};
use ka_agent_port::WorkerPort;
use ka_agent_runtime::event::AgentEvent;
use ka_agent_runtime::surface::Surface;
use ka_agent_runtime::{Envelope, Paths, Runtime};
use ka_agent_store::Store;

/// Everything the runtime told the surface, in order.
#[derive(Default)]
struct Recorder(Mutex<Vec<AgentEvent>>);

impl Recorder {
    /// The approval cards so far: (id, question, expiry).
    fn cards(&self) -> Vec<(String, String, i64)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Approval {
                    id,
                    question,
                    expires_at_ms,
                    ..
                } => Some((id.clone(), question.clone(), *expires_at_ms)),
                _ => None,
            })
            .collect()
    }
}

impl Surface for Recorder {
    fn emit(&self, env: &Envelope) {
        self.0.lock().unwrap().push(env.ev.clone());
    }
}

fn op() -> Principal {
    Principal::operator()
}

/// Poll until `probe` has something; 30 s is the whole budget.
async fn wait_for<T>(tag: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(found) = probe() {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {tag}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The engine's question. The same text carries a permission ask and an
/// ask with its own options: allow-for-session keyed on that text must
/// answer the first and stand aside for the second.
const QUESTION: &str = "Pick a colour";

/// One `ask` event as the worker forwards it: the first question's text
/// and options, verbatim. Proven against the protocol type before it is
/// scripted — a line the port cannot decode is dropped in silence.
fn ask(id: &str, options: &[&str]) -> String {
    let event = serde_json::json!({
        "ev": "ka",
        "event": {
            "type": "ask",
            "id": id,
            "questions": [{"text": QUESTION, "options": options}],
        }
    });
    ka_agent_port::proto::from_line::<ka_agent_port::WorkerEvent>(&event.to_string())
        .unwrap_or_else(|e| panic!("not a worker event: {e}"));
    event.to_string()
}

/// A worker that poses the asks of `asks` — one JSON line per prompt, in
/// order — and appends to `answers` the next line it reads after each.
/// Declared as a scripted speaker: that is what lets a worker without a
/// model take turns (`Runtime::preflight`); the script ignores the flag.
#[cfg(unix)]
fn ask_port(root: &Path, asks: &Path, answers: &Path) -> WorkerPort {
    let wrapper = root.join("ask.sh");
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' '{{"ev":"ready"}}'
n=0
while IFS= read -r line; do
  case "$line" in
    *'"op":"open"'*) printf '%s\n' '{{"ev":"strand","id":"strand_test","path":null}}' ;;
    *'"op":"submit"'*)
      n=$((n + 1))
      sed -n "${{n}}p" '{}'
      IFS= read -r answer
      printf '%s\n' "$answer" >> '{}'
      printf '%s\n' '{{"ev":"ka","event":{{"type":"turn_finished","stop":"aborted","usage":{{"input":1,"output":1,"cache_read":0,"cache_write":0,"cost":0.0}}}}}}'
      printf '%s\n' '{{"ev":"ka","event":{{"type":"idle"}}}}'
      ;;
    *'"op":"close"'*) exit 0 ;;
  esac
done
"#,
        asks.display(),
        answers.display()
    );
    std::fs::write(&wrapper, script).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    WorkerPort {
        binary: wrapper,
        scenario_override: Some(asks.to_path_buf()),
    }
}

struct Stack {
    rt: Runtime,
    store: Arc<Store>,
    rec: Arc<Recorder>,
    answers: PathBuf,
    root: PathBuf,
}

impl Stack {
    /// Everything the worker logged after its asks.
    fn answers(&self) -> String {
        std::fs::read_to_string(&self.answers).unwrap_or_default()
    }

    /// The one line the worker got for engine ask `ask_id`.
    fn answer_for(&self, ask_id: &str) -> Option<String> {
        let needle = format!(r#""ask_id":"{ask_id}""#);
        self.answers()
            .lines()
            .find(|l| l.contains(&needle))
            .map(str::to_string)
    }
}

/// The runtime, a 1 s approval TTL, and a worker that poses: a permission
/// ask, then the same question with its own options three times (the
/// expiry, a deny by hand, an allow by hand).
#[cfg(unix)]
fn stack() -> Stack {
    let root = std::env::temp_dir().join(format!("ka-ask-shape-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let asks = root.join("asks.jsonl");
    // One line per ask, the last one newline-terminated too: the script's
    // `sed` prints an unterminated final line without a newline, and the
    // port's reader waits for one.
    std::fs::write(
        &asks,
        [
            ask("ask_1", &["allow", "always", "deny"]),
            ask("ask_2", &["a", "b", "c", "decline"]),
            ask("ask_3", &["a", "b", "c", "decline"]),
            ask("ask_4", &["a", "b", "c", "decline"]),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    let answers = root.join("answers.jsonl");
    let store = Arc::new(Store::open(&root.join("state.db")).unwrap());
    let rec = Arc::new(Recorder::default());
    let cfg = DaemonConfig {
        workspace: root.join("ws"),
        sandbox: "off".into(),
        approval_ttl_secs: 1,
        ..DaemonConfig::default()
    };
    let rt = Runtime::new(
        cfg,
        Paths::new(root.clone()),
        Arc::clone(&store),
        ask_port(&root, &asks, &answers),
        Arc::clone(&rec) as Arc<dyn Surface>,
    )
    .unwrap();
    Stack {
        rt,
        store,
        rec,
        answers,
        root,
    }
}

/// An ask with its own options is answered by them: a session-allow on the
/// same question text does not pre-answer it, no grant applies to it, its
/// expiry sends the decline (the last index — 2 would accept `c`), and so
/// does a deny by hand; an allow by hand takes the first option.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ask_with_its_own_options_is_answered_by_them() {
    let s = stack();
    let idle = || (s.rt.session_status_for(&op()) == SessionStatus::Idle).then_some(());

    // A permission ask: allow-for-session is a grant on it, and it answers
    // choice 0.
    s.rt.handle_message("first", "console").await.unwrap();
    let first = wait_for("the permission card", || {
        s.rec.cards().first().map(|c| c.0.clone())
    })
    .await;
    s.rt.decide_for_session(&first, "console").await.unwrap();
    wait_for("the first turn to settle", idle).await;
    let line = s.answer_for("ask_1").unwrap();
    assert!(line.contains(r#""choice":0"#), "{line}");

    // The same question text, now with its own options: the session-allow
    // stands aside — a card, and nothing answered yet.
    s.rt.handle_message("second", "console").await.unwrap();
    let second = wait_for("the card for the ask with its own options", || {
        s.rec.cards().get(1).map(|c| c.0.clone())
    })
    .await;
    let (_, question, expires_at_ms) = s.rec.cards()[1].clone();
    assert!(
        question.starts_with(&format!("{QUESTION}\noptions: a · b · c · decline")),
        "the card must show the ask's own options: {question}"
    );
    assert_eq!(
        s.rt.session_status_for(&op()),
        SessionStatus::AwaitingApproval
    );
    assert!(
        s.answer_for("ask_2").is_none(),
        "the session-allow pre-answered an ask with its own options: {}",
        s.answers()
    );

    // No grant applies to it: `always`, allow-for-session and the Always
    // decision are all refused, with a reason, and nothing is stored.
    for refused in [
        s.rt.decide_always(&second, "console").await.unwrap_err(),
        s.rt.decide_for_session(&second, "console")
            .await
            .unwrap_err(),
        s.rt.decide(&second, ApprovalDecision::Always, "console")
            .await
            .unwrap_err(),
    ] {
        assert_eq!(refused.to_string(), NOT_A_PERMISSION_ASK);
    }
    assert!(
        s.rt.rules_list(&op()).unwrap().is_empty(),
        "a rule was minted from an ask with its own options"
    );
    assert!(
        s.answer_for("ask_2").is_none(),
        "a refused grant still answered: {}",
        s.answers()
    );

    // Its TTL lapses: the decline, the ask's own last option.
    while ka_agent_core::now_ms() <= expires_at_ms {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let expired = s.rt.expire_approvals().await.unwrap();
    assert_eq!(
        expired.len(),
        1,
        "only the ask with its own options was pending"
    );
    assert_eq!(expired[0].as_str(), second, "the wrong approval expired");
    let line = s
        .answer_for("ask_2")
        .unwrap_or_else(|| panic!("the expiry never reached the worker: {}", s.answers()));
    assert!(line.contains(r#""choice":3"#), "not the decline: {line}");
    wait_for("the second turn to settle", idle).await;

    // A deny by hand is the same decline; an allow takes the first option,
    // and the audit row names the value that accepted.
    s.rt.handle_message("third", "console").await.unwrap();
    let third = wait_for("the third card", || {
        s.rec.cards().get(2).map(|c| c.0.clone())
    })
    .await;
    s.rt.decide(&third, ApprovalDecision::Deny, "console")
        .await
        .unwrap();
    wait_for("the third turn to settle", idle).await;
    let line = s.answer_for("ask_3").unwrap();
    assert!(line.contains(r#""choice":3"#), "not the decline: {line}");

    s.rt.handle_message("fourth", "console").await.unwrap();
    let fourth = wait_for("the fourth card", || {
        s.rec.cards().get(3).map(|c| c.0.clone())
    })
    .await;
    s.rt.decide(&fourth, ApprovalDecision::Allow, "console")
        .await
        .unwrap();
    wait_for("the fourth turn to settle", idle).await;
    let line = s.answer_for("ask_4").unwrap();
    assert!(
        line.contains(r#""choice":0"#),
        "not the first option: {line}"
    );
    let audit = s.store.recent_audit(50).unwrap();
    assert!(
        audit.iter().any(|row| {
            row.action == "approval.decide"
                && row.subject == fourth
                && row.detail.as_deref() == Some("Allow (first option: \"a\")")
        }),
        "the audit row must name the accepted option: {audit:?}"
    );

    let _ = std::fs::remove_dir_all(&s.root);
}
