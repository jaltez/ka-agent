//! Server-rendered HTML pages. All user-derived text goes through
//! [`escape_html`](crate::escape_html). Mutations are POST-only.

use crate::{Response, escape_html};
use ka_agent_core::ApprovalDecision;
use ka_agent_runtime::Runtime;
use ka_agent_store::Store;

const STYLE: &str = "<style>body{font-family:sans-serif;max-width:800px;margin:2em auto;padding:0 1em}\\
         pre{background:#f4f4f4;padding:1em;overflow-x:auto}\\
         table{border-collapse:collapse;width:100%}th,td{border:1px solid #ddd;padding:0.4em;text-align:left}\\
         .card{border:1px solid #ddd;border-radius:4px;padding:1em;margin:1em 0}\\
         .card button{margin:0 0.5em}\\
         nav a{margin-right:0.8em}\\
         .muted{color:#666}</style>";

/// The viewport-aware head + top navigation. The current page is bold.
fn html_page(title: &str, body: String) -> String {
    format!(
        "<!DOCTYPE html>\n<html><head><title>{title} — Ka Agent</title>\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">{STYLE}</head><body>\
         <h1>{title}</h1>{body}</body></html>"
    )
}

/// A page with the shared navigation (approvals count from the runtime).
fn nav_page(runtime: &Runtime, active: &str, title: &str, body: String) -> Response {
    let count = runtime
        .pending_approval_cards()
        .map(|c| c.len())
        .unwrap_or(0);
    let items = [
        ("/", "Chat"),
        ("/status", "Status"),
        ("/tasks", "Tasks"),
        ("/memory", "Memory"),
        ("/skills", "Skills"),
        ("/approvals", "Approvals"),
        ("/history", "History"),
        ("/contacts", "Contacts"),
        ("/audit", "Audit"),
    ];
    let nav: String = items
        .iter()
        .map(|(href, label)| {
            let label = if *href == "/approvals" {
                format!("{label} ({count})")
            } else {
                (*label).to_string()
            };
            if *href == active {
                format!("<b><a href=\"{href}\">{label}</a></b>")
            } else {
                format!("<a href=\"{href}\">{label}</a>")
            }
        })
        .collect::<Vec<_>>()
        .join(" · ");
    Response::html(format!(
        "<!DOCTYPE html>\n<html><head><title>{title} — Ka Agent</title>\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">{STYLE}</head><body>\
         <nav>{nav}</nav><h1>{title}</h1>{body}</body></html>"
    ))
}

/// `GET /status`: the operator report plus the settings with the CLI that
/// changes each.
pub fn status(runtime: &Runtime) -> crate::Response {
    let report = match runtime.status_report() {
        Ok(r) => r,
        Err(e) => {
            return nav_page(
                runtime,
                "/status",
                "Error",
                format!("<p>{}</p>", escape_html(&e.to_string())),
            );
        }
    };
    let cfg = runtime.config();
    let channels = [
        ("telegram", cfg.telegram.enabled),
        ("matrix", cfg.matrix.enabled),
    ]
    .iter()
    .filter(|&(_n, on)| *on)
    .map(|(n, _on)| {
        format!("<li>{n} — <code>ka-agent config set {n}.enabled false</code> to turn off</li>")
    })
    .collect::<Vec<_>>()
    .join("");
    let body = format!(
        "<pre>{}</pre>\
         <h2>settings</h2><table>\
         <tr><td>model</td><td>{}</td><td><code>ka-agent model &lt;provider/model&gt;</code></td></tr>\
         <tr><td>mode</td><td>{}</td><td><code>ka-agent config set mode &lt;mode&gt;</code></td></tr>\
         <tr><td>timezone</td><td>{}</td><td><code>ka-agent config set timezone \"Europe/Madrid\"</code></td></tr>\
         <tr><td>workspace</td><td>{}</td><td><code>ka-agent config edit</code> (restart)</td></tr>\
         <tr><td>web port</td><td>{}</td><td><code>ka-agent config set web.port &lt;port&gt;</code> (restart)</td></tr>\
         </table><h2>channels</h2><ul>{channels}</ul>",
        escape_html(&report.render(&runtime.now_zoned())),
        escape_html(cfg.model.as_deref().unwrap_or("(none)")),
        escape_html(&cfg.mode),
        escape_html(&report.timezone),
        escape_html(&cfg.workspace.display().to_string()),
        cfg.web.port,
    );
    nav_page(runtime, "/status", "Status", body)
}

/// `GET /`: the chat page (last 20 turns, live updates via /events).
pub fn chat_page(runtime: &Runtime, store: &Store) -> crate::Response {
    let now = runtime.now_zoned();
    let mut turns_html = String::new();
    if let Ok(turns) = runtime.recent_turns(20) {
        for (t, answer) in turns.iter().rev() {
            let at = jiff::Timestamp::from_millisecond(t.created_ms)
                .map(|ts| ts.to_zoned(now.time_zone().clone()))
                .unwrap_or_else(|_| now.clone());
            let time = format!("{:02}:{:02}", at.hour(), at.minute());
            turns_html.push_str(&format!(
                "<div class='card'><div class='muted'>{} · {}</div><p><b>you:</b> {}</p><p><b>ka:</b> {}</p></div>",
                time,
                escape_html(ka_agent_core::ids::short_id(t.id.as_str())),
                escape_html(&t.inbound_text),
                escape_html(answer)
            ));
        }
    }
    let _ = store;
    let body = format!(
        "<div id='log'>{turns_html}</div>\
         <form id='send'><textarea id='text' rows='3' cols='60' placeholder='talk to ka (Enter sends, Shift+Enter newline)'></textarea><br><button>send</button></form>\
         <script src=\"/static/chat.js\" defer></script>"
    );
    nav_page(runtime, "/", "Chat", body)
}

/// `POST /chat/send`: a command or a message from the web chat (fetch).
pub async fn chat_send(runtime: &Runtime, store: &Store, body: &str) -> crate::Response {
    let text = crate::url_decode(&form_field_url(body, "text").replace('+', " "));
    let text = text.trim().to_string();
    if text.is_empty() {
        return Response::bad_request("empty text");
    }
    match ka_agent_channels::commands::route(runtime, "web", &text).await {
        ka_agent_channels::commands::Outcome::Reply(r) => {
            Response::json(serde_json::json!({"reply": r}).to_string())
        }
        ka_agent_channels::commands::Outcome::Silent => {
            Response::json(serde_json::json!({"ok": true}).to_string())
        }
        ka_agent_channels::commands::Outcome::NotACommand => {
            let expanded =
                ka_agent_channels::refs::expand(store, &runtime.workspace(), &text).await;
            match runtime.handle_message(&expanded, "web").await {
                Ok(()) => Response::json(serde_json::json!({"ok": true}).to_string()),
                Err(e) => {
                    Response::json(serde_json::json!({"reply": format!("! {e}")}).to_string())
                }
            }
        }
    }
}

/// Read one field from an urlencoded body WITHOUT decoding (for callers
/// that need the raw value; normal path is [`form_field`]).
fn form_field_url(body: &str, name: &str) -> String {
    for pair in body.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && crate::url_decode(k) == name
        {
            return v.to_string();
        }
    }
    String::new()
}

/// `GET /tasks`: reminders and crons with cancel buttons.
pub fn tasks(runtime: &Runtime) -> crate::Response {
    let now = runtime.now_zoned();
    let rows = match runtime.scheduled_tasks() {
        Ok(r) => r,
        Err(e) => {
            return nav_page(
                runtime,
                "/tasks",
                "Error",
                format!("<p>{}</p>", escape_html(&e.to_string())),
            );
        }
    };
    if rows.is_empty() {
        return nav_page(
            runtime,
            "/tasks",
            "Tasks",
            "<p>no reminders or crons</p>".into(),
        );
    }
    let rows_html: Vec<String> = rows
        .iter()
        .map(|t| {
            let next = t
                .scheduled_at_ms
                .map(|at| escape_html(&ka_agent_core::when::describe(at, &now)))
                .unwrap_or_else(|| "-".into());
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
                 <td><form method=post action=/tasks/cancel>\
                 <input type=hidden name=id value=\"{}\"><button>cancel</button></form></td></tr>",
                escape_html(ka_agent_core::ids::short_id(t.id.as_str())),
                escape_html(&t.kind),
                next,
                escape_html(&t.title),
                escape_html(t.id.as_str())
            )
        })
        .collect();
    nav_page(
        runtime,
        "/tasks",
        "Tasks",
        format!(
            "<table><tr><th>id</th><th>kind</th><th>next</th><th>title</th><th></th></tr>{}</table>",
            rows_html.join("")
        ),
    )
}

pub async fn tasks_cancel(runtime: &Runtime, body: &str) -> crate::Response {
    let id = form_field(body, "id");
    let outcome = match runtime.cancel_scheduled(&id) {
        Ok(o) => o,
        Err(e) => {
            return nav_page(
                runtime,
                "/tasks",
                "Error",
                format!("<p>{}</p>", escape_html(&e.to_string())),
            );
        }
    };
    match outcome {
        ka_agent_runtime::CancelOutcome::Cancelled(_) => Response::see_other("/tasks"),
        ka_agent_runtime::CancelOutcome::NotFound => nav_page(
            runtime,
            "/tasks",
            "Tasks",
            format!("<p>no scheduled task matches {}</p>", escape_html(&id)),
        ),
        ka_agent_runtime::CancelOutcome::Ambiguous(ids) => nav_page(
            runtime,
            "/tasks",
            "Tasks",
            format!(
                "<p>ambiguous — matches: {}</p>",
                escape_html(&ids.join(", "))
            ),
        ),
    }
}

/// `GET /skills`: enable/disable (skills are prompt text, not permissions).
pub fn skills(runtime: &Runtime) -> crate::Response {
    let engine = runtime.engine_home();
    let skills = ka_agent_runtime::skills::Skills::new(&engine);
    let available = skills.available();
    if available.is_empty() {
        return nav_page(
            runtime,
            "/skills",
            "Skills",
            "<p>no skills in the library — put them under ~/.config/ka-agent/skills</p>".into(),
        );
    }
    let enabled: Vec<String> = skills.enabled().into_iter().map(|s| s.name).collect();
    let rows: Vec<String> = available
        .iter()
        .map(|s| {
            let on = enabled.contains(&s.name);
            let (label, value) = if on {
                ("Disable", "0")
            } else {
                ("Enable", "1")
            };
            format!(
                "<tr><td>{}</td><td>{}</td><td><form method=post action=/skills/toggle>\
                 <input type=hidden name=name value=\"{}\"><input type=hidden name=enable value=\"{value}\">\
                 <button>{label}</button></form></td></tr>",
                escape_html(&s.name),
                if on { "enabled" } else { "disabled" },
                escape_html(&s.name)
            )
        })
        .collect();
    nav_page(
        runtime,
        "/skills",
        "Skills",
        format!(
            "<table><tr><th>name</th><th>state</th><th></th></tr>{}</table>",
            rows.join("")
        ),
    )
}

pub fn skills_toggle(runtime: &Runtime, body: &str) -> crate::Response {
    let name = form_field(body, "name");
    let enable = form_field(body, "enable") == "1";
    let skills = ka_agent_runtime::skills::Skills::new(&runtime.engine_home());
    let result = if enable {
        skills.enable(&name)
    } else {
        skills.disable(&name)
    };
    match result {
        Ok(_) => Response::see_other("/skills"),
        Err(e) => nav_page(
            runtime,
            "/skills",
            "Error",
            format!("<p>{}</p>", escape_html(&e.to_string())),
        ),
    }
}

/// `GET /contacts`: linked identities + strangers that messaged the bot.
pub fn contacts(runtime: &Runtime, store: &Store) -> crate::Response {
    let linked = store.list_identities().unwrap_or_default();
    let linked_rows: Vec<String> = linked
        .iter()
        .map(|r| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape_html(&r.channel),
                escape_html(&r.address),
                escape_html(&r.via)
            )
        })
        .collect();
    let strangers = ka_agent_channels::pairing::unauthorized_contacts(store, 20);
    let stranger_rows: Vec<String> = strangers
        .iter()
        .map(|c| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><code>ka-agent identity link {} {}</code></td></tr>",
                escape_html(&c.channel),
                escape_html(&c.address),
                escape_html(&ka_agent_core::when::describe(c.last_ms, &runtime.now_zoned())),
                c.count,
                escape_html(&c.channel),
                escape_html(&c.address)
            )
        })
        .collect();
    let body = format!(
        "<h2>linked</h2>{}\
         <h2>unknown contacts</h2>{}",
        if linked_rows.is_empty() {
            "<p>none — <code>ka-agent pair telegram</code></p>".into()
        } else {
            format!(
                "<table><tr><th>channel</th><th>address</th><th>via</th></tr>{}</table>",
                linked_rows.join("")
            )
        },
        if stranger_rows.is_empty() {
            "<p>no unknown contacts</p>".to_string()
        } else {
            format!(
                "<table><tr><th>channel</th><th>address</th><th>last seen</th><th>attempts</th><th>link</th></tr>{}</table>",
                stranger_rows.join("")
            )
        }
    );
    nav_page(runtime, "/contacts", "Contacts", body)
}

