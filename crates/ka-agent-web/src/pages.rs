//! Server-rendered HTML pages. All user-derived text goes through
//! [`escape_html`](crate::escape_html). Mutations are POST-only.

use crate::{Response, escape_html};
use ka_agent_core::ApprovalDecision;
use ka_agent_runtime::Runtime;
use ka_agent_store::Store;

const STYLE: &str = "<style>\
:root{--bg:#fafafa;--fg:#1a1a1a;--muted:#6b7280;--border:#e5e7eb;--accent:#4f46e5;--accent-soft:#eef2ff;--ok:#047857;--bad:#b91c1c;--warn:#b45309;--surface:#fff;--radius:10px}\
@media(prefers-color-scheme:dark){:root{--bg:#0f1115;--fg:#e6e8ea;--muted:#9aa1ab;--border:#262b33;--accent:#818cf8;--accent-soft:#1c2033;--ok:#34d399;--bad:#f87171;--warn:#fbbf24;--surface:#171a20}}\
*{box-sizing:border-box}\
body{margin:0;font:15px/1.55 system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;background:var(--bg);color:var(--fg)}\
.layout{display:flex;min-height:100vh;max-width:1100px;margin:0 auto}\
aside{width:190px;flex:none;padding:1.2rem .8rem;border-right:1px solid var(--border)}\
aside .brand{font-weight:700;padding:.2rem .6rem 1rem;letter-spacing:.02em}\
aside a{display:flex;justify-content:space-between;gap:.4rem;padding:.42rem .6rem;border-radius:8px;color:var(--fg);text-decoration:none}\
aside a:hover{background:var(--accent-soft)}\
aside a.active{background:var(--accent-soft);font-weight:600}\
aside .badge{background:var(--accent);color:#fff;font-size:.72rem;border-radius:99px;padding:0 .5rem;line-height:1.4}\
main{flex:1;padding:1.4rem 1.6rem;min-width:0}\
h1{font-size:1.35rem;margin:.1rem 0 1.1rem}\
@media(max-width:760px){.layout{flex-direction:column}aside{width:auto;border-right:none;border-bottom:1px solid var(--border);padding:.5rem .6rem}aside .brand{padding:.3rem .6rem}aside a{display:inline-block;margin:.15rem .2rem}main{padding:1rem}}\
.card{background:var(--surface);border:1px solid var(--border);border-radius:var(--radius);padding:.9rem 1rem;margin:.7rem 0}\
button{font:inherit;border:1px solid var(--border);background:var(--surface);color:var(--fg);border-radius:8px;padding:.35rem .9rem;cursor:pointer}\
button:hover{border-color:var(--accent)}\
button.primary{background:var(--accent);border-color:var(--accent);color:#fff}\
.muted{color:var(--muted)}\
pre{background:var(--bg);border:1px solid var(--border);border-radius:8px;padding:.7rem .9rem;overflow-x:auto}\
code{background:var(--bg);border-radius:4px;padding:.05rem .3rem}\
pre code{background:none;padding:0}\
table{border-collapse:collapse;width:100%;font-size:.92rem}\
th,td{border-bottom:1px solid var(--border);padding:.45rem .5rem;text-align:left}\
th{color:var(--muted);font-weight:600}\
#chat{display:flex;flex-direction:column;height:calc(100vh - 8.4rem)}\
#log{flex:1;overflow-y:auto;padding:.4rem 0}\
.bubble{max-width:78%;border-radius:14px;padding:.55rem .9rem;margin:.3rem 0;white-space:pre-wrap;word-wrap:break-word}\
.bubble.user{align-self:flex-end;background:var(--accent);color:#fff;border-bottom-right-radius:4px}\
.bubble.assistant{align-self:flex-start;background:var(--surface);border:1px solid var(--border);border-bottom-left-radius:4px}\
.bubble.system{align-self:center;background:none;color:var(--muted);font-size:.85rem;max-width:100%}\
.bubble.approval{align-self:flex-start;background:var(--surface);border:1px solid var(--warn);max-width:100%}\
#log{display:flex;flex-direction:column}\
.bubble .tool{font-size:.85rem;color:var(--muted);white-space:pre-wrap}\
.bubble.user code{background:rgba(255,255,255,.2)}\
form#send{display:flex;gap:.5rem;padding:.6rem 0}\
#text{flex:1;font:inherit;border:1px solid var(--border);border-radius:10px;background:var(--surface);color:var(--fg);padding:.55rem .8rem}\
.statgrid{display:grid;grid-template-columns:repeat(auto-fit,minmax(210px,1fr));gap:.7rem}\
.statgrid .card{margin:0}\
.statgrid .k{color:var(--muted);font-size:.8rem;text-transform:uppercase;letter-spacing:.04em}\
.statgrid .v{font-size:1.15rem;margin-top:.15rem}\
.ok{color:var(--ok)}.bad{color:var(--bad)}.warn{color:var(--warn)}</style>";

