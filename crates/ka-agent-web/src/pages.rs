//! Server-rendered HTML pages. All user-derived text goes through
//! [`escape_html`](crate::escape_html). Mutations are POST-only.

use crate::{Response, escape_html};
use ka_agent_core::ApprovalDecision;
use ka_agent_runtime::Runtime;
use ka_agent_store::Store;

fn html_page(title: &str, body: String) -> String {
    format!(
        "<!DOCTYPE html>\n<html><head><title>{title} — Ka Agent</title>\
         <style>body{{font-family:sans-serif;max-width:800px;margin:2em auto;padding:0 1em}}\
         pre{{background:#f4f4f4;padding:1em;overflow-x:auto}}\
         table{{border-collapse:collapse;width:100%}}th,td{{border:1px solid #ddd;padding:0.4em;text-align:left}}\
         .card{{border:1px solid #ddd;border-radius:4px;padding:1em;margin:1em 0}}\
         .card button{{margin:0 0.5em}}</style></head><body><h1>{title}</h1>{body}<hr><a href=\"/\">← dashboard</a></body></html>"
    )
}

pub fn dashboard(runtime: &Runtime, store: &Store) -> crate::Response {
    let _ = store;
    let (status, strand, usd, tokens, crashes) = runtime.status();
    let session = escape_html(runtime.session_id().as_str());
    let strand = strand
        .as_deref()
        .map(escape_html)
        .unwrap_or_else(|| "(new)".into());
    let body = format!(
        "<table><tr><td>session</td><td>{session}</td></tr>\
         <tr><td>status</td><td>{status:?}</td></tr>\
         <tr><td>strand</td><td>{strand}</td></tr>\
         <tr><td>budget</td><td>${usd:.4} + {tokens} tokens</td></tr>\
         <tr><td>crashes</td><td>{crashes}</td></tr></table>\
         <a href=\"/history\">history</a> · <a href=\"/memory\">memory</a> · \
         <a href=\"/approvals\">approvals</a> · <a href=\"/audit\">audit</a> · \
         <a href=\"/intents\">intents</a>"
    );
    Response::html(html_page("Dashboard", body))
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
                        "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                        escape_html(&h.turn),
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
        "<p><b>inbound:</b></p><pre>{}</p><p><b>answer:</b></p><pre>{}</p>",
        escape_html(&turn.inbound_text),
        escape_html(&answer)
    );
    Response::html(html_page("Turn", body))
}

pub fn memory(runtime: &Runtime) -> crate::Response {
    let Ok(report) = runtime.memory_report() else {
        return Response::html(html_page("Memory", "<p>memory error</p>".into()));
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
                        "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                        escape_html(&e.file),
                        escape_html(&e.text),
                        escape_html(&e.source)
                    )
                })
                .collect();
            format!(
                "<table><tr><th>file</th><th>text</th><th>source</th></tr>{}</table>",
                rows.join("")
            )
        })
        .unwrap_or_else(|e| format!("<p>entries error: {}</p>", escape_html(&e.to_string())));
    let compiled = runtime
        .memory_compiled_text()
        .map(|text| format!("<pre>{}</pre>", escape_html(&text)))
        .unwrap_or_else(|e| format!("<p>compile error: {}</p>", escape_html(&e.to_string())));
    let body = format!(
        "<p>memory: {}B / {}B{}</p><ul>{}</ul>\
         <h2>propose an entry</h2>\
         <form method=post action=/memory/edit>\
         <textarea name=text rows=4 cols=60 placeholder=\"one durable fact\"></textarea><br>\
         <button>remember</button></form>\
         <h2>accepted entries</h2>{entries}\
         <h2>compiled (injected)</h2>{compiled}",
        report.total_bytes,
        report.budget_bytes,
        if report.truncated { " (truncated)" } else { "" },
        files.join("")
    );
    Response::html(html_page("Memory", body))
}

pub fn memory_edit(runtime: &Runtime, body: &str) -> crate::Response {
    let text = form_field(body, "text");
    if text.trim().is_empty() {
        return Response::html(html_page(
            "Memory",
            "<p>nothing to remember (empty text)</p><p><a href=\"/memory\">back</a></p>".into(),
        ));
    }
    match runtime.remember(&text, "web") {
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

/// Read one field from an `application/x-www-form-urlencoded` body,
/// percent-decoding it. Unknown fields are ignored.
fn form_field(body: &str, name: &str) -> String {
    for pair in body.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if crate::url_decode(k) == name {
                return crate::url_decode(&v.replace('+', " "));
            }
        }
    }
    String::new()
}

pub fn approvals(runtime: &Runtime, store: &Store) -> crate::Response {
    let sid = runtime.session_id();
    let pending = store.pending_approvals(&sid).unwrap_or_default();
    if pending.is_empty() {
        return Response::html(html_page("Approvals", "<p>no pending approvals</p>".into()));
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
            Some(format!(
                "<div class='card'><p>{q}</p>{d}<form method=post action=/approvals/decide>\
                 <input type=hidden name=id value={}><button name=decision value=y>Allow</button>\
                 <button name=decision value=n>Deny</button></form></div>",
                escape_html(a.id.as_str())
            ))
        })
        .collect();
    Response::html(html_page("Approvals", cards.join("")))
}

pub async fn approvals_decide(runtime: &Runtime, body: &str) -> crate::Response {
    let id = form_field(body, "id");
    let decision = match form_field(body, "decision").as_str() {
        "y" | "always" => ApprovalDecision::Allow,
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

pub fn audit(store: &Store) -> crate::Response {
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