/// `POST /memory/forget`: remove one accepted entry.
pub async fn memory_forget(runtime: &Runtime, body: &str) -> crate::Response {
    let id = form_field(body, "id");
    let _ = runtime.forget(&id);
    Response::see_other("/memory")
}

/// The fetch path of the approval card buttons (chat.js).
pub async fn approvals_decide_json(runtime: &Runtime, body: &str) -> crate::Response {
    let id = form_field(body, "id");
    let decision = match form_field(body, "decision").as_str() {
        "y" => ApprovalDecision::Allow,
        _ => ApprovalDecision::Deny,
    };
    match runtime.decide(&id, decision).await {
        Ok(()) => Response::json(serde_json::json!({"ok": true}).to_string()),
        Err(e) => Response::json(serde_json::json!({"error": e.to_string()}).to_string()),
    }
}

pub fn history(runtime: &Runtime, query: Option<&str>) -> crate::Response {
    let Some(q) = query.filter(|q| !q.is_empty()) else {
        let body = "<form method=get action=/history><input name=q placeholder='search…'><button>search</button></form>".into();
        return Response::html(html_page("History", body));
    };
    // FTS search is available on the runtime
    match runtime.history_search(q, 20) {
        Ok(hits) if hits.is_empty() => {
            let body = format!("<p>no results for {}</p>", escape_html(q));
            Response::html(html_page("History search", body))
        }
        Ok(hits) => {
            let rows: Vec<String> = hits
                .iter()
                .map(|h| {
                    format!(
                        "<tr><td><a href=\"/history/show/{}\">{}</a></td><td>{}</td><td>{}</td></tr>",
                        escape_html(&h.turn),
                        escape_html(ka_agent_core::ids::short_id(&h.turn)),
                        escape_html(&h.role),
                        escape_html(&h.snippet)
                    )
                })
                .collect();
            let body = format!(
                "<table><tr><th>turn</th><th>role</th><th>snippet</th></tr>{}</table>",
                rows.join(
                    "
"
                )
            );
            Response::html(html_page("History search", body))
        }
        Err(e) => Response::html(html_page(
            "Error",
            format!("<p>{}</p>", escape_html(&e.to_string())),
        )),
    }
}