/// The viewport-aware head + sidebar layout (top bar on narrow screens).
fn html_page(title: &str, body: String) -> String {
    format!(
        "<!DOCTYPE html>\n<html><head><title>{title} — {}</title>\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">{STYLE}</head><body>\
         {body}</body></html>",
        persona_name()
    )
}

/// The assistant's display name (`Ka` by default) from the profile's
/// persona file — the same source every other surface reads.
pub fn persona_name() -> String {
    let path = ka_agent_core::layout::base_dir()
        .join("memory")
        .join("persona.md");
    std::fs::read_to_string(path)
        .ok()
        .map(|t| ka_agent_memory::persona::persona_name(&t))
        .unwrap_or_else(|| "Ka".to_string())
}

/// A page with the shared sidebar navigation. Badges: approvals waiting,
/// memory to review.
fn nav_page(runtime: &Runtime, active: &str, title: &str, body: String) -> Response {
    let count = runtime
        .pending_approval_cards()
        .map(|c| c.len())
        .unwrap_or(0);
    let review = runtime.pending_candidates().map(|c| c.len()).unwrap_or(0);
    let items: [(&str, &str, usize); 10] = [
        ("/", "Chat", 0),
        ("/status", "Status", 0),
        ("/tasks", "Tasks", 0),
        ("/memory", "Memory", review),
        ("/skills", "Skills", 0),
        ("/approvals", "Approvals", count),
        ("/history", "History", 0),
        ("/usage", "Usage", 0),
        ("/contacts", "Contacts", 0),
        ("/audit", "Audit", 0),
    ];
    let nav: String = items
        .iter()
        .map(|(href, label, n)| {
            let badge = if *n > 0 {
                format!("<span class=\"badge\">{n}</span>")
            } else {
                String::new()
            };
            let class = if *href == active { "active" } else { "" };
            format!("<a class=\"{class}\" href=\"{href}\">{label}{badge}</a>")
        })
        .collect::<Vec<_>>()
        .join("");
    Response::html(html_page(
        title,
        format!(
            "<div class=\"layout\"><aside><div class=\"brand\">{}</div>{nav}</aside>\
             <main><h1>{title}</h1>{body}</main></div>",
            escape_html(&persona_name())
        ),
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
    let card = |k: &str, v: String| {
        format!("<div class='card'><div class='k'>{k}</div><div class='v'>{v}</div></div>")
    };
    let report_line = |i: usize| {
        report
            .render(&runtime.now_zoned())
            .lines()
            .nth(i)
            .unwrap_or("")
            .to_string()
    };
    let usd = match report.limit_usd {
        Some(cap) => format!("${:.4} / ${cap:.2}", report.spent_usd),
        None => format!("${:.4}", report.spent_usd),
    };
    let settings = settings_form(runtime);
    let body = format!(
        "<div class='statgrid'>\
         {}{}{}{}{}{}\
         </div>\
         <h2>settings</h2>\
         {settings}\
         <h2>channels</h2><ul>{channels}</ul>",
        card("state", escape_html(&report_line(0))),
        card(
            "model",
            escape_html(cfg.model.as_deref().unwrap_or("(none)"))
        ),
        card("today", escape_html(&usd)),
        card(
            "queue / approvals",
            format!("{} / {}", report.queue, report.pending_approvals)
        ),
        card(
            "memory",
            report
                .memory
                .as_ref()
                .map(|m| escape_html(&m.line()))
                .unwrap_or_else(|| "—".into())
        ),
        card(
            "worker",
            if report.worker_running {
                "running".into()
            } else {
                "stopped (starts with your next message)".into()
            }
        ),
    );
    nav_page(runtime, "/status", "Status", body)
}

/// The settings form: every `KEYS` entry marked `web`, its control chosen
/// by kind, the current value read from the config file, and the key's
/// apply timing beside it (plan 3.2: one registry, one form).
fn settings_form(runtime: &Runtime) -> String {
    use ka_agent_core::config_keys::{KEYS, Kind};
    let Some(path) = runtime.config_path() else {
        return "<p class='muted'>no config file to edit</p>".into();
    };
    let mut rows = String::new();
    for spec in KEYS.iter().filter(|k| k.web) {
        let current = ka_agent_core::config_file::get(&path, spec.path)
            .ok()
            .flatten()
            .unwrap_or_default();
        let field = spec.path.replace('.', "_");
        let control = match spec.kind {
            Kind::Bool => format!(
                "<select name='{field}'><option value=''>—</option>{}</select>",
                ["true", "false"]
                    .iter()
                    .map(|v| format!(
                        "<option value='{v}'{}{}>{v}</option>",
                        if current == *v { " selected" } else { "" },
                        ""
                    ))
                    .collect::<String>()
            ),
            Kind::Enum(options) => format!(
                "<select name='{field}'><option value=''>—</option>{}</select>",
                options
                    .iter()
                    .map(|v| format!(
                        "<option value='{v}'{}>{v}</option>",
                        if current == *v { " selected" } else { "" }
                    ))
                    .collect::<String>()
            ),
            Kind::Int { .. } | Kind::Float { .. } => format!(
                "<input name='{field}' value='{}' inputmode='decimal' placeholder='{}'>",
                escape_html(&current),
                escape_html(spec.default)
            ),
            Kind::List => format!(
                "<input name='{field}' value='{}' placeholder='{}'>",
                escape_html(&current),
                escape_html(spec.default)
            ),
            Kind::Str | Kind::Table => format!(
                "<input name='{field}' value='{}' placeholder='{}'>",
                escape_html(&current),
                escape_html(spec.default)
            ),
        };
        rows.push_str(&format!(
            "<tr><td><code>{}</code><div class='muted'>{}</div></td>\
             <td>{control}</td><td class='muted'>{}</td></tr>",
            escape_html(spec.path),
            escape_html(spec.doc),
            spec.apply.label()
        ));
    }
    format!(
        "<form method='post' action='/settings'><table>{rows}</table>\
         <button class='primary'>save</button>\
         <span class='muted'>empty fields are left alone; each key shows when it applies</span></form>"
    )
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
            // The answer is rendered by the shared Markdown renderer (it
            // escapes first, so only its own tags appear); the operator's
            // words stay verbatim.
            let answer_html =
                ka_agent_core::markdown::render(answer, ka_agent_core::markdown::Target::Html);
            turns_html.push_str(&format!(
                "<div class='bubble user'>{}</div>\
                 <div class='bubble assistant'><div class='muted'>{} · {}</div>{}</div>",
                escape_html(&t.inbound_text),
                time,
                escape_html(ka_agent_core::ids::short_id(t.id.as_str())),
                answer_html
            ));
        }
    }
    let _ = store;
    let body = format!(
        "<div id='chat'><div id='log'>{turns_html}</div>\
         <form id='send'><textarea id='text' rows='2' placeholder='talk to ka (Enter sends, Shift+Enter newline)'></textarea><button class='primary'>send</button></form></div>\
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

/// `GET /usage`: spend per day, CSS bars, today against the budget.
pub fn usage(runtime: &Runtime, store: &Store) -> crate::Response {
    let cfg = runtime.config();
    let since = jiff::Timestamp::now()
        .checked_sub(jiff::Span::new().days(30))
        .map(|t| t.as_millisecond())
        .unwrap_or(0);
    let rows = store.usage_rows_since(since).unwrap_or_default();
    // Bucket by local day (a turn lands on the day it started).
    let tz = runtime.now_zoned().time_zone().clone();
    let mut buckets: Vec<(String, u64, u64, f64)> = Vec::new();
    for (at_ms, cost, tokens) in rows {
        let day = jiff::Timestamp::from_millisecond(at_ms)
            .map(|t| t.to_zoned(tz.clone()).strftime("%Y-%m-%d").to_string())
            .unwrap_or_default();
        match buckets.last_mut() {
            Some(last) if last.0 == day => {
                last.1 += 1;
                last.2 += tokens;
                last.3 += cost;
            }
            _ => buckets.push((day, 1, tokens, cost)),
        }
    }
    let max = buckets
        .iter()
        .map(|b| b.3)
        .fold(0.0_f64, f64::max)
        .max(0.01);
    let cap = cfg.budgets.daily_usd;
    let rows_html: String = buckets
        .iter()
        .rev()
        .map(|(day, turns, tokens, cost)| {
            let pct = (cost / max * 100.0).clamp(1.0, 100.0);
            let over = cap.is_some_and(|c| *cost > c);
            format!(
                "<tr><td>{day}</td><td>{turns}</td><td>{tokens}</td>\
                 <td><span class='bar{}' style='width:{}em'></span> <span class='{}'>${:.4}</span></td></tr>",
                if over { " over" } else { "" },
                (pct / 4.0) as u32,
                if over { "bad" } else { "" },
                cost
            )
        })
        .collect();
    let today = buckets.last().map(|b| b.3).unwrap_or(0.0);
    let budget_note = match cap {
        Some(c) => format!("${today:.4} of ${c:.2} today"),
        None => format!("${today:.4} today (no cap set)"),
    };
    let body = format!(
        "<p class='muted'>{budget_note}</p>\
         <table><tr><th>day</th><th>turns</th><th>tokens</th><th>cost</th></tr>{rows_html}</table>\
         <style>.bar{{display:inline-block;height:0.8em;background:var(--accent);border-radius:3px;vertical-align:middle}}.bar.over{{background:var(--bad)}}</style>"
    );
    nav_page(runtime, "/usage", "Usage", body)
}

/// `POST /settings`: format-preserving writes, then a live apply. Keys that
/// would need a restart are not offered here.
pub async fn settings_apply(runtime: &Runtime, body: &str) -> crate::Response {
    use std::fmt::Write as _;
    let Some(path) = runtime.config_path() else {
        return Response::see_other("/status");
    };
    use ka_agent_core::config_keys::{KEYS, Kind};
    let mut applied: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for spec in KEYS.iter().filter(|k| k.web) {
        let field = spec.path.replace('.', "_");
        let raw = form_field_url(body, &field);
        if raw.trim().is_empty() {
            continue;
        }
        let v = crate::url_decode(&raw.replace('+', " ")).trim().to_string();
        // Render as the TOML value of the registered kind; lists accept
        // comma-separated words (or a ready TOML array).
        let rendered = match spec.kind {
            Kind::Bool | Kind::Int { .. } | Kind::Float { .. } => v.clone(),
            Kind::Str | Kind::Enum(_) => format!("{v:?}"),
            Kind::List => {
                if v.starts_with('[') {
                    v.clone()
                } else {
                    let items: Vec<String> = v
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|s| format!("{s:?}"))
                        .collect();
                    format!("[{}]", items.join(", "))
                }
            }
            Kind::Table => v.clone(),
        };
        match ka_agent_core::config_file::set(&path, spec.path, &rendered) {
            Ok(_) => applied.push(format!("{} = {rendered}", spec.path)),
            Err(e) => errors.push(format!("{}: {e}", spec.path)),
        }
    }
    let mut flash = String::new();
    if !applied.is_empty() {
        match ka_agent_core::DaemonConfig::load(&path) {
            Ok(cfg) => {
                let report = runtime.reload_config(cfg, false).await;
                let restart = report.restart_required;
                let _ = write!(
                    flash,
                    "<p class='muted'>saved: {}{}</p>",
                    applied.join(", "),
                    if restart.is_empty() {
                        String::new()
                    } else {
                        format!(" — restart needed for: {}", restart.join(", "))
                    }
                );
            }
            Err(e) => errors.push(format!("reload: {e}")),
        }
    }
    if !errors.is_empty() {
        let _ = write!(flash, "<p class='bad'>{}</p>", errors.join("; "));
    }
    // The redirect drops the message; park it in a meta-like query param is
    // overkill — surface it on the status page via a cookie-free flash by
    // rendering the status page directly.
    let mut page = status(runtime);
    if let Some(body_start) = page.body.find("<h1>Status</h1>") {
        let at = body_start + "<h1>Status</h1>".len();
        page.body.insert_str(at, &flash);
    }
    page
}