pub fn history_show(store: &Store, turn_id: &str) -> crate::Response {
    let Some(turn) = store
        .get_turn(&ka_agent_core::TurnId(turn_id.into()))
        .ok()
        .flatten()
    else {
        return Response::not_found();
    };
    let chunks = store.chunks_for_turn(&turn.id).ok().unwrap_or_default();
    let answer = chunks.iter().map(|c| c.text.as_str()).collect::<String>();
    let body = format!(
        "<p><b>inbound:</b></p><pre>{}</pre><p><b>answer:</b></p><pre>{}</pre>",
        escape_html(&turn.inbound_text),
        escape_html(&answer)
    );
    Response::html(html_page("Turn", body))
}

pub fn memory(runtime: &Runtime) -> crate::Response {
    let Ok(report) = runtime.memory_report() else {
        return nav_page(runtime, "/memory", "Memory", "<p>memory error</p>".into());
    };
    let files: Vec<String> = report
        .files
        .iter()
        .map(|f| format!("<li>{}: {} bytes</li>", escape_html(&f.name), f.bytes))
        .collect();
    let entries = runtime
        .memory_entries()
        .map(|entries| {
            if entries.is_empty() {
                return "<p>no accepted entries</p>".to_string();
            }
            let rows: Vec<String> = entries
                .iter()
                .take(50)
                .map(|e| {
                    format!(
                        "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td>\\
                         <td><form method=post action=/memory/forget>\\
                         <input type=hidden name=id value=\"{}\"><button>forget</button></form></td></tr>",
                        escape_html(ka_agent_core::ids::short_id(&e.id)),
                        escape_html(&e.file),
                        escape_html(&e.text),
                        escape_html(&e.source),
                        escape_html(&e.id)
                    )
                })
                .collect();
            format!(
                "<table><tr><th>id</th><th>file</th><th>text</th><th>source</th><th></th></tr>{}</table>",
                rows.join("")
            )
        })
        .unwrap_or_else(|e| format!("<p>entries error: {}</p>", escape_html(&e.to_string())));
    let compiled = runtime
        .memory_compiled_text()
        .map(|text| format!("<pre>{}</pre>", escape_html(&text)))
        .unwrap_or_else(|e| format!("<p>compile error: {}</p>", escape_html(&e.to_string())));
    let candidates = render_candidates(runtime);
    let body = format!(
        "<p>memory: {}B / {}B{}</p><ul>{}</ul>\
         <h2>propose an entry</h2>\
         <form method=post action=/memory/edit>\
         <textarea name=text rows=4 cols=60 placeholder=\"one durable fact\"></textarea><br>\
         <button>propose</button></form>\
         {candidates}\
         <h2>accepted entries</h2>{entries}\
         <h2>compiled (injected)</h2>{compiled}",
        report.total_bytes,
        report.budget_bytes,
        if report.truncated { " (truncated)" } else { "" },
        files.join("")
    );
    nav_page(runtime, "/memory", "Memory", body)
}

pub fn memory_edit(runtime: &Runtime, body: &str) -> crate::Response {
    let text = form_field(body, "text");
    if text.trim().is_empty() {
        return Response::html(html_page(
            "Memory",
            "<p>nothing to remember (empty text)</p><p><a href=\"/memory\">back</a></p>".into(),
        ));
    }
    match runtime.propose_memory("web", &text, None) {
        Ok(_id) => {
            // 303 so a refresh does not re-submit the form.
            let mut resp = Response::redirect("/memory");
            resp.status = 303;
            resp
        }
        Err(e) => Response::html(html_page(
            "Error",
            format!("<p>{}</p>", escape_html(&e.to_string())),
        )),
    }
}

/// The review inbox: each candidate with the diff accepting it would apply
/// to its target file, and accept/reject forms (SEED.md §4.4). Accepting
/// appends `- {text}` plus a provenance comment to facts.md
/// (`MemoryStore::add`); the id/timestamp are generated on accept, so the
/// preview marks them.
fn render_candidates(runtime: &Runtime) -> String {
    let rows = runtime.pending_candidates().unwrap_or_default();
    if rows.is_empty() {
        return "<h2>candidates</h2><p>none awaiting review</p>".into();
    }
    let facts = runtime
        .memory_facts_text()
        .ok()
        .flatten()
        .unwrap_or_default();
    let context: String = facts
        .lines()
        .rev()
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|l| format!("  {l}\n"))
        .collect::<String>()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let cards: Vec<String> = rows
        .iter()
        .map(|c| {
            let diff = format!(
                "{context}+ - {}\n+ <!-- id: …; source: {}; accepted: …; valid_until: - -->\n",
                escape_html(&c.text),
                escape_html(&c.source)
            );
            format!(
                "<div class='card'><p><b>{}</b> proposes:</p><pre>{diff}</pre>\
                 <form method=post action=/memory/candidate>\
                 <input type=hidden name=id value=\"{}\">\
                 <button name=decision value=y>Accept</button>\
                 <button name=decision value=n>Reject</button></form></div>",
                escape_html(&c.source),
                escape_html(c.id.as_str())
            )
        })
        .collect();
    format!("<h2>candidates</h2>{}", cards.join(""))
}

/// `POST /memory/candidate`: apply a review decision.
pub fn memory_candidate(runtime: &Runtime, body: &str) -> crate::Response {
    let id = form_field(body, "id");
    let applied = match form_field(body, "decision").as_str() {
        "y" => runtime.accept_candidate(&id),
        _ => runtime.reject_candidate(&id),
    };
    match applied {
        // Ok(false): unknown or already-settled candidate — say so instead of
        // silently reporting success.
        Ok(true) => {
            let mut resp = Response::redirect("/memory");
            resp.status = 303;
            resp
        }
        Ok(false) => Response::html(html_page(
            "Memory",
            format!(
                "<p>no pending candidate {}: it may already be settled</p>\
                 <p><a href=\"/memory\">back</a></p>",
                escape_html(&id)
            ),
        )),
        Err(e) => Response::html(html_page(
            "Memory",
            format!(
                "<p>decision not applied: {}</p><p><a href=\"/memory\">back</a></p>",
                escape_html(&e.to_string())
            ),
        )),
    }
}

/// Read one field from an `application/x-www-form-urlencoded` body,
/// percent-decoding it. Unknown fields are ignored.
fn form_field(body: &str, name: &str) -> String {
    for pair in body.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && crate::url_decode(k) == name
        {
            return crate::url_decode(&v.replace('+', " "));
        }
    }
    String::new()
}

pub fn approvals(runtime: &Runtime, store: &Store) -> crate::Response {
    let sid = runtime.session_id();
    let pending = store.pending_approvals(&sid).unwrap_or_default();
    if pending.is_empty() {
        return nav_page(
            runtime,
            "/approvals",
            "Approvals",
            "<p>no pending approvals</p>".into(),
        );
    }
    let cards: Vec<String> = pending
        .iter()
        .filter_map(|a| {
            let ka_agent_core::ApprovalKind::KaAsk {
                question, detail, ..
            } = &a.kind
            else {
                return None;
            };
            let q = escape_html(question);
            let d = detail
                .as_deref()
                .map(|d| {
                    let escaped = escape_html(&d.lines().take(20).collect::<Vec<_>>().join("\n"));
                    format!("<pre>{escaped}</pre>")
                })
                .unwrap_or_default();
            // Same expiry rendering as the chat card (operator zone).
            let expires =
                ka_agent_core::fmt_interval(a.expires_at_ms - ka_agent_core::now_ms());
            Some(format!(
                "<div class='card'><p>{q}</p>{d}<p>expires in {expires}</p>\
                 <form method=post action=/approvals/decide>\
                 <input type=hidden name=id value=\"{}\"><button name=decision value=y>Allow</button>\
                 <button name=decision value=n>Deny</button></form></div>",
                escape_html(a.id.as_str())
            ))
        })
        .collect();
    nav_page(runtime, "/approvals", "Approvals", cards.join(""))
}

pub async fn approvals_decide(runtime: &Runtime, body: &str) -> crate::Response {
    let id = form_field(body, "id");
    let decision = match form_field(body, "decision").as_str() {
        "y" => ApprovalDecision::Allow,
        _ => ApprovalDecision::Deny,
    };
    match runtime.decide(&id, decision).await {
        Ok(()) => Response::redirect("/approvals"),
        Err(e) => Response::html(html_page(
            "Approvals",
            format!(
                "<p>decision not applied: {}</p><p><a href=\"/approvals\">back</a></p>",
                escape_html(&e.to_string())
            ),
        )),
    }
}

pub fn audit(runtime: &Runtime, store: &Store) -> crate::Response {
    let records = store.recent_audit(50).unwrap_or_default();
    if records.is_empty() {
        return Response::html(html_page("Audit", "<p>no audit records</p>".into()));
    }
    let rows: Vec<String> = records
        .iter()
        .map(|r| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                r.seq,
                {
                    let now = runtime.now_zoned();
                    let at = jiff::Timestamp::from_millisecond(r.at_ms)
                        .map(|t| t.to_zoned(now.time_zone().clone()))
                        .unwrap_or_else(|_| now.clone());
                    format!(
                        "{:04}-{:02}-{:02} {:02}:{:02}",
                        at.year(),
                        at.month() as u8,
                        at.day(),
                        at.hour(),
                        at.minute()
                    )
                },
                escape_html(&r.actor),
                escape_html(&r.action),
                escape_html(&r.subject)
            )
        })
        .collect();
    let body = format!(
        "<table><tr><th>seq</th><th>at</th><th>actor</th><th>action</th><th>subject</th></tr>{}</table>",
        rows.join("")
    );
    Response::html(html_page("Audit", body))
}

pub fn intents(store: &Store) -> crate::Response {
    let intents = store.recent_intents(20).unwrap_or_default();
    let rows: Vec<String> = intents
        .iter()
        .map(|i| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{:?}</td><td>{}</td></tr>",
                escape_html(&i.id),
                escape_html(&i.tool),
                i.status,
                escape_html(i.summary.as_deref().unwrap_or(""))
            )
        })
        .collect();
    let body = format!(
        "<table><tr><th>id</th><th>tool</th><th>status</th><th>summary</th></tr>{}</table>",
        rows.join("")
    );
    Response::html(html_page("Intents", body))
}
