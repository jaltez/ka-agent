//! Server-rendered HTML pages. All user-derived text goes through
//! [`escape_html`](crate::escape_html). Mutations are POST-only.

use crate::{Response, escape_html};
use ka_agent_core::ApprovalDecision;
use ka_agent_runtime::Runtime;
use ka_agent_store::Store;

/// The actor of the admin-only pages (they are rendered only for the
/// operator; per-user pages take an explicit principal).
fn op() -> ka_agent_core::Principal {
    ka_agent_core::Principal::operator()
}

/// One sidebar entry: `(path, label, badge count)`.
type NavItem = (&'static str, &'static str, usize);

const STYLE: &str = r#"<style>
:root{
  --bg:#f6f6f4;--fg:#16181d;--muted:#6b7280;--border:#e3e4e8;--border-strong:#cfd1d8;
  --accent:#4f46e5;--accent-fg:#fff;--accent-soft:#eef0ff;--surface:#fff;--surface-2:#fafaf9;
  --ok:#047857;--ok-soft:#ecfdf5;--bad:#b91c1c;--bad-soft:#fef2f2;--warn:#b45309;--warn-soft:#fffbeb;
  --radius:10px;--radius-lg:14px;--shadow:0 1px 2px rgba(16,18,23,.05),0 8px 24px rgba(16,18,23,.06);
  --mono:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;
}
@media(prefers-color-scheme:dark){:root:not([data-theme="light"]){
  --bg:#0e1014;--fg:#e7e9ee;--muted:#9aa1ab;--border:#23272f;--border-strong:#333945;
  --accent:#8b93ff;--accent-fg:#0d0f14;--accent-soft:#1a1e33;--surface:#14171d;--surface-2:#191d24;
  --ok:#34d399;--ok-soft:#12251f;--bad:#f87171;--bad-soft:#2a1515;--warn:#fbbf24;--warn-soft:#2a2111;
  --shadow:0 1px 2px rgba(0,0,0,.4),0 8px 24px rgba(0,0,0,.35);
}}
:root[data-theme="dark"]{
  --bg:#0e1014;--fg:#e7e9ee;--muted:#9aa1ab;--border:#23272f;--border-strong:#333945;
  --accent:#8b93ff;--accent-fg:#0d0f14;--accent-soft:#1a1e33;--surface:#14171d;--surface-2:#191d24;
  --ok:#34d399;--ok-soft:#12251f;--bad:#f87171;--bad-soft:#2a1515;--warn:#fbbf24;--warn-soft:#2a2111;
  --shadow:0 1px 2px rgba(0,0,0,.4),0 8px 24px rgba(0,0,0,.35);
}
*{box-sizing:border-box}
[hidden]{display:none !important}
html,body{height:100%}
body{margin:0;font:14.5px/1.55 system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;background:var(--bg);color:var(--fg);-webkit-font-smoothing:antialiased}
a{color:var(--accent);text-decoration:none}
a:hover{text-decoration:underline}
.layout{display:flex;min-height:100vh;max-width:1240px;margin:0 auto}
aside{width:224px;flex:none;padding:1rem .7rem 1.4rem;border-right:1px solid var(--border);background:var(--surface);position:sticky;top:0;height:100vh;overflow-y:auto}
aside .brand{display:block;padding:.35rem .65rem 1.1rem;font-weight:700;letter-spacing:.01em}
aside .brand .who{display:block;font-weight:500;color:var(--muted);font-size:.8rem;margin-top:.15rem}
.navlabel{padding:.9rem .65rem .3rem;font-size:.7rem;letter-spacing:.09em;text-transform:uppercase;color:var(--muted)}\
.themebtn{width:calc(100% - 1.3rem);justify-content:flex-start;margin:.6rem .65rem 0;font-size:.8rem}
aside a{display:flex;align-items:center;justify-content:space-between;gap:.5rem;padding:.42rem .65rem;margin:.05rem 0;border-radius:8px;color:var(--fg);text-decoration:none;font-size:.93rem}
aside a:hover{background:var(--surface-2);text-decoration:none}
aside a.active{background:var(--accent-soft);color:var(--accent);font-weight:600}
aside a .badge{background:var(--accent);color:var(--accent-fg)}
.badge{display:inline-flex;align-items:center;justify-content:center;min-width:1.35rem;height:1.35rem;padding:0 .4rem;border-radius:99px;background:var(--accent);color:var(--accent-fg);font-size:.72rem;font-weight:600}
main{flex:1;padding:1.6rem 2rem 3rem;min-width:0}
.pagehead{display:flex;align-items:flex-start;justify-content:space-between;gap:1rem;flex-wrap:wrap;margin:0 0 1.2rem}
.pagehead h1{font-size:1.4rem;line-height:1.2;margin:0}
.pagehead .sub{color:var(--muted);margin-top:.25rem;max-width:62ch}
.pagehead .actions{display:flex;gap:.45rem;flex-wrap:wrap}
h2{font-size:1.02rem;margin:1.5rem 0 .6rem;letter-spacing:.01em}
h3{font-size:.92rem;margin:1rem 0 .4rem}
.card{background:var(--surface);border:1px solid var(--border);border-radius:var(--radius-lg);padding:1rem 1.1rem;margin:.75rem 0;box-shadow:var(--shadow)}
.card h2,.card h3{margin-top:0}
.card .cardhead{display:flex;align-items:center;justify-content:space-between;gap:.6rem;margin:-.1rem 0 .7rem}
.card .cardhead .k{color:var(--muted);font-size:.78rem;text-transform:uppercase;letter-spacing:.07em}
button,.btn{font:inherit;font-size:.9rem;display:inline-flex;align-items:center;gap:.35rem;border:1px solid var(--border-strong);background:var(--surface);color:var(--fg);border-radius:8px;padding:.38rem .8rem;cursor:pointer;line-height:1.2;transition:border-color .12s,background .12s}
button:hover,.btn:hover{border-color:var(--accent);text-decoration:none}
button.primary,.btn.primary{background:var(--accent);border-color:var(--accent);color:var(--accent-fg)}
button.primary:hover{filter:brightness(1.06)}
button.ghost,.btn.ghost{border-color:transparent;background:transparent;color:var(--muted)}
button.ghost:hover{background:var(--surface-2);color:var(--fg)}
button.danger{color:var(--bad);border-color:var(--bad)}
button.sm{padding:.22rem .55rem;font-size:.82rem}
button:disabled{opacity:.5;cursor:not-allowed}
:focus-visible{outline:2px solid var(--accent);outline-offset:2px}
input,select,textarea{font:inherit;color:var(--fg);background:var(--surface);border:1px solid var(--border-strong);border-radius:8px;padding:.42rem .6rem;min-height:2.1rem}
input:focus,select:focus,textarea:focus{border-color:var(--accent);outline:none;box-shadow:0 0 0 3px var(--accent-soft)}
textarea{resize:vertical;min-height:4.5rem;width:100%}
label.field{display:block;margin:.55rem 0}
label.field>span{display:block;font-size:.8rem;color:var(--muted);margin-bottom:.22rem}
label.field input,label.field select,label.field textarea{width:100%}
.formrow{display:flex;gap:.5rem;flex-wrap:wrap;align-items:end}
.formrow>*{margin:0}
.muted{color:var(--muted)}
.small{font-size:.85rem}
.mono,code,pre{font-family:var(--mono);font-size:.86em}
code{background:var(--surface-2);border:1px solid var(--border);border-radius:5px;padding:.06rem .32rem}
pre{background:var(--surface-2);border:1px solid var(--border);border-radius:10px;padding:.7rem .9rem;overflow:auto}
pre code{background:none;border:0;padding:0}
table{border-collapse:collapse;width:100%;font-size:.9rem}
th,td{border-bottom:1px solid var(--border);padding:.5rem .55rem;text-align:left;vertical-align:top}
th{color:var(--muted);font-weight:600;font-size:.75rem;text-transform:uppercase;letter-spacing:.06em}
tr:last-child td{border-bottom:0}
tbody tr:hover{background:var(--surface-2)}
.tablewrap{overflow-x:auto;border:1px solid var(--border);border-radius:var(--radius);background:var(--surface)}
.tablewrap table{min-width:100%}
.tablewrap th,.tablewrap td{padding:.55rem .7rem}
.statgrid{display:grid;grid-template-columns:repeat(auto-fit,minmax(200px,1fr));gap:.75rem}
.statgrid .card{margin:0}
.statgrid .k{color:var(--muted);font-size:.75rem;text-transform:uppercase;letter-spacing:.07em}
.statgrid .v{font-size:1.15rem;margin-top:.2rem;font-weight:600}
.grid2{display:grid;grid-template-columns:repeat(auto-fit,minmax(300px,1fr));gap:.75rem;align-items:start}
.chip{display:inline-flex;align-items:center;gap:.3rem;border:1px solid var(--border);border-radius:99px;padding:.12rem .6rem;font-size:.8rem;color:var(--muted);background:var(--surface)}
.chip b{color:var(--fg);font-weight:600}
.chip.warn{border-color:var(--warn);color:var(--warn);background:var(--warn-soft)}
.tag{display:inline-block;border-radius:6px;padding:.08rem .45rem;font-size:.75rem;background:var(--surface-2);border:1px solid var(--border)}
.empty{border:1px dashed var(--border-strong);border-radius:var(--radius);padding:1rem;color:var(--muted);text-align:center}
.flash{border-left:3px solid var(--accent);background:var(--accent-soft);padding:.55rem .8rem;border-radius:8px;margin:.6rem 0}
.flash.bad{border-left-color:var(--bad);background:var(--bad-soft)}
.flash.ok{border-left-color:var(--ok);background:var(--ok-soft)}
.bar{height:.45rem;border-radius:99px;background:var(--border);overflow:hidden}
.bar>i{display:block;height:100%;background:var(--accent)}
.ok{color:var(--ok)}.bad{color:var(--bad)}.warn{color:var(--warn)}
.actions{display:flex;gap:.4rem;flex-wrap:wrap;align-items:center}
.nowrap{white-space:nowrap}
/* chat */
#chat{display:flex;flex-direction:column;height:calc(100vh - 10.5rem)}
#chatbar{display:flex;align-items:center;gap:.4rem;flex-wrap:wrap;padding:.1rem 0 .6rem}
#chatbar .spacer{flex:1}
#log{flex:1;overflow-y:auto;padding:.4rem .2rem;display:flex;flex-direction:column;gap:.1rem}
.bubble{max-width:78%;border-radius:14px;padding:.55rem .9rem;margin:.28rem 0;white-space:pre-wrap;word-wrap:break-word;line-height:1.5}
.bubble.user{align-self:flex-end;background:var(--accent);color:var(--accent-fg);border-bottom-right-radius:4px}
.bubble.user code{background:rgba(255,255,255,.18);border-color:transparent;color:inherit}
.bubble.assistant{align-self:flex-start;background:var(--surface);border:1px solid var(--border);border-bottom-left-radius:4px}
.bubble.system{align-self:center;background:none;color:var(--muted);font-size:.85rem;max-width:100%}
.bubble.approval{align-self:flex-start;background:var(--warn-soft);border:1px solid var(--warn);max-width:100%}
.bubble .tool{font-size:.85rem;color:var(--muted);white-space:pre-wrap}
.bubble .tool.ok{color:var(--ok)}
.bubble .tool.bad{color:var(--bad)}
.bubble details{margin-top:.25rem}
.bubble details summary{cursor:pointer;color:var(--muted);font-size:.82rem}
.bubble img.thumb{max-width:320px;max-height:240px;border-radius:8px;display:block;margin-top:.35rem}
details.card>summary{cursor:pointer;list-style:none;display:flex;align-items:center;gap:.5rem;flex-wrap:wrap}
details.card>summary::-webkit-details-marker{display:none}
details.card>summary:before{content:'▸';color:var(--muted)}
details.card[open]>summary:before{content:'▾'}
.bubble .meta{color:var(--muted);font-size:.75rem;margin-bottom:.15rem}
#chat.streaming #log:after{content:'▍';color:var(--accent);animation:blink 1s steps(2) infinite;align-self:flex-start;margin-left:.3rem}
@keyframes blink{50%{opacity:0}}
form#send{display:flex;gap:.5rem;padding:.65rem 0 0;align-items:flex-end;position:sticky;bottom:0;background:var(--bg);padding-bottom:.4rem}
#text{flex:1;font:inherit;border:1px solid var(--border-strong);border-radius:12px;background:var(--surface);color:var(--fg);padding:.6rem .85rem;min-height:2.6rem;max-height:12rem}
#text:focus{border-color:var(--accent);box-shadow:0 0 0 3px var(--accent-soft);outline:none}
.filebtn{display:inline-flex;align-items:center;justify-content:center;border:1px solid var(--border-strong);border-radius:9px;padding:.42rem .6rem;cursor:pointer;background:var(--surface);min-height:2.6rem}
.filebtn:hover{border-color:var(--accent)}
.kbd{font-family:var(--mono);font-size:.75rem;border:1px solid var(--border-strong);border-bottom-width:2px;border-radius:5px;padding:0 .28rem;background:var(--surface)}
/* mobile */
@media(max-width:820px){
  .layout{flex-direction:column}
  aside{width:auto;height:auto;position:static;border-right:0;border-bottom:1px solid var(--border);padding:.4rem .55rem .6rem;overflow:visible;display:flex;flex-wrap:wrap;align-items:center}
  aside .brand{flex:1 1 100%;display:block;padding:.25rem .5rem}
  aside .brand .who{display:none}
  .navlabel{flex:1 1 100%;padding:.4rem .5rem .1rem}
  aside a{margin:.12rem .1rem;padding:.3rem .55rem}
  form#send{flex-wrap:wrap}
  #text{flex:1 1 100%}
  form#send button.primary{margin-left:auto}
  .navlabel{display:inline-block;padding:.35rem .4rem;font-size:.66rem}
  aside a{display:inline-flex;margin:.1rem .1rem;padding:.32rem .55rem}
  main{padding:1rem .9rem 2.5rem}
  .pagehead{margin-bottom:.8rem}
  #chat{height:auto;min-height:60vh;flex:1}
  .bubble{max-width:92%}
}
</style>"#;

/// The viewport-aware head + sidebar layout (top bar on narrow screens).
/// Both the page title and the persona name are user-derived and escaped.
fn html_page(persona: &str, title: &str, body: String) -> String {
    format!(
        "<!DOCTYPE html>\n<html><head><title>{} — {}</title>\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">{STYLE}\
         <script src=\"/static/ui.js\" defer></script></head><body>\
         {body}</body></html>",
        escape_html(title),
        escape_html(persona)
    )
}

/// A page head using the display name of the profile the runtime serves
/// (`runtime.persona_name()` reads `<data root>/memory/persona.md`).
fn runtime_page(runtime: &Runtime, title: &str, body: String) -> String {
    html_page(&runtime.persona_name(), title, body)
}

/// The panel chrome for the account pages (users, groups, knowledge): the
/// same stylesheet and header, without the live badges (they need a
/// runtime, and these pages are forms).
pub fn admin_shell(active: &str, title: &str, subtitle: &str, body: String) -> Response {
    let nav = [
        ("/users", "Users"),
        ("/groups", "Groups"),
        ("/kb", "Knowledge"),
        ("/agents", "Agents"),
        ("/", "Chat"),
        ("/status", "Status"),
    ];
    let links: String = nav
        .iter()
        .map(|(href, label)| {
            let class = if *href == active { " class=active" } else { "" };
            format!(
                "<a href=\"{href}\"{class}>{}</a>",
                escape_html(label)
            )
        })
        .collect();
    let body = format!(
        "<div class=\"layout\"><aside><a class=brand href=\"/\">Ka Agent</a>{links}</aside>         <main><div class=\"pagehead\"><div><h1>{}</h1><div class=\"sub\">{}</div></div></div>{}</main></div>",
        escape_html(title),
        escape_html(subtitle),
        body
    );
    Response::html(html_page("Ka Agent", title, body))
}

/// A page with the shared sidebar navigation. Badges: approvals waiting,
/// memory to review, queued messages.
fn nav_page(runtime: &Runtime, active: &str, title: &str, body: String) -> Response {
    nav_page_sub(runtime, active, title, "", body)
}

/// The same with a one-line description under the title (what the page is
/// for) — every page says why it exists, like the CLI help does.
fn nav_page_sub(
    runtime: &Runtime,
    active: &str,
    title: &str,
    subtitle: &str,
    body: String,
) -> Response {
    let operator = ka_agent_core::Principal::operator();
    let approvals = runtime
        .pending_approval_cards(&operator)
        .map(|c| c.len())
        .unwrap_or(0);
    let review = runtime
        .pending_candidates_for(&operator)
        .map(|c| c.len())
        .unwrap_or(0);
    let queued = runtime.queued(&operator).map(|q| q.len()).unwrap_or(0);
    let groups: [(&str, &[NavItem]); 3] = [
        ("Chat", &[("/", "Chat", 0), ("/queue", "Queue", queued)]),
        (
            "Operator",
            &[
                ("/status", "Status", 0),
                ("/users", "Users", 0),
                ("/groups", "Groups", 0),
                ("/kb", "Knowledge", 0),
                ("/agents", "Agents", 0),
                ("/approvals", "Approvals", approvals),
                ("/memory", "Memory", review),
                ("/skills", "Skills", 0),
                ("/history", "History", 0),
                ("/usage", "Usage", 0),
            ],
        ),
        (
            "System",
            &[
                ("/tasks", "Tasks & cron", 0),
                ("/profiles", "Profiles & roles", 0),
                ("/models", "Models & behavior", 0),
                ("/config", "Config", 0),
                ("/grants", "Grants", 0),
                ("/keys", "Keys & secrets", 0),
                ("/extensions", "Extensions", 0),
                ("/doctor", "Doctor", 0),
                ("/logs", "Logs", 0),
                ("/audit", "Audit", 0),
                ("/intents", "Intents", 0),
                ("/contacts", "Contacts", 0),
            ],
        ),
    ];
    let mut nav = String::new();
    for (group, items) in groups {
        nav.push_str(&format!("<div class=\"navlabel\">{group}</div>"));
        for (href, label, n) in items {
            let badge = if *n > 0 {
                format!("<span class=\"badge\">{n}</span>")
            } else {
                String::new()
            };
            let class = if *href == active { "active" } else { "" };
            // `data-nav` gives chat.js a stable hook for live badge
            // updates (approvals, memory review) without a reload.
            nav.push_str(&format!(
                "<a class=\"{class}\" data-nav=\"{href}\" href=\"{href}\">{label}{badge}</a>"
            ));
        }
    }
    let who = format!(
        "{} · {}",
        escape_html(&runtime.persona_name()),
        escape_html(&ka_agent_core::layout::profile_name())
    );
    let sub = if subtitle.is_empty() {
        String::new()
    } else {
        format!("<div class=\"sub\">{}</div>", escape_html(subtitle))
    };
    Response::html(runtime_page(
        runtime,
        title,
        format!(
            "<div class=\"layout\"><aside><div class=\"brand\">Ka Agent<span class=\"who\">{who}</span></div>\
             <button class=\"ghost themebtn\" id=\"theme-toggle\" type=\"button\">theme: system</button>{nav}</aside>\
             <main><div class=\"pagehead\"><div><h1>{title}</h1>{sub}</div></div>{body}</main></div>"
        ),
    ))
}

/// A small result page for an action that produced a message (success or
/// refusal): the operator sees the agent's own words, plus a way back.
fn result_page(
    runtime: &Runtime,
    active: &str,
    title: &str,
    message: &str,
    ok: bool,
) -> crate::Response {
    let class = if ok { "flash ok" } else { "flash bad" };
    nav_page_sub(
        runtime,
        active,
        title,
        "",
        format!(
            "<div class=\"{class}\">{}</div><p><a href=\"{active}\">back to {title}</a></p>",
            escape_html(message)
        ),
    )
}

/// `GET /models`: the current model and its key, the mode/effort pickers,
/// the persona editor, and the provider catalog with one-click switching —
/// the TUI's `/model`, `/mode`, `/effort` and `ka-agent persona` in one
/// page.
pub fn models(runtime: &Runtime) -> crate::Response {
    let cfg = runtime.config();
    let current = cfg.model.clone().unwrap_or_else(|| "unset".into());
    let key_line = match runtime.model_key_state() {
        Ok(Some((_, ka_agent_runtime::ModelKeyState::Present))) => {
            "<span class=\"ok\">key present</span>".to_string()
        }
        Ok(Some((_, ka_agent_runtime::ModelKeyState::Missing(env)))) => format!(
            "<span class=\"bad\">✗ {env} missing — on your computer run <code>ka-agent key set {env}</code></span>",
            env = escape_html(&env)
        ),
        Ok(Some((_, ka_agent_runtime::ModelKeyState::Keyless))) => {
            "<span class=\"muted\">no key needed</span>".to_string()
        }
        Ok(None) => {
            "<span class=\"muted\">no model configured — run ka-agent setup</span>".to_string()
        }
        Err(e) => format!("<span class=\"bad\">{}</span>", escape_html(&e.to_string())),
    };
    let mode = runtime.effective_mode_for(ka_agent_core::OPERATOR_USER);
    let modes: String = ka_agent_channels::commands::MODES
        .iter()
        .map(|(name, desc)| {
            let class = if *name == mode { "primary" } else { "" };
            format!(
                "<form method=post action=/models/mode style=display:inline>\
                 <input type=hidden name=mode value=\"{name}\">\
                 <button class=\"{class}\">{name}</button></form> \
                 <span class=\"muted small\">{desc}</span>"
            )
        })
        .collect::<Vec<_>>()
        .join("<br>");
    let effort = runtime
        .effort(&op())
        .unwrap_or_else(|| "model default".to_string());
    let efforts: String = ka_agent_channels::commands::EFFORTS
        .iter()
        .map(|level| {
            let class = if *level == effort { "primary" } else { "" };
            format!(
                "<form method=post action=/models/effort style=display:inline>\
                 <input type=hidden name=level value=\"{level}\">\
                 <button class=\"{class} sm\">{level}</button></form>"
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let persona = escape_html(&runtime.persona_text());
    let listing = runtime.models_listing();
    let mut provider_rows = String::new();
    for p in &listing.providers {
        let is_current = listing
            .current
            .as_deref()
            .is_some_and(|c| c.starts_with(&format!("{}/", p.name)));
        let mut rows = String::new();
        // A big catalog is collapsed: the current provider opens, and each
        // list caps at 20 with a pointer to the selector form.
        for m in p.models.iter().take(20) {
            let current_mark = if listing.current.as_deref() == Some(m.as_str()) {
                " <span class=\"tag\">current</span>"
            } else {
                ""
            };
            rows.push_str(&format!(
                "<tr><td><code>{}</code>{current_mark}</td>\
                 <td><form method=post action=/models/set>\
                 <input type=hidden name=selector value=\"{}\"><button class=\"sm\">use</button></form></td></tr>",
                escape_html(m),
                escape_html(m)
            ));
        }
        let more = if p.models.len() > 20 {
            format!(
                "<p class=\"muted small\">{} more — set any selector with the form above.</p>",
                p.models.len() - 20
            )
        } else {
            String::new()
        };
        provider_rows.push_str(&format!(
            "<details class=\"card provider\"{}><summary><b>{}</b> <span class=\"muted small\">{}</span> {}</summary>\
             <div class=\"tablewrap\"><table>{rows}</table></div>{more}</details>",
            if is_current { " open" } else { "" },
            escape_html(&p.name),
            escape_html(&p.note),
            if is_current { "<span class=\"tag\">current provider</span>" } else { "" }
        ));
    }
    if provider_rows.is_empty() {
        provider_rows = "<div class=\"empty\">no model catalog yet — set one by selector below or run <code>ka-agent models refresh</code></div>".into();
    }
    // Every group's model policy, read from the store (the toggles for the
    // catalog itself live on the Groups page, one editor for one concept).
    let groups = runtime.store().list_groups().unwrap_or_default();
    let group_rows = if groups.is_empty() {
        "<div class=\"empty\">no groups — every user may use every model</div>".to_string()
    } else {
        let rows: String = groups
            .iter()
            .map(|g| {
                let allow = if g.models_allow.is_empty() {
                    "every model".to_string()
                } else {
                    g.models_allow.join(", ")
                };
                format!(
                    "<tr><td><code>{}</code></td><td>{}</td><td>{}</td></tr>",
                    escape_html(&g.name),
                    escape_html(&allow),
                    escape_html(g.models_default.as_deref().unwrap_or("profile default"))
                )
            })
            .collect();
        format!(
            "<div class=\"tablewrap\"><table><tr><th>group</th><th>allowed models</th><th>default</th></tr>{rows}</table></div>"
        )
    };
    let body = format!(
        "<div class=\"grid2\">\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">current model</span><span class=\"tag\">{current}</span></div>\
         <p>{key_line}</p>\
         <form method=post action=/models/set class=formrow>\
         <input name=selector placeholder=\"provider/model[@effort]\" style=\"flex:1\" required>\
         <button class=primary>set model</button></form>\
         <p class=\"muted small\">Applies to the session now and to `model` in the config.</p></div>\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">mode</span><span class=\"tag\">{mode}</span></div>{modes}\
         <p class=\"muted small\">A role's `/mode` is clamped to its own ceiling.</p></div>\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">reasoning effort</span><span class=\"tag\">{effort}</span></div>{efforts}</div>\
         </div>\
         <h2>Persona</h2>\
         <form method=post action=/persona>\
         <label class=field><span>Persona text compiled into IDENTITY.md — the first line must be <code># &lt;Name&gt;</code>, then tone and house rules</span>\
         <textarea name=text rows=7>{persona}</textarea></label>\
         <button class=primary>save persona</button></form>\
         <h2>Per-group model access</h2>{group_rows}\
         <p class=\"muted small\">A user's allowlist is the union of their groups' lists; a group with none grants every catalog model. Editing lives on the <a href=\"/groups\">Groups</a> page.</p>\
         <h2>Provider catalog</h2>{provider_rows}",
        current = escape_html(&current),
    );
    nav_page_sub(
        runtime,
        "/models",
        "Models & behavior",
        "The session's model, mode, effort and persona — the TUI's /model, /mode, /effort and the persona editor.",
        body,
    )
}

pub async fn models_set(runtime: &Runtime, body: &str) -> crate::Response {
    let selector = form_field(body, "selector");
    let operator = ka_agent_core::Principal::operator();
    match runtime.set_model(selector.trim(), &operator).await {
        Ok(id) => result_page(
            runtime,
            "/models",
            "Models & behavior",
            &format!("model → {id}"),
            true,
        ),
        Err(e) => result_page(
            runtime,
            "/models",
            "Models & behavior",
            &e.to_string(),
            false,
        ),
    }
}

pub async fn models_mode(runtime: &Runtime, body: &str) -> crate::Response {
    let mode = form_field(body, "mode");
    let operator = ka_agent_core::Principal::operator();
    match runtime.set_mode(&mode, "web", &operator).await {
        Ok(applied) => result_page(
            runtime,
            "/models",
            "Models & behavior",
            &format!("mode → {applied}"),
            true,
        ),
        Err(e) => result_page(
            runtime,
            "/models",
            "Models & behavior",
            &e.to_string(),
            false,
        ),
    }
}

pub async fn models_effort(runtime: &Runtime, body: &str) -> crate::Response {
    let level = form_field(body, "level");
    let operator = ka_agent_core::Principal::operator();
    match runtime.set_effort(&level, &operator).await {
        Ok(reply) => result_page(runtime, "/models", "Models & behavior", &reply, true),
        Err(e) => result_page(
            runtime,
            "/models",
            "Models & behavior",
            &e.to_string(),
            false,
        ),
    }
}

pub fn persona_set(runtime: &Runtime, body: &str) -> crate::Response {
    let text = form_field(body, "text");
    match runtime.set_persona(&text) {
        Ok(()) => result_page(
            runtime,
            "/models",
            "Models & behavior",
            "persona saved (IDENTITY.md recompiled for every role's engine home)",
            true,
        ),
        Err(e) => result_page(
            runtime,
            "/models",
            "Models & behavior",
            &e.to_string(),
            false,
        ),
    }
}

/// `GET /config`: every registered key (path, kind, apply timing, default,
/// current value) plus the editable web-safe form and the raw file.
pub fn config_page(runtime: &Runtime) -> crate::Response {
    use ka_agent_core::config_keys::KEYS;
    let Some(path) = runtime.config_path() else {
        return nav_page_sub(
            runtime,
            "/config",
            "Config",
            "",
            "<div class=\"empty\">no config file to edit</div>".into(),
        );
    };
    let mut rows = String::new();
    for spec in KEYS {
        let value = ka_agent_core::config_file::get(&path, spec.path)
            .ok()
            .flatten()
            .unwrap_or_else(|| "—".to_string());
        rows.push_str(&format!(
            "<tr><td><code>{}</code><div class=\"muted small\">{}</div></td>\
             <td>{}</td><td>{}</td><td class=\"muted small\">{}</td><td><code>{}</code></td></tr>",
            escape_html(spec.path),
            escape_html(spec.doc),
            escape_html(spec.kind.label()),
            escape_html(spec.apply.label()),
            escape_html(spec.default),
            escape_html(&value)
        ));
    }
    let raw = std::fs::read_to_string(&path)
        .map(|t| escape_html(&t))
        .unwrap_or_else(|e| escape_html(&e.to_string()));
    let body = format!(
        "<p class=\"muted\">file <code>{}</code> · changes apply now, next turn or after a restart (see each row); the form below edits only keys the panel may touch.</p>\
         {}<h2>Every key</h2><div class=\"tablewrap\"><table><tr><th>key</th><th>type</th><th>applies</th><th>default</th><th>current</th></tr>{rows}</table></div>\
         <h2>config.toml</h2><details><summary class=\"muted\">show the raw file</summary><pre><code>{raw}</code></pre></details>",
        escape_html(&path.display().to_string()),
        settings_form(runtime)
    );
    nav_page_sub(
        runtime,
        "/config",
        "Config",
        "The profile's configuration: schema, apply timing and the safe editor. The CLI equivalent is `ka-agent config get/set/schema/edit`.",
        body,
    )
}

/// `GET /queue`: messages waiting for the next turn, with remove/clear.
pub fn queue(runtime: &Runtime, who: &ka_agent_core::Principal) -> crate::Response {
    let items = runtime.queued(who).unwrap_or_default();
    if items.is_empty() {
        return nav_page_sub(
            runtime,
            "/queue",
            "Queue",
            "Messages that arrived mid-turn wait here until the next turn.",
            "<div class=\"empty\">the queue is empty</div>".into(),
        );
    }
    let rows: String = items
        .iter()
        .map(|(id, source, text)| {
            format!(
                "<tr><td><code>{}</code></td><td>{}</td><td>{}</td>\
                 <td><form method=post action=/queue/remove>\
                 <input type=hidden name=id value=\"{}\"><button class=sm>remove</button></form></td></tr>",
                escape_html(ka_agent_core::ids::short_id(id)),
                escape_html(source),
                escape_html(text),
                escape_html(id)
            )
        })
        .collect();
    nav_page_sub(
        runtime,
        "/queue",
        "Queue",
        "Messages that arrived mid-turn wait here until the next turn.",
        format!(
            "<div class=\"tablewrap\"><table><tr><th>id</th><th>source</th><th>text</th><th></th></tr>{rows}</table></div>\
             <form method=post action=/queue/clear><button class=danger>clear the queue</button></form>"
        ),
    )
}

pub fn queue_remove(
    runtime: &Runtime,
    body: &str,
    who: &ka_agent_core::Principal,
) -> crate::Response {
    let id = form_field(body, "id");
    match runtime.remove_queued(&id, who) {
        Ok(true) => Response::see_other("/queue"),
        Ok(false) => result_page(
            runtime,
            "/queue",
            "Queue",
            "no queued message matches that id",
            false,
        ),
        Err(e) => result_page(runtime, "/queue", "Queue", &e.to_string(), false),
    }
}

pub fn queue_clear(runtime: &Runtime, who: &ka_agent_core::Principal) -> crate::Response {
    match runtime.clear_queue(who) {
        Ok(n) => result_page(
            runtime,
            "/queue",
            "Queue",
            &format!("cleared {n} queued message(s)"),
            true,
        ),
        Err(e) => result_page(runtime, "/queue", "Queue", &e.to_string(), false),
    }
}

/// `GET /grants`: scoped tool grants (`ka-agent grant add|list|rm`).
pub fn grants(runtime: &Runtime) -> crate::Response {
    let grants = ka_agent_channels::grants::Grants::load(runtime.store()).unwrap_or_default();
    let rows: String = grants
        .scopes
        .iter()
        .map(|scope| {
            format!(
                "<tr><td><code>{}</code></td><td><form method=post action=/grants/rm>\
                 <input type=hidden name=scope value=\"{}\"><button class=\"sm danger\">remove</button></form></td></tr>",
                escape_html(scope),
                escape_html(scope)
            )
        })
        .collect();
    let body = format!(
        "<div class=\"card\"><div class=\"cardhead\"><span class=\"k\">scope grants</span><span class=\"tag\">{} scope(s)</span></div>\
         <p class=\"muted small\">Scoped tools (http_get, GitHub) need a matching grant. Wildcards: <code>http:get:*</code>.</p>\
         <form method=post action=/grants/add class=formrow>\
         <input name=scope placeholder=\"http:get:api.github.com\" style=\"flex:1\" required>\
         <button class=primary>add grant</button></form></div>\
         {}",
        grants.scopes.len(),
        if rows.is_empty() {
            "<div class=\"empty\">no grants</div>".to_string()
        } else {
            format!(
                "<div class=\"tablewrap\"><table><tr><th>scope</th><th></th></tr>{rows}</table></div>"
            )
        }
    );
    nav_page_sub(
        runtime,
        "/grants",
        "Grants",
        "Authority for scoped tools, stored in the state store — the CLI's `ka-agent grant`.",
        body,
    )
}

pub fn grants_add(runtime: &Runtime, body: &str) -> crate::Response {
    let scope = form_field(body, "scope");
    let mut grants = match ka_agent_channels::grants::Grants::load(runtime.store()) {
        Ok(g) => g,
        Err(e) => return result_page(runtime, "/grants", "Grants", &e.to_string(), false),
    };
    grants.add(&scope);
    match grants.save(runtime.store()) {
        Ok(()) => Response::see_other("/grants"),
        Err(e) => result_page(runtime, "/grants", "Grants", &e.to_string(), false),
    }
}

pub fn grants_rm(runtime: &Runtime, body: &str) -> crate::Response {
    let scope = form_field(body, "scope");
    let mut grants = match ka_agent_channels::grants::Grants::load(runtime.store()) {
        Ok(g) => g,
        Err(e) => return result_page(runtime, "/grants", "Grants", &e.to_string(), false),
    };
    grants.remove(&scope);
    match grants.save(runtime.store()) {
        Ok(()) => Response::see_other("/grants"),
        Err(e) => result_page(runtime, "/grants", "Grants", &e.to_string(), false),
    }
}

/// `GET /keys`: which provider keys and channel secrets exist — names
/// only, never values (`ka-agent key list`, `secret list`).
pub fn keys(runtime: &Runtime) -> crate::Response {
    let env_path = ka_agent_port::catalog::engine_env_path(&runtime.engine_home());
    let env = ka_agent_port::catalog::read_env_file(&env_path);
    let provider_rows: String = if env.is_empty() {
        "<tr><td colspan=2 class=\"muted\">no provider keys in the engine .env</td></tr>".into()
    } else {
        env.keys()
            .map(|k| {
                format!(
                    "<tr><td><code>{}</code></td><td class=\"ok\">set</td></tr>",
                    escape_html(k)
                )
            })
            .collect()
    };
    let secrets_dir = runtime.data_root().join("secrets");
    let mut secret_names: Vec<String> = std::fs::read_dir(&secrets_dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    secret_names.sort();
    let secret_rows: String = if secret_names.is_empty() {
        "<tr><td colspan=2 class=\"muted\">no channel/connector secrets</td></tr>".into()
    } else {
        secret_names
            .iter()
            .map(|n| {
                format!(
                    "<tr><td><code>{}</code></td><td class=\"ok\">set</td>\
                     <td><form method=post action=/keys/secret/rm style=display:inline>\
                     <input type=hidden name=name value=\"{}\">\
                     <button class=\"sm danger\">remove</button></form></td></tr>",
                    escape_html(n),
                    escape_html(n)
                )
            })
            .collect()
    };
    let model_line = match runtime.model_key_state() {
        Ok(Some((id, ka_agent_runtime::ModelKeyState::Missing(env)))) => format!(
            "<div class=\"flash bad\">{} needs <code>{}</code> — on your computer run <code>ka-agent key set {}</code></div>",
            escape_html(&id),
            escape_html(&env),
            escape_html(&env)
        ),
        Ok(Some((id, _))) => format!(
            "<div class=\"flash ok\">{} is ready</div>",
            escape_html(&id)
        ),
        _ => "<div class=\"flash\">no model configured</div>".into(),
    };
    let body = format!(
        "{model_line}\
         <div class=\"grid2\">\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">provider keys</span><span class=\"tag\">engine .env</span></div>\
         <div class=\"tablewrap\"><table><tr><th>name</th><th>state</th></tr>{provider_rows}</table></div>\
         <p class=\"muted small\">set: <code>ka-agent key set NAME</code>. Values are never shown here.</p></div>\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">secrets</span><span class=\"tag\">0600 files</span></div>\
         <div class=\"tablewrap\"><table><tr><th>name</th><th>state</th><th></th></tr>{secret_rows}</table></div>\
         <p class=\"muted small\">set: <code>ka-agent secret set NAME</code>. Values are never shown here.</p></div>\
         </div>"
    );
    nav_page_sub(
        runtime,
        "/keys",
        "Keys & secrets",
        "Which credentials exist (names only) and whether the current model has its key — `ka-agent key` / `ka-agent secret` parity.",
        body,
    )
}

/// `POST /keys/provider`: write one provider key into the engine `.env`
/// (0600). The value is never rendered or audited.
pub fn keys_provider(runtime: &Runtime, body: &str) -> crate::Response {
    let name = form_field(body, "name").trim().to_string();
    let value = form_field(body, "value");
    let valid = (2..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !valid {
        return result_page(
            runtime,
            "/keys",
            "Keys & secrets",
            "bad key name (A-Z, 0-9, _)",
            false,
        );
    }
    if value.is_empty() {
        return result_page(runtime, "/keys", "Keys & secrets", "empty value", false);
    }
    let path = ka_agent_port::catalog::engine_env_path(&runtime.engine_home());
    match ka_agent_port::catalog::upsert_env_file(&path, &name, &value) {
        Ok(()) => {
            let mut rec = ka_agent_core::AuditRecord::new("operator", "key.set", &name, None)
                .with_role(ka_agent_core::ADMIN_ROLE);
            let _ = runtime.store().append_audit(&mut rec);
            result_page(
                runtime,
                "/keys",
                "Keys & secrets",
                &format!("{name} set (0600) — applies the next time a worker starts"),
                true,
            )
        }
        Err(e) => result_page(runtime, "/keys", "Keys & secrets", &e.to_string(), false),
    }
}

/// `POST /keys/secret`: write one secret file (0600). Same "never echo"
/// rule as the CLI.
pub fn keys_secret(runtime: &Runtime, body: &str) -> crate::Response {
    let name = form_field(body, "name").trim().to_string();
    let value = form_field(body, "value");
    let valid = (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !valid {
        return result_page(runtime, "/keys", "Keys & secrets", "bad secret name", false);
    }
    if value.is_empty() {
        return result_page(runtime, "/keys", "Keys & secrets", "empty value", false);
    }
    let dir = runtime.data_root().join("secrets");
    match ka_agent_core::secret::store(&name, &value, &dir) {
        Ok(_path) => {
            let mut rec = ka_agent_core::AuditRecord::new("operator", "secret.set", &name, None)
                .with_role(ka_agent_core::ADMIN_ROLE);
            let _ = runtime.store().append_audit(&mut rec);
            result_page(
                runtime,
                "/keys",
                "Keys & secrets",
                &format!("secret {name} set (0600)"),
                true,
            )
        }
        Err(e) => result_page(runtime, "/keys", "Keys & secrets", &e.to_string(), false),
    }
}

/// `POST /mcp/add`: append one MCP server to `[[mcp]]` and hot-reload.
pub async fn mcp_add(runtime: &Runtime, body: &str) -> crate::Response {
    let name = form_field(body, "name").trim().to_string();
    if !ka_agent_core::config::McpServer::valid_name(&name) {
        return result_page(
            runtime,
            "/extensions",
            "Extensions",
            "MCP name: [a-z0-9_-], up to 32, not kagent",
            false,
        );
    }
    let kind = form_field(body, "kind");
    let command = form_field(body, "command").trim().to_string();
    let url = form_field(body, "url").trim().to_string();
    let (command, url) = match kind.as_str() {
        "http" if !url.is_empty() => (None, Some(url)),
        "stdio" if !command.is_empty() => {
            let mut words = command.split_whitespace();
            let bin = words.next().unwrap_or_default().to_string();
            (
                Some((bin, words.map(str::to_string).collect::<Vec<_>>())),
                None,
            )
        }
        _ => {
            return result_page(
                runtime,
                "/extensions",
                "Extensions",
                "give exactly one of a stdio command or an http url",
                false,
            );
        }
    };
    let mut server = ka_agent_core::config::McpServer {
        name: name.clone(),
        url,
        ..Default::default()
    };
    if let Some((bin, args)) = command {
        server.command = Some(bin);
        server.args = args;
    }
    for line in form_field(body, "env").lines() {
        if let Some((k, v)) = line.split_once('=') {
            let (k, v) = (k.trim(), v.trim());
            if k.is_empty() {
                continue;
            }
            if kind == "http" {
                server.headers.insert(k.to_string(), v.to_string());
            } else {
                server.env.insert(k.to_string(), v.to_string());
            }
        }
    }
    let Some(path) = runtime.config_path() else {
        return result_page(
            runtime,
            "/extensions",
            "Extensions",
            "no config file to edit",
            false,
        );
    };
    let cfg = runtime.config();
    let mut servers = cfg.mcp.clone();
    if servers.iter().any(|m| m.name == name) {
        return result_page(
            runtime,
            "/extensions",
            "Extensions",
            "an MCP server with that name exists",
            false,
        );
    }
    servers.push(server);
    match ka_agent_core::config_file::set_mcp(&path, &servers) {
        Ok(new_cfg) => {
            runtime.reload_config(new_cfg, false).await;
            result_page(
                runtime,
                "/extensions",
                "Extensions",
                &format!("MCP server {name} added — the next worker starts it"),
                true,
            )
        }
        Err(e) => result_page(runtime, "/extensions", "Extensions", &e.to_string(), false),
    }
}

/// `POST /mcp/rm`: remove one MCP server by name.
pub async fn mcp_rm(runtime: &Runtime, body: &str) -> crate::Response {
    let name = form_field(body, "name");
    let Some(path) = runtime.config_path() else {
        return result_page(
            runtime,
            "/extensions",
            "Extensions",
            "no config file to edit",
            false,
        );
    };
    let cfg = runtime.config();
    let servers: Vec<_> = cfg.mcp.iter().filter(|m| m.name != name).cloned().collect();
    if servers.len() == cfg.mcp.len() {
        return result_page(
            runtime,
            "/extensions",
            "Extensions",
            "no such MCP server",
            false,
        );
    }
    match ka_agent_core::config_file::set_mcp(&path, &servers) {
        Ok(new_cfg) => {
            runtime.reload_config(new_cfg, false).await;
            result_page(
                runtime,
                "/extensions",
                "Extensions",
                &format!("MCP server {name} removed"),
                true,
            )
        }
        Err(e) => result_page(runtime, "/extensions", "Extensions", &e.to_string(), false),
    }
}

/// `GET /extensions`: MCP servers, plugins, hooks and the live tool
/// inventory — read-only where the CLI mutates (install/enable is the
/// operator's terminal), visible so the panel covers the surface.
pub fn extensions(runtime: &Runtime) -> crate::Response {
    let cfg = runtime.config();
    let mcp_rows: String = if cfg.mcp.is_empty() {
        "<tr><td colspan=4 class=\"muted\">none — add one below or with <code>ka-agent mcp add</code></td></tr>".into()
    } else {
        cfg.mcp
            .iter()
            .map(|m| {
                let transport = m
                    .command
                    .as_ref()
                    .map(|c| format!("stdio · <code>{}</code>", escape_html(c)))
                    .or_else(|| {
                        m.url
                            .as_ref()
                            .map(|u| format!("http · <code>{}</code>", escape_html(u)))
                    })
                    .unwrap_or_default();
                format!(
                    "<tr><td><code>{}</code></td><td>{transport}</td><td>{}</td><td>\
                     <form method=post action=/mcp/rm style=display:inline>\
                     <input type=hidden name=name value=\"{}\">\
                     <button class=\"sm danger\">remove</button></form></td></tr>",
                    escape_html(&m.name),
                    escape_html(&m.args.join(" ")),
                    escape_html(&m.name)
                )
            })
            .collect()
    };
    let lock = runtime.data_root().join("plugins/installed.json");
    let plugins = ka_agent_plugins::Lockfile::load(&lock)
        .map(|l| {
            if l.plugins.is_empty() {
                "<tr><td colspan=3 class=\"muted\">none — <code>ka-agent plugin install</code></td></tr>".to_string()
            } else {
                l.plugins
                    .iter()
                    .map(|p| {
                        format!(
                            "<tr><td><code>{}</code></td><td>{}</td><td>{}</td></tr>",
                            escape_html(&p.id),
                            escape_html(&p.version),
                            if p.enabled { "<span class=\"ok\">enabled</span>" } else { "disabled" }
                        )
                    })
                    .collect()
            }
        })
        .unwrap_or_else(|_| "<tr><td colspan=3 class=\"muted\">no plugin lockfile</td></tr>".into());
    let hooks: String = if cfg.hooks.is_empty() {
        "<tr><td colspan=3 class=\"muted\">none — <code>[[hooks]]</code></td></tr>".into()
    } else {
        cfg.hooks
            .iter()
            .map(|h| {
                format!(
                    "<tr><td><code>{}</code></td><td>{}</td><td><code>{}</code></td></tr>",
                    escape_html(&h.on),
                    escape_html(h.tool.as_deref().unwrap_or("—")),
                    escape_html(&h.command)
                )
            })
            .collect()
    };
    let tools = runtime.tools(&op());
    let tools_html = if tools.is_empty() {
        "<div class=\"empty\">no live inventory — the worker starts with the next message</div>"
            .to_string()
    } else {
        format!(
            "<div class=\"actions\">{}</div>",
            tools
                .iter()
                .map(|t| format!("<span class=\"tag\"><code>{}</code></span>", escape_html(t)))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };
    let body = format!(
        "<div>\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">MCP servers</span><span class=\"tag\">{}</span></div>\
         <div class=\"tablewrap\"><table><tr><th>name</th><th>transport</th><th>args</th></tr>{mcp_rows}</table></div>\
         <p class=\"muted small\">Tools appear as <code>&lt;server&gt;__&lt;tool&gt;</code>. Secrets resolve at worker start.</p></div>\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">plugins</span></div>\
         <div class=\"tablewrap\"><table><tr><th>id</th><th>version</th><th>state</th></tr>{plugins}</table></div>\
         <p class=\"muted small\">Install/enable from the terminal: <code>ka-agent plugin</code>.</p></div>\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">hooks</span><span class=\"tag\">{}</span></div>\
         <div class=\"tablewrap\"><table><tr><th>event</th><th>tool</th><th>command</th></tr>{hooks}</table></div></div>\
         </div>\
         <h2>Live tools</h2>{tools_html}",
        cfg.mcp.len(),
        cfg.hooks.len()
    );
    nav_page_sub(
        runtime,
        "/extensions",
        "Extensions",
        "What the engine can reach beyond its hands: MCP servers, plugins, hooks and the live tool inventory.",
        body,
    )
}

/// `GET /logs`: the daemon log's tail (the same file `ka-agent logs` reads).
pub fn logs(runtime: &Runtime) -> crate::Response {
    use std::io::{Read, Seek, SeekFrom};
    let path = runtime.data_root().join("logs/daemon.log");
    // Read only the last 256 KiB: a rotated log can be large, and the page
    // only shows the tail.
    let mut text = String::new();
    if let Ok(mut f) = std::fs::File::open(&path) {
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let start = len.saturating_sub(256 * 1024);
        if f.seek(SeekFrom::Start(start)).is_ok() {
            let mut buf = Vec::new();
            let _ = f.read_to_end(&mut buf);
            text = String::from_utf8_lossy(&buf).into_owned();
            if start > 0 {
                // Drop the partial first line of a mid-line seek.
                if let Some(nl) = text.find('\n') {
                    text = text[nl + 1..].to_string();
                }
            }
        }
    }
    let lines: Vec<&str> = text.lines().collect();
    let tail: String = lines
        .iter()
        .skip(lines.len().saturating_sub(300))
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let body = format!(
        "<p class=\"muted\">{} · last {} of {} line(s)</p>\
         {}",
        escape_html(&path.display().to_string()),
        lines.len().min(300),
        lines.len(),
        if tail.is_empty() {
            "<div class=\"empty\">the log is empty (ka-agent logs -f follows the journal)</div>"
                .to_string()
        } else {
            format!("<pre>{}</pre>", escape_html(&tail))
        }
    );
    nav_page_sub(
        runtime,
        "/logs",
        "Logs",
        "The daemon log tail — the same file `ka-agent logs` reads.",
        body,
    )
}

/// `POST /doctor/fix`: the binary's safe repairs (0700 roots, 0600 secret
/// files), with the child's own output.
pub fn doctor_fix(runtime: &Runtime) -> crate::Response {
    let Ok(exe) = std::env::current_exe() else {
        return result_page(
            runtime,
            "/doctor",
            "Doctor",
            "cannot locate the binary",
            false,
        );
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("doctor").arg("--fix");
    if let Some(cfg) = runtime.config_path() {
        cmd.arg("--config").arg(cfg);
    }
    cmd.env("KA_AGENT_DATA_DIR", runtime.data_root());
    let (ok, text) = match cmd.output() {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !o.status.success() {
                let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                text = if err.is_empty() {
                    text
                } else {
                    format!("{text}\n{err}")
                };
            }
            (o.status.success(), text)
        }
        Err(e) => (false, format!("doctor did not start: {e}")),
    };
    result_page(
        runtime,
        "/doctor",
        "Doctor",
        if text.is_empty() {
            "doctor --fix finished"
        } else {
            &text
        },
        ok,
    )
}

/// `POST /keys/secret/rm`: delete one secret file (the CLI's `secret rm`).
pub fn keys_secret_rm(runtime: &Runtime, body: &str) -> crate::Response {
    let name = form_field(body, "name").trim().to_string();
    let valid = (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !valid || name == "." || name == ".." {
        return result_page(runtime, "/keys", "Keys & secrets", "bad secret name", false);
    }
    let path = runtime.data_root().join("secrets").join(&name);
    match std::fs::remove_file(&path) {
        Ok(()) => {
            let mut rec = ka_agent_core::AuditRecord::new("operator", "secret.rm", &name, None)
                .with_role(ka_agent_core::ADMIN_ROLE);
            let _ = runtime.store().append_audit(&mut rec);
            result_page(
                runtime,
                "/keys",
                "Keys & secrets",
                &format!("secret {name} removed"),
                true,
            )
        }
        Err(e) => result_page(runtime, "/keys", "Keys & secrets", &e.to_string(), false),
    }
}

/// `POST /backup`: run the binary's own `backup` (no user path input) and
/// show its output. Restore is destructive and stays in the CLI.
pub fn backup_now(runtime: &Runtime) -> crate::Response {
    let Some(cfg) = runtime.config_path() else {
        return result_page(
            runtime,
            "/status",
            "Status",
            "no config file — nothing to back up",
            false,
        );
    };
    let Ok(exe) = std::env::current_exe() else {
        return result_page(
            runtime,
            "/status",
            "Status",
            "cannot locate the binary",
            false,
        );
    };
    let out = std::process::Command::new(exe)
        .arg("backup")
        .arg("--config")
        .arg(&cfg)
        .env("KA_AGENT_DATA_DIR", runtime.data_root())
        .output();
    let (ok, text) = match out {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !o.status.success() {
                let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                text = if err.is_empty() {
                    text
                } else {
                    format!("{text}\n{err}")
                };
            }
            (o.status.success(), text)
        }
        Err(e) => (false, format!("backup did not start: {e}")),
    };
    if ok {
        let mut rec = ka_agent_core::AuditRecord::new("operator", "backup.create", "backups", None)
            .with_role(ka_agent_core::ADMIN_ROLE);
        let _ = runtime.store().append_audit(&mut rec);
    }
    result_page(
        runtime,
        "/status",
        "Status",
        if text.is_empty() {
            "backup done"
        } else {
            &text
        },
        ok,
    )
}

/// The full `ka-agent doctor --json` (pin, contract hash, sandbox, live
/// checks), rendered beside the in-process checks. Args are fixed: the
/// admin page supplies no input.
fn full_doctor(runtime: &Runtime) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("doctor").arg("--json");
    if let Some(cfg) = runtime.config_path() {
        cmd.arg("--config").arg(cfg);
    }
    cmd.env("KA_AGENT_DATA_DIR", runtime.data_root());
    let out = cmd.output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let checks = v["checks"].as_array()?;
    let rows: String = checks
        .iter()
        .map(|c| {
            let level = c["level"].as_str().unwrap_or("info");
            let (mark, class) = match level {
                "ok" => ("✓", "ok"),
                "warn" => ("!", "warn"),
                "fail" => ("✗", "bad"),
                _ => ("·", "muted"),
            };
            format!(
                "<tr><td class=\"{class}\">{mark}</td><td>{}</td></tr>",
                escape_html(c["text"].as_str().unwrap_or(""))
            )
        })
        .collect();
    Some(format!(
        "<div class=\"card\"><div class=\"cardhead\"><span class=\"k\">full doctor</span><span class=\"tag\">ka-agent doctor --json</span></div>\
         <div class=\"tablewrap\"><table><tr><th></th><th>check</th></tr>{rows}</table></div></div>"
    ))
}

/// `GET /doctor`: the in-process half of `ka-agent doctor` — config,
/// model key, store, data-root permissions, web publication and channel
/// tokens, each with the remedy.
pub fn doctor(runtime: &Runtime) -> crate::Response {
    let mut rows: Vec<(bool, String, String)> = Vec::new(); // (ok, check, detail)
    let cfg = runtime.config();
    rows.push((
        true,
        "config".into(),
        format!(
            "loaded from {}",
            runtime
                .config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(no file)".into())
        ),
    ));
    match runtime.model_key_state() {
        Ok(Some((id, ka_agent_runtime::ModelKeyState::Present))) => {
            rows.push((true, "model".into(), format!("{id} · key present")))
        }
        Ok(Some((id, ka_agent_runtime::ModelKeyState::Keyless))) => {
            rows.push((true, "model".into(), format!("{id} · no key needed")))
        }
        Ok(Some((id, ka_agent_runtime::ModelKeyState::Missing(env)))) => rows.push((
            false,
            "model".into(),
            format!("{id} needs {env} — `ka-agent key set {env}`"),
        )),
        Ok(None) => rows.push((false, "model".into(), "no model — `ka-agent setup`".into())),
        Err(e) => rows.push((false, "model".into(), e.to_string())),
    }
    let version = runtime.store().user_version().unwrap_or(0);
    rows.push((
        version > 0,
        "store".into(),
        format!("state.db schema v{version}, {} sessions", {
            runtime
                .store()
                .list_recent_sessions(1000)
                .map(|s| s.len())
                .unwrap_or(0)
        }),
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let root = runtime.data_root();
        let mode = std::fs::metadata(&root)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0);
        let ok = mode & 0o077 == 0;
        rows.push((
            ok,
            "data root".into(),
            format!(
                "{} · {:03o}{}",
                root.display(),
                mode,
                if ok {
                    ""
                } else {
                    " — others can enter; `ka-agent doctor --fix` tightens it"
                }
            ),
        ));
    }
    let web_url = runtime
        .store()
        .meta_get("web.url")
        .ok()
        .flatten()
        .filter(|u| !u.is_empty());
    rows.push((
        web_url.is_some() || !cfg.web.enabled,
        "web".into(),
        web_url.unwrap_or_else(|| "not published (disabled or failed to bind)".into()),
    ));
    let secrets = runtime.data_root().join("secrets");
    let token_state = |enabled: bool, name: Option<&str>| -> (bool, String) {
        if !enabled {
            return (true, "off".into());
        }
        match name {
            Some(n) if secrets.join(n).is_file() => (true, format!("secret {n} present")),
            Some(n) => (
                false,
                format!("secret {n} missing — `ka-agent secret set {n}`"),
            ),
            None => (false, "enabled without token_secret".into()),
        }
    };
    let (t_ok, t_text) = token_state(cfg.telegram.enabled, cfg.telegram.token_secret.as_deref());
    rows.push((t_ok, "telegram".into(), t_text));
    let (m_ok, m_text) = token_state(cfg.matrix.enabled, cfg.matrix.token_secret.as_deref());
    rows.push((m_ok, "matrix".into(), m_text));
    let html: String = rows
        .iter()
        .map(|(ok, check, detail)| {
            let mark = if *ok {
                "<span class=\"ok\">✓</span>"
            } else {
                "<span class=\"bad\">✗</span>"
            };
            format!(
                "<tr><td>{mark}</td><td>{}</td><td>{}</td></tr>",
                escape_html(check),
                escape_html(detail)
            )
        })
        .collect();
    nav_page_sub(
        runtime,
        "/doctor",
        "Doctor",
        "The in-process health checks: config, model key, store, data-root permissions, web publication and channel tokens.",
        format!(
            "<div class=\"tablewrap\"><table><tr><th></th><th>check</th><th>detail</th></tr>{html}</table></div>\
             <form method=post action=/doctor/fix style=\"margin:.6rem 0\">\
             <button>apply safe fixes (0700 roots, 0600 secrets)</button></form>\
             <h2>Full doctor</h2>{}",
            full_doctor(runtime).unwrap_or_else(|| {
                "<div class=\"empty\">the full doctor could not run here</div>".to_string()
            })
        ),
    )
}

/// `GET /status`: the operator report plus the settings with the CLI that
/// changes each.
pub fn status(runtime: &Runtime, _who: &ka_agent_core::Principal) -> crate::Response {
    let report = match runtime.status_report_for(&op()) {
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
    let usd = match report.limit_usd {
        Some(cap) => format!("${:.4} / ${cap:.2}", report.spent_usd),
        None => format!("${:.4}", report.spent_usd),
    };
    let settings = settings_form(runtime);
    let usd_bar = report.limit_usd.map(|cap| {
        let pct = (report.spent_usd / cap * 100.0).clamp(0.0, 100.0);
        format!(
            "<div class=\"bar\"><i style=\"width:{pct:.1}%\"></i></div>\
             <div class=\"muted small\">${:.4} of ${cap:.2} today</div>",
            report.spent_usd
        )
    });
    let token_bar = report.limit_tokens.map(|cap| {
        let pct = (report.spent_tokens as f64 / cap as f64 * 100.0).clamp(0.0, 100.0);
        format!(
            "<div class=\"bar\"><i style=\"width:{pct:.1}%\"></i></div>\
             <div class=\"muted small\">{} of {cap} tokens today</div>",
            report.spent_tokens
        )
    });
    let stderr = runtime.worker_stderr_tail();
    let stderr_html = if stderr.is_empty() {
        String::new()
    } else {
        format!(
            "<h2>Worker stderr</h2><details><summary class=\"muted\">last {} line(s)</summary><pre>{}</pre></details>",
            stderr.len(),
            escape_html(&stderr.join("\n"))
        )
    };
    let state = card(
        "state",
        format!(
            "● {}{}",
            escape_html(&report.status.to_lowercase()),
            if report.private { " · private" } else { "" }
        ),
    );
    let model = card(
        "model",
        escape_html(cfg.model.as_deref().unwrap_or("(none)")),
    );
    let mode = card(
        "mode / effort",
        format!(
            "{} / {}",
            escape_html(&report.mode),
            escape_html(runtime.effort(&op()).as_deref().unwrap_or("default"))
        ),
    );
    let today = card("today", escape_html(&usd));
    let queue = card(
        "queue / approvals",
        format!("{} / {}", report.queue, report.pending_approvals),
    );
    let memory = card(
        "memory",
        report
            .memory
            .as_ref()
            .map(|m| escape_html(&m.line()))
            .unwrap_or_else(|| "—".into()),
    );
    let worker = card(
        "worker",
        if report.worker_running {
            "running".into()
        } else {
            "stopped (starts with your next message)".into()
        },
    );
    let role = card(
        "role / tools",
        format!(
            "{} · {} tools",
            escape_html(&op().role),
            runtime.tools(&op()).len()
        ),
    );
    let budget = format!(
        "{}{}",
        usd_bar
            .map(|b| format!(
                "<div class=\"card\"><div class=\"cardhead\"><span class=\"k\">budget (USD)</span></div>{b}</div>"
            ))
            .unwrap_or_default(),
        token_bar
            .map(|b| format!(
                "<div class=\"card\"><div class=\"cardhead\"><span class=\"k\">budget (tokens)</span></div>{b}</div>"
            ))
            .unwrap_or_default()
    );
    let body = format!(
        "<div class='statgrid'>\
         {state}{model}{mode}{today}{queue}{memory}{worker}{role}\
         </div>\
         {budget}\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">backup</span><span class=\"tag\">state.db + memory + notes + config</span></div>\
         <form method=post action=/backup><button class=\"primary\">backup now</button></form>\
         <p class=\"muted small\">Writes <code>&lt;data&gt;/backups/&lt;timestamp&gt;</code> and prunes to the newest 7 — the same snapshot <code>ka-agent backup</code> takes. Restore stays in the CLI.</p></div>\
         <h2>settings</h2>\
         {settings}\
         <h2>channels</h2><ul>{channels}</ul>{stderr_html}"
    );
    nav_page_sub(
        runtime,
        "/status",
        "Status",
        "Session, model, spend, queue, worker and channels at a glance.",
        body,
    )
}

/// The settings form: every `KEYS` entry marked `web`, its control chosen
/// by kind, the current value read from the config file, and the key's
/// apply timing beside it (one registry, one form).
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
                        "<option value='{v}'{}>{v}</option>",
                        if current == *v { " selected" } else { "" }
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
            Kind::Int { min, max } => format!(
                "<input name='{field}' type='number' min='{min}' max='{max}' value='{}' placeholder='{}'>",
                escape_html(&current),
                escape_html(spec.default)
            ),
            Kind::Float { min } => format!(
                "<input name='{field}' type='number' step='any' min='{min}' value='{}' placeholder='{}'>",
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
            "<tr><td><code>{}</code><div class='muted small'>{}</div></td>\
             <td>{control}</td><td class='muted small nowrap'>{}</td></tr>",
            escape_html(spec.path),
            escape_html(spec.doc),
            spec.apply.label()
        ));
    }
    format!(
        "<div class=\"card\"><div class=\"cardhead\"><span class=\"k\">editable here</span><span class=\"tag\">web-safe keys</span></div>\
         <form method='post' action='/settings'><div class=\"tablewrap\"><table>{rows}</table></div>\
         <p><button class='primary'>save</button> \
         <span class='muted small'>empty fields are left alone; each key shows when it applies</span></p></form></div>"
    )
}

/// `GET /profiles`: profiles on disk, the configured roles, and the linked
/// identities with their role (ADR-0002).
pub fn profiles(runtime: &Runtime, store: &Store) -> crate::Response {
    let cfg = runtime.config();
    let active = ka_agent_core::layout::profile_name();
    let profiles = ka_agent_core::layout::list_profiles();
    let mut profile_rows = String::new();
    for name in &profiles {
        let mark = if *name == active {
            " <span class=\"tag\">active</span>"
        } else {
            ""
        };
        let mut actions = format!(
            "<form method=post action=/profiles/use style=display:inline>\
             <input type=hidden name=name value=\"{}\"><button class=sm>use</button></form>",
            escape_html(name)
        );
        if *name != active {
            actions.push_str(&format!(
                "<form method=post action=/profiles/remove style=display:inline>\
                 <input type=hidden name=name value=\"{}\">\
                 <input name=confirm placeholder=\"type name\" size=8>\
                 <button class=\"sm danger\">remove</button></form>",
                escape_html(name)
            ));
        }
        profile_rows.push_str(&format!(
            "<tr><td><code>{}</code>{mark}</td><td class=\"actions\">{actions}</td></tr>",
            escape_html(name)
        ));
    }
    if profile_rows.is_empty() {
        profile_rows.push_str("<tr><td class='muted' colspan='2'>no profiles on disk</td></tr>");
    }
    let roles: String = cfg
        .roles
        .iter()
        .map(|r| {
            format!(
                "<tr><td><a href=\"/roles/edit/{}\"><code>{}</code></a></td><td>{}</td><td>{}</td><td class=\"nowrap\">{}</td></tr>",
                escape_html(&r.name),
                escape_html(&r.name),
                escape_html(&r.mode),
                if r.allow.is_empty() {
                    "—".to_string()
                } else {
                    escape_html(&r.allow.join(", "))
                },
                r.budget_daily_usd
                    .map(|u| format!("${u:.2}"))
                    .unwrap_or_else(|| "profile".into())
            )
        })
        .collect();
    let mut role_options = String::new();
    for name in std::iter::once(ka_agent_core::ADMIN_ROLE.to_string())
        .chain(cfg.roles.iter().map(|r| r.name.clone()))
    {
        role_options.push_str(&format!("<option value='{name}'>{name}</option>"));
    }
    let identities = store.list_identities().unwrap_or_default();
    let mut identity_rows = String::new();
    for id in &identities {
        let options: String = std::iter::once(ka_agent_core::ADMIN_ROLE.to_string())
            .chain(cfg.roles.iter().map(|r| r.name.clone()))
            .map(|name| {
                format!(
                    "<option value='{name}'{}>{name}</option>",
                    if id.role == name { " selected" } else { "" }
                )
            })
            .collect();
        identity_rows.push_str(&format!(
            "<tr><td><code>{}:{}</code></td><td><form method='post' action='/identities/role'>\
             <input type='hidden' name='channel' value='{}'>\
             <input type='hidden' name='address' value='{}'>\
             <select name='role'>{options}</select> <button class=sm>set</button></form>\
             <form method=post action=/identities/unlink style=display:inline>\
             <input type=hidden name=channel value='{}'>\
             <input type=hidden name=address value='{}'>\
             <button class=\"sm danger\">unlink</button></form></td></tr>",
            escape_html(&id.channel),
            escape_html(&id.address),
            escape_html(&id.channel),
            escape_html(&id.address),
            escape_html(&id.channel),
            escape_html(&id.address)
        ));
    }
    if identity_rows.is_empty() {
        identity_rows.push_str("<tr><td class='muted' colspan='2'>no identities linked</td></tr>");
    }
    let body = format!(
        "<div>\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">profiles</span><span class=\"tag\">{count}</span></div>\
         <div class=\"tablewrap\"><table><tr><th>name</th><th>actions</th></tr>{profile_rows}</table></div>\
         <form method=post action=/profiles/create>\
         <label class=field><span>new profile</span><input name=name placeholder=\"work\" required pattern=\"[a-z0-9-]{{1,32}}\"></label>\
         <label class=field><span>copy the config from</span><input name=from placeholder=\"default\"></label>\
         <button class=primary>create profile</button></form>\
         <p class=\"muted small\">A one-time config copy (minus channels) that keeps roles and <code>[tools]</code> and claims a free web port.</p></div>\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">roles (config)</span><span class=\"tag\">{role_count}</span></div>\
         <div class=\"tablewrap\"><table><tr><th>name</th><th>mode</th><th>allow</th><th>budget/day</th></tr>{roles}</table></div>\
         <p class=\"muted small\">Roles are defined in <code>ka-agent.toml</code> (<code>[[role]]</code>); the editor writes that file and the daemon hot-reloads it.</p>\
         <p><a class=\"btn\" href=/roles/new>new role</a></p>\
         <form method=post action=/tools/disable>\
         <label class=field><span>[tools] disable — profile-wide engine baseline (also admin's)</span>\
         <input name=specs value=\"{tools}\" placeholder=\"tool:bash, mcp:github\"></label>\
         <button>save tools baseline</button></form></div>\
         </div>\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">linked identities</span><span class=\"tag\">{identity_count}</span></div>\
         <div class=\"tablewrap\"><table><tr><th>identity</th><th>role</th></tr>{identity_rows}</table></div>\
         <p class=\"muted small\">Link from the terminal with <code>ka-agent identity link &lt;channel&gt; &lt;address&gt; --role &lt;role&gt;</code>, or here:</p>\
         <form method=post action=/identities/link class=formrow>\
         <input name=channel placeholder=telegram size=9 required pattern=\"[a-z0-9-]+\">\
         <input name=address placeholder=\"12345 / @user:domain\" style=\"flex:1\" required>\
         <select name=role>{role_options}</select> <button class=primary>link identity</button></form></div>\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">new web user</span></div>\
         <form method=post action=/pair/issue class=formrow>\
         <select name=role>{role_options}</select> <button class=primary>issue pairing code</button></form>\
         <p class=\"muted small\">Open <code>/pair</code> in that browser and enter the code; it gets that role's chat only (10 minutes, one use).</p></div>",
        count = profiles.len(),
        role_count = cfg.roles.len(),
        identity_count = identities.len(),
        tools = escape_html(&cfg.tools.disable.join(", "))
    );
    nav_page_sub(
        runtime,
        "/profiles",
        "Profiles & roles",
        "Profiles on disk, the configured roles, and who is linked as which role.",
        body,
    )
}

/// A comma-separated form field into a list (empty entries dropped).
fn csv_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// `GET /roles/new` and `GET /roles/edit/<name>`: the role editor. Roles
/// are config (`[[role]]`), so the form writes the same file the CLI edits
/// and the daemon's hot reload applies it.
pub fn role_edit(runtime: &Runtime, name: Option<&str>) -> crate::Response {
    let cfg = runtime.config();
    let existing = name.and_then(|n| cfg.roles.iter().find(|r| r.name == n).cloned());
    if name.is_some() && existing.is_none() {
        return result_page(
            runtime,
            "/profiles",
            "Profiles & roles",
            "no such role in the config",
            false,
        );
    }
    let editing = existing.is_some();
    let role = existing
        .clone()
        .unwrap_or_else(|| ka_agent_core::config::RoleSpec {
            name: String::new(),
            mode: "ask".into(),
            ..Default::default()
        });
    let mode_options: String = ka_agent_channels::commands::MODES
        .iter()
        .map(|(m, desc)| {
            format!(
                "<option value=\"{m}\"{}>{m} — {desc}</option>",
                if role.mode == *m { " selected" } else { "" }
            )
        })
        .collect();
    let name_field = if editing {
        format!(
            "<input name=name value=\"{}\" readonly><input type=hidden name=original value=\"{}\">",
            escape_html(&role.name),
            escape_html(&role.name)
        )
    } else {
        "<input name=name placeholder=\"guest\" required pattern=\"[a-z0-9-]{1,32}\"><input type=hidden name=original value=\"\">".to_string()
    };
    let delete = if editing {
        format!(
            "<form method=post action=/roles/delete style=\"margin-top:.6rem\">\
             <input type=hidden name=name value=\"{}\">\
             <label class=field><span>force (a role still bound to identities is refused without it)</span>\
             <input name=force value=\"\" placeholder=\"1\"></label>\
             <button class=\"danger\">delete role</button></form>",
            escape_html(&role.name)
        )
    } else {
        String::new()
    };
    let body = format!(
        "<div class=\"card\" style=\"max-width:42rem\">\
         <form method=post action=/roles/save>\
         <label class=field><span>name</span>{name_field}</label>\
         <label class=field><span>mode (ceiling and default)</span><select name=mode>{mode_options}</select></label>\
         <label class=field><span>allow — exact hand names, comma-separated (or use disable)</span>\
         <input name=allow value=\"{}\" placeholder=\"read, grep, kagent__notes_search\"></label>\
         <label class=field><span>disable — engine feature specs (subtractive; not with allow)</span>\
         <input name=disable value=\"{}\" placeholder=\"tool:bash, mcp:github\"></label>\
         <label class=field><span>workspace (empty = <code>&lt;data&gt;/roles/&lt;name&gt;/workspace</code>)</span>\
         <input name=workspace value=\"{}\" placeholder=\"~/projects/shared\"></label>\
         <label class=field><span>budget per day (USD, empty = the profile budget)</span>\
         <input name=budget type=number step=any min=0 value=\"{}\"></label>\
         <label class=field><span>commands (empty = all the role may reach; help/status always)</span>\
         <input name=commands value=\"{}\" placeholder=\"status, help, mode, skills, history, memory, cron\"></label>\
         <p><button class=primary>save role</button> <a href=\"/profiles\">cancel</a></p></form>\
         {delete}</div>",
        escape_html(&role.allow.join(", ")),
        escape_html(&role.disable.join(", ")),
        escape_html(
            &role
                .workspace
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ),
        role.budget_daily_usd
            .map(|u| u.to_string())
            .unwrap_or_default(),
        escape_html(&role.commands.join(", ")),
    );
    nav_page_sub(
        runtime,
        "/profiles",
        if editing { "Edit role" } else { "New role" },
        "A role is config policy: session, memory, tools, grants, budget and commands. Identities bind to it.",
        body,
    )
}

/// `POST /roles/save`: validate and write `[[role]]` through the core
/// config editor (the whole file is re-validated before it is written).
pub async fn role_save(runtime: &Runtime, body: &str) -> crate::Response {
    let original = form_field(body, "original");
    let name = form_field(body, "name").trim().to_string();
    if !ka_agent_core::valid_role_name(&name) {
        return result_page(
            runtime,
            "/profiles",
            "Profiles & roles",
            "role name must be [a-z0-9-], 1..=32",
            false,
        );
    }
    if !original.is_empty() && original != name {
        return result_page(
            runtime,
            "/profiles",
            "Profiles & roles",
            "renaming a role is not supported (identities and crons reference it)",
            false,
        );
    }
    let Some(path) = runtime.config_path() else {
        return result_page(
            runtime,
            "/profiles",
            "Profiles & roles",
            "no config file to edit",
            false,
        );
    };
    let cfg = runtime.config();
    let mut roles = cfg.roles.clone();
    let mut role = if original.is_empty() {
        ka_agent_core::config::RoleSpec {
            name: name.clone(),
            grants: Default::default(),
            ..Default::default()
        }
    } else {
        match roles.iter().find(|r| r.name == original) {
            Some(r) => r.clone(),
            None => {
                return result_page(
                    runtime,
                    "/profiles",
                    "Profiles & roles",
                    "no such role any more",
                    false,
                );
            }
        }
    };
    let mode = form_field(body, "mode");
    role.mode = if mode.is_empty() { "ask".into() } else { mode };
    role.allow = csv_list(&form_field(body, "allow"));
    role.disable = csv_list(&form_field(body, "disable"));
    let workspace = form_field(body, "workspace").trim().to_string();
    role.workspace = (!workspace.is_empty()).then(|| std::path::PathBuf::from(workspace));
    let budget = form_field(body, "budget").trim().to_string();
    role.budget_daily_usd = if budget.is_empty() {
        None
    } else {
        match budget.parse::<f64>() {
            Ok(v) => Some(v),
            Err(_) => {
                return result_page(
                    runtime,
                    "/profiles",
                    "Profiles & roles",
                    "budget is not a number",
                    false,
                );
            }
        }
    };
    role.commands = csv_list(&form_field(body, "commands"));
    if original.is_empty() {
        roles.push(role);
    } else if let Some(slot) = roles.iter_mut().find(|r| r.name == original) {
        *slot = role;
    }
    match ka_agent_core::config_file::set_roles(&path, &roles) {
        Ok(new_cfg) => {
            runtime.reload_config(new_cfg, false).await;
            result_page(
                runtime,
                "/profiles",
                "Profiles & roles",
                &format!("role {name} saved — workers pick it up with the next message"),
                true,
            )
        }
        Err(e) => result_page(
            runtime,
            "/profiles",
            "Profiles & roles",
            &e.to_string(),
            false,
        ),
    }
}

/// `POST /roles/delete`: refuse while identities or crons still reference
/// the role unless `force=1`.
pub async fn role_delete(runtime: &Runtime, body: &str) -> crate::Response {
    let name = form_field(body, "name");
    let force = form_field(body, "force") == "1";
    let Some(path) = runtime.config_path() else {
        return result_page(
            runtime,
            "/profiles",
            "Profiles & roles",
            "no config file to edit",
            false,
        );
    };
    let cfg = runtime.config();
    let mut roles = cfg.roles.clone();
    let Some(index) = roles.iter().position(|r| r.name == name) else {
        return result_page(
            runtime,
            "/profiles",
            "Profiles & roles",
            "no such role",
            false,
        );
    };
    let identities = runtime
        .store()
        .list_identities()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| i.role == name)
        .count();
    let crons = runtime
        .list_crons()
        .unwrap_or_default()
        .into_iter()
        .filter(|t| {
            t.payload
                .as_deref()
                .and_then(ka_agent_core::CronSpec::from_payload)
                .and_then(|s| s.role)
                .as_deref()
                == Some(name.as_str())
        })
        .count();
    if (identities > 0 || crons > 0) && !force {
        return result_page(
            runtime,
            "/profiles",
            "Profiles & roles",
            &format!(
                "role {name} is still bound to {identities} identity(ies) and {crons} cron(s) — reassign/remove them or submit force=1"
            ),
            false,
        );
    }
    roles.remove(index);
    match ka_agent_core::config_file::set_roles(&path, &roles) {
        Ok(new_cfg) => {
            runtime.reload_config(new_cfg, false).await;
            result_page(
                runtime,
                "/profiles",
                "Profiles & roles",
                &format!("role {name} deleted"),
                true,
            )
        }
        Err(e) => result_page(
            runtime,
            "/profiles",
            "Profiles & roles",
            &e.to_string(),
            false,
        ),
    }
}

/// `POST /tools/disable`: the profile-wide engine baseline (also admin's).
pub async fn tools_disable(runtime: &Runtime, body: &str) -> crate::Response {
    let Some(path) = runtime.config_path() else {
        return result_page(
            runtime,
            "/profiles",
            "Profiles & roles",
            "no config file to edit",
            false,
        );
    };
    let specs = csv_list(&form_field(body, "specs"));
    match ka_agent_core::config_file::set_tools_disable(&path, &specs) {
        Ok(new_cfg) => {
            runtime.reload_config(new_cfg, false).await;
            result_page(
                runtime,
                "/profiles",
                "Profiles & roles",
                &format!("tools.disable has {} spec(s)", specs.len()),
                true,
            )
        }
        Err(e) => result_page(
            runtime,
            "/profiles",
            "Profiles & roles",
            &e.to_string(),
            false,
        ),
    }
}

/// `POST /identities/link`: bind a channel identity to a role (the CLI
/// `ka-agent identity link … --role …`).
pub fn identity_link(runtime: &Runtime, body: &str) -> crate::Response {
    let channel = form_field(body, "channel").trim().to_string();
    let address = form_field(body, "address").trim().to_string();
    let role = form_field(body, "role").trim().to_string();
    let channel_ok = !channel.is_empty()
        && channel
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !channel_ok || address.is_empty() || address.chars().any(char::is_control) {
        return result_page(
            runtime,
            "/profiles",
            "Profiles & roles",
            "channel/address are not valid",
            false,
        );
    }
    let cfg = runtime.config();
    if role != ka_agent_core::ADMIN_ROLE && !cfg.roles.iter().any(|r| r.name == role) {
        return result_page(
            runtime,
            "/profiles",
            "Profiles & roles",
            "role is not defined in the config",
            false,
        );
    }
    match runtime
        .store()
        .link_identity_as(&channel, &address, "web", &role)
    {
        Ok(true) => {
            let mut rec = ka_agent_core::AuditRecord::new(
                "operator",
                ka_agent_core::audit::actions::IDENTITY_LINK,
                &format!("{channel}:{address}"),
                Some(format!("role {role}")),
            )
            .with_role(ka_agent_core::ADMIN_ROLE);
            let _ = runtime.store().append_audit(&mut rec);
            result_page(
                runtime,
                "/profiles",
                "Profiles & roles",
                &format!("linked {channel}:{address} as role {role}"),
                true,
            )
        }
        Ok(false) => result_page(
            runtime,
            "/profiles",
            "Profiles & roles",
            &format!("{channel}:{address} was already linked (use set role to change it)"),
            false,
        ),
        Err(e) => result_page(
            runtime,
            "/profiles",
            "Profiles & roles",
            &e.to_string(),
            false,
        ),
    }
}

/// `POST /identities/unlink`: remove one binding (audited).
pub fn identity_unlink(runtime: &Runtime, body: &str) -> crate::Response {
    let channel = form_field(body, "channel");
    let address = form_field(body, "address");
    match runtime.store().unlink_identity(&channel, &address) {
        Ok(true) => {
            let mut rec = ka_agent_core::AuditRecord::new(
                "operator",
                ka_agent_core::audit::actions::IDENTITY_UNLINK,
                &format!("{channel}:{address}"),
                None,
            )
            .with_role(ka_agent_core::ADMIN_ROLE);
            let _ = runtime.store().append_audit(&mut rec);
            result_page(
                runtime,
                "/profiles",
                "Profiles & roles",
                &format!("unlinked {channel}:{address}"),
                true,
            )
        }
        Ok(false) => result_page(
            runtime,
            "/profiles",
            "Profiles & roles",
            "no such identity",
            false,
        ),
        Err(e) => result_page(
            runtime,
            "/profiles",
            "Profiles & roles",
            &e.to_string(),
            false,
        ),
    }
}

/// `POST /identities/role`: reassign one identity's role (audited with the
/// admin role, like the CLI).
pub fn identity_role(store: &Store, body: &str) -> crate::Response {
    let channel = form_field(body, "channel");
    let address = form_field(body, "address");
    let role = form_field(body, "role");
    if channel.is_empty() || address.is_empty() || role.trim().is_empty() {
        return Response::bad_request("channel, address and role are required");
    }
    match store.set_identity_role(&channel, &address, role.trim()) {
        Ok(_) => {
            let mut rec = ka_agent_core::AuditRecord::new(
                "operator",
                ka_agent_core::audit::actions::IDENTITY_ROLE,
                &format!("{channel}:{address}"),
                Some(format!("role → {}", role.trim())),
            )
            .with_role(ka_agent_core::ADMIN_ROLE);
            let _ = store.append_audit(&mut rec);
            Response::see_other("/profiles")
        }
        Err(e) => Response::json(serde_json::json!({"error": e.to_string()}).to_string()),
    }
}

/// The largest upload accepted from the web chat (bytes). The request
/// parser allows a little more so the route can answer 413 cleanly.
const UPLOAD_MAX: usize = 8 * 1024 * 1024;

/// A safe file base name: no separators, `[A-Za-z0-9._-]`, 1..=80 chars,
/// not a dot-only name.
fn upload_name(raw: &str) -> Option<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let mut out = String::new();
    for c in base.chars() {
        out.push(
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            },
        );
    }
    let out = out.trim_matches('.').to_string();
    if out.is_empty() {
        return None;
    }
    Some(out.chars().take(80).collect())
}

/// `POST /upload`: one file from the chat composer, stored under the
/// caller role's `attachments/`; the reply is the `@attachments/…`
/// reference to paste into the message.
pub fn web_upload(
    runtime: &Runtime,
    role: &str,
    username: &str,
    raw_name: &str,
    bytes: &[u8],
) -> crate::Response {
    if bytes.is_empty() {
        return Response::bad_request("empty upload");
    }
    if bytes.len() > UPLOAD_MAX {
        return Response::base(413, "text/plain", "file too large (max 8 MiB)\n".into());
    }
    let Some(name) = upload_name(raw_name) else {
        return Response::bad_request("bad file name");
    };
    let who = ka_agent_core::Principal::new(username, role);
    let dir = runtime.workspace_for(&who).join("attachments");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Response::base(500, "text/plain", format!("attachments: {e}\n"));
    }
    let stored = format!("web-{}-{name}", &crate::random_hex(4)[..8]);
    if let Err(e) = std::fs::write(dir.join(&stored), bytes) {
        return Response::base(500, "text/plain", format!("write: {e}\n"));
    }
    Response::json(
        serde_json::json!({
            "ok": true,
            "name": stored,
            "ref": format!("@attachments/{stored}"),
            "size": bytes.len(),
        })
        .to_string(),
    )
}

/// The content type (and whether a browser may render it inline) for a
/// served attachment.
fn attachment_type(name: &str) -> (&'static str, bool) {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => ("image/png", true),
        "jpg" | "jpeg" => ("image/jpeg", true),
        "webp" => ("image/webp", true),
        "gif" => ("image/gif", true),
        "pdf" => ("application/pdf", false),
        "txt" | "log" => ("text/plain; charset=utf-8", true),
        "md" => ("text/markdown; charset=utf-8", true),
        "json" => ("application/json", true),
        "csv" => ("text/csv; charset=utf-8", false),
        "zip" => ("application/zip", false),
        "mp3" | "ogg" | "opus" | "m4a" => ("audio/mpeg", true),
        "mp4" | "webm" => ("video/mp4", true),
        _ => ("application/octet-stream", false),
    }
}

/// `GET /file/<name>`: one attachment of the caller's role. Only direct
/// children of that role's `attachments/` are served (traversal and
/// symlinks are refused).
pub fn serve_file(
    runtime: &Runtime,
    role: &str,
    username: &str,
    raw: &str,
) -> crate::Response {
    let Some(name) = upload_name(raw) else {
        return Response::bad_request("bad file name");
    };
    // Reject spellings that only normalize to a name (a traversal attempt).
    if name != raw {
        return Response::bad_request("bad file name");
    }
    // A direct child of the role's `attachments/` or of its workspace
    // (where `send_file` may point). Nothing deeper, no symlink escapes.
    let who = ka_agent_core::Principal::new(username, role);
    let ws = runtime.workspace_for(&who);
    let mut found = None;
    for dir in [ws.join("attachments"), ws.clone()] {
        let Ok(canon_dir) = std::fs::canonicalize(&dir) else {
            continue;
        };
        let Ok(path) = std::fs::canonicalize(canon_dir.join(&name)) else {
            continue;
        };
        if path.starts_with(&canon_dir) && path.is_file() {
            found = Some(path);
            break;
        }
    }
    let Some(path) = found else {
        return Response::not_found();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return Response::not_found();
    };
    let (content_type, inline) = attachment_type(&name);
    let mut resp = Response::bytes(content_type, bytes);
    resp.extra_headers.push((
        "Content-Disposition",
        format!(
            "{}; filename=\"{}\"",
            if inline { "inline" } else { "attachment" },
            name
        ),
    ));
    resp
}

pub fn pair_issue(runtime: &Runtime, store: &Store, body: &str) -> crate::Response {
    let cfg = runtime.config();
    let role = form_field(body, "role").trim().to_string();
    if role != ka_agent_core::ADMIN_ROLE && !cfg.roles.iter().any(|r| r.name == role) {
        return Response::bad_request("role is not defined in the config");
    }
    let code = match ka_agent_channels::pairing::issue_for_role(store, "web", Some(&role)) {
        Ok(code) => code,
        Err(e) => {
            return Response::base(500, "text/plain", format!("pairing: {e}\n"));
        }
    };
    let mut rec = ka_agent_core::AuditRecord::new(
        "operator",
        ka_agent_core::audit::actions::IDENTITY_PAIR,
        "web (pairing code issued)",
        Some(format!("role {role}")),
    )
    .with_role(ka_agent_core::ADMIN_ROLE);
    let _ = store.append_audit(&mut rec);
    let base = store
        .meta_get("web.url")
        .ok()
        .flatten()
        .filter(|u| !u.is_empty())
        .map(|u| match u.split_once("://") {
            Some((scheme, rest)) => {
                let authority = rest.split('/').next().unwrap_or(rest);
                format!("{scheme}://{authority}")
            }
            None => u,
        })
        .unwrap_or_else(|| "http://127.0.0.1:8480".to_string());
    let body = format!(
        "<div class='card' style='max-width:34rem'>         <div class='k'>role {role}</div>         <p style='font-size:1.6rem;letter-spacing:.08em'><code>{code}</code></p>         <p>Open <code>{base}/pair</code> in the browser and enter it.          The code is valid for 10 minutes and works once.</p>         <p class='muted'>Losing the browser cookie means issuing a new code.</p>         <p><a href='/profiles'>back to profiles</a></p></div>",
        role = escape_html(&role),
        code = escape_html(&code),
        base = escape_html(&base)
    );
    nav_page(runtime, "/profiles", "Pairing code", body)
}

/// `POST /profiles/create|use`: run the operator CLI (no shell) and report
/// its output. Profiles are a global concept; a pinned data root refuses.
pub async fn profile_action(action: &str, body: &str) -> crate::Response {
    if std::env::var_os("KA_AGENT_DATA_DIR").is_some_and(|v| !v.is_empty()) {
        return Response::forbidden(
            "profiles are managed outside a pinned KA_AGENT_DATA_DIR — use the CLI",
        );
    }
    let name = form_field(body, "name").trim().to_string();
    if !ka_agent_core::layout::valid_profile_name(&name) {
        return Response::bad_request("profile name must be [a-z0-9-], up to 32 chars");
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| "ka-agent".into());
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("profile")
        .arg(action)
        .arg(&name)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if action == "create" {
        let from = form_field(body, "from");
        let from = from.trim();
        if !from.is_empty() {
            cmd.arg("--from").arg(from);
        }
    }
    if action == "remove" {
        // Destructive: the name typed twice (the form asks for it).
        if form_field(body, "confirm").trim() != name {
            return Response::bad_request(
                "removing a profile deletes its data — type the profile name to confirm",
            );
        }
        cmd.arg("--yes");
    }
    let Ok(out) = cmd.output().await else {
        return Response::base(
            500,
            "text/plain",
            "profile command failed to start\n".into(),
        );
    };
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Response::base(400, "text/plain", format!("{msg}\n"));
    }
    Response::see_other("/profiles")
}

/// `GET /`: the chat page (last 20 turns, live updates via /events).
pub fn chat_page(runtime: &Runtime, store: &Store) -> crate::Response {
    chat_page_user(runtime, store, &ka_agent_core::Principal::operator())
}

/// The chat page for one user: their own history, their own composer.
pub fn chat_page_user(
    runtime: &Runtime,
    store: &Store,
    who: &ka_agent_core::Principal,
) -> crate::Response {
    let _ = store;
    let body = chat_markup(runtime, who);
    if who.is_admin() {
        // The operator's panel: full navigation.
        return nav_page(runtime, "/", "Chat", body);
    }
    // A member reaches the conversation only: no operator navigation, just
    // the brand and the chat itself (same chrome the role realm had).
    let shell = format!(
        "<div class=\"layout\"><aside><div class=\"brand\">{}<span class=\"who\">{}</span></div></aside>\
         <main><div class=\"pagehead\"><div><h1>Chat</h1></div></div>{body}</main></div>",
        escape_html(&runtime.persona_name()),
        escape_html(&who.username)
    );
    Response::html(html_page(&runtime.persona_name(), "Chat", shell))
}

/// Rendered when another role's turn owns the single-session core: no
/// history, no cross-role data.
pub fn chat_page_busy(user: &str) -> crate::Response {
    let body = format!(
        "<main><h1>Chat</h1><div class='card'>your previous message is still running — the next one queues behind it ({})</div></main>",
        escape_html(user)
    );
    crate::Response::html(html_page("Ka Agent", "Chat", body))
}

fn chat_markup(runtime: &Runtime, who: &ka_agent_core::Principal) -> String {
    let now = runtime.now_zoned();
    let mut turns_html = String::new();
    if let Ok(turns) = runtime.recent_turns(20, 0, who) {
        if turns.is_empty() {
            // A blank first visit says nothing: one hint bubble, the same
            // shape every other empty state in the panel uses.
            turns_html.push_str(
                "<div class='bubble system'>Ka is ready — say hi; commands start with /</div>",
            );
        }
        for (t, answer) in turns.iter().rev() {
            // A private conversation's usage-only row carries no text —
            // render nothing rather than a pair of blank bubbles.
            if t.inbound_text.is_empty() && answer.is_empty() {
                continue;
            }
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
    let mode = runtime.effective_mode_for(&who.username);
    let model = runtime
        .model_for(who, false)
        .unwrap_or_else(|| "no model".into());
    let approvals = runtime
        .pending_approval_cards(who)
        .map(|c| c.len())
        .unwrap_or(0);
    let waiting = if approvals > 0 {
        format!("<a class='chip warn' href='/approvals'>{approvals} approval(s) waiting</a>")
    } else {
        String::new()
    };
    format!(
        "<div id='chatbar'><span class='chip'>{persona} · {role}</span>\
         <span class='chip'>mode <b id='chip-mode'>{mode}</b></span>\
         <span class='chip' id='chip-model'>{model}</span>\
         <span class='chip' id='chip-ctx' hidden>ctx</span>{waiting}\
         <span class='spacer'></span>\
         <button type='button' data-cmd='/new' title='start a fresh session'>new</button>\
         <button type='button' data-cmd='/retry' title='re-send the last failed turn'>retry</button>\
         <button type='button' data-cmd='/stop' title='abort the running reply'>stop</button>\
         <button type='button' data-cmd='/undo' title='revert the last file change of this session'>undo</button></div>\
         <div id='chat'><div id='log'>{turns_html}</div>\
         <form id='send'><textarea id='text' rows='2' placeholder='talk to ka (Enter sends, Shift+Enter newline)'></textarea>\
         <label class='filebtn' title='attach a file (up to 8 MiB)'>attach<input type='file' id='file' hidden></label>\
         <button type='button' id='private' title='start a private conversation (nothing is remembered)'>private</button><button class='primary'>send</button></form></div>\
         <script src=\"/static/chat.js\" defer></script>",
        persona = escape_html(&runtime.persona_name()),
        role = escape_html(&who.role),
        mode = escape_html(&mode),
        model = escape_html(&model),
        waiting = waiting
    )
}

/// `POST /chat/send`: a command or a message from the web chat (fetch).
pub async fn chat_send(runtime: &Runtime, store: &Store, body: &str) -> crate::Response {
    chat_send_for(
        runtime,
        store,
        body,
        "web",
        &ka_agent_core::Principal::operator(),
    )
    .await
}

/// The same for one user's browser session: every command and turn runs as
/// `source` (the user is derived from it server-side).
pub async fn chat_send_for(
    runtime: &Runtime,
    store: &Store,
    body: &str,
    source: &str,
    who: &ka_agent_core::Principal,
) -> crate::Response {
    let text = crate::url_decode(&form_field_url(body, "text").replace('+', " "));
    let text = text.trim().to_string();
    if text.is_empty() {
        return Response::bad_request("empty text");
    }
    match ka_agent_channels::commands::route(runtime, source, &text).await {
        ka_agent_channels::commands::Outcome::Reply(r) => {
            Response::json(serde_json::json!({"reply": r}).to_string())
        }
        ka_agent_channels::commands::Outcome::Silent => {
            Response::json(serde_json::json!({"ok": true}).to_string())
        }
        ka_agent_channels::commands::Outcome::NotACommand => {
            let (expanded, refs) =
                ka_agent_channels::refs::expand(store, &runtime.workspace_for(who), who, &text)
                    .await;
            ka_agent_channels::refs::stage_sources(store, source, &refs);
            match runtime.handle_message(&expanded, source).await {
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

/// `GET /tasks`: reminders and crons — create them with the same grammar
/// the chat uses, and pause/resume/trigger/remove each cron.
pub fn tasks(runtime: &Runtime, _who: &ka_agent_core::Principal) -> crate::Response {
    let now = runtime.now_zoned();
    let rows = match runtime.scheduled_tasks() {
        Ok(r) => r,
        Err(e) => {
            return result_page(runtime, "/tasks", "Tasks & cron", &e.to_string(), false);
        }
    };
    let mut table = String::new();
    for t in &rows {
        let spec = t
            .payload
            .as_deref()
            .and_then(ka_agent_core::CronSpec::from_payload);
        let next = t
            .scheduled_at_ms
            .map(|at| escape_html(&ka_agent_core::when::describe(at, &now)))
            .unwrap_or_else(|| "-".into());
        let role = spec
            .as_ref()
            .and_then(|s| s.role.clone())
            .unwrap_or_else(|| ka_agent_core::ADMIN_ROLE.to_string());
        let tools = spec
            .as_ref()
            .filter(|s| !s.tools.is_empty())
            .map(|s| format!("<code>{}</code>", escape_html(&s.tools.join(","))))
            .unwrap_or_else(|| "—".into());
        let status = format!("{:?}", t.status);
        let actions = if t.kind == "reminder" {
            format!(
                "<form method=post action=/tasks/snooze class=inline>\
                 <input type=hidden name=id value=\"{id}\">\
                 <input name=when placeholder=30m size=6><button class=sm>snooze</button></form>\
                 <form method=post action=/tasks/cancel class=inline>\
                 <input type=hidden name=id value=\"{id}\"><button class=\"sm danger\">cancel</button></form>",
                id = escape_html(t.id.as_str())
            )
        } else {
            let paused = matches!(t.status, ka_agent_core::TaskStatus::Interrupted);
            let toggle = if paused {
                "<input type=hidden name=action value=resume><button class=sm>resume</button>"
            } else {
                "<input type=hidden name=action value=pause><button class=sm>pause</button>"
            };
            format!(
                "<form method=post action=/tasks/cron class=inline>\
                 <input type=hidden name=id value=\"{id}\">{toggle}</form>\
                 <form method=post action=/tasks/cron class=inline>\
                 <input type=hidden name=id value=\"{id}\"><input type=hidden name=action value=trigger>\
                 <button class=sm>run now</button></form>\
                 <form method=post action=/tasks/cron class=inline>\
                 <input type=hidden name=id value=\"{id}\"><input type=hidden name=action value=remove>\
                 <button class=\"sm danger\">remove</button></form>",
                id = escape_html(t.id.as_str())
            )
        };
        table.push_str(&format!(
            "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td class=\"actions\">{}</td></tr>",
            escape_html(ka_agent_core::ids::short_id(t.id.as_str())),
            escape_html(&t.kind),
            escape_html(&role),
            next,
            escape_html(&t.title),
            tools,
            escape_html(&status),
            actions
        ));
    }
    let body = format!(
        "<div class=\"grid2\">\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">new cron</span></div>\
         <form method=post action=/tasks/add>\
         <input type=hidden name=kind value=cron>\
         <label class=field><span>Schedule + prompt, exactly like <code>/cron add</code></span>\
         <textarea name=args rows=3 placeholder=\"every day at 08:00 summarize my inbox\"></textarea></label>\
         <button class=primary>schedule cron</button></form>\
         <p class=\"muted small\"><code>every 1h …</code>, <code>in 10m …</code>, <code>every weekday at 9 …</code>; add <code>--tools read,grep</code> to narrow the toolset.</p></div>\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">new reminder</span></div>\
         <form method=post action=/tasks/add>\
         <input type=hidden name=kind value=reminder>\
         <label class=field><span>When + text, exactly like <code>/remind</code></span>\
         <textarea name=args rows=3 placeholder=\"tomorrow 9am call the bank\"></textarea></label>\
         <button class=primary>set reminder</button></form>\
         <p class=\"muted small\">Reminders notify; they never spend a model turn.</p></div>\
         </div>\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">calendar digest</span></div>\
         <form method=post action=/tasks/calendar class=formrow>\
         <input name=args placeholder=\"add https://…/calendar.ics  ·  today\" style=\"flex:1\">\
         <button>run /calendar</button></form>\
         <p class=\"muted small\">The same command as the chat: a daily digest at <code>calendar.digest_time</code>, or render today's.</p></div>\
         <h2>Scheduled</h2>\
         {}",
        if table.is_empty() {
            "<div class=\"empty\">nothing scheduled</div>".to_string()
        } else {
            format!(
                "<div class=\"tablewrap\"><table><tr><th>id</th><th>kind</th><th>role</th><th>next</th><th>title</th><th>tools</th><th>status</th><th></th></tr>{table}</table></div>\
                 <style>form.inline{{display:inline-flex;gap:.3rem;align-items:center;margin:.1rem}}form.inline input{{width:auto}}form.inline button{{margin:0}}</style>"
            )
        }
    );
    nav_page_sub(
        runtime,
        "/tasks",
        "Tasks & cron",
        "Scheduled prompts and reminders, with the same grammar as /cron and /remind — plus pause, resume, trigger and remove.",
        body,
    )
}

/// `POST /tasks/add`: run the router's own `/cron add` / `/remind` so the
/// web and the chat share one parser and one reply.
pub async fn tasks_add(runtime: &Runtime, body: &str) -> crate::Response {
    let kind = form_field(body, "kind");
    let args = form_field(body, "args").trim().to_string();
    if args.is_empty() {
        return result_page(
            runtime,
            "/tasks",
            "Tasks & cron",
            "the schedule is empty",
            false,
        );
    }
    let command = match kind.as_str() {
        "reminder" => format!("/remind {args}"),
        _ => format!("/cron add {args}"),
    };
    match ka_agent_channels::commands::route(runtime, "web", &command).await {
        ka_agent_channels::commands::Outcome::Reply(reply) => {
            let ok = !reply.starts_with('!');
            result_page(runtime, "/tasks", "Tasks & cron", &reply, ok)
        }
        ka_agent_channels::commands::Outcome::Silent => {
            result_page(runtime, "/tasks", "Tasks & cron", "scheduled", true)
        }
        ka_agent_channels::commands::Outcome::NotACommand => {
            result_page(runtime, "/tasks", "Tasks & cron", "not a schedule", false)
        }
    }
}

/// `POST /tasks/cron`: pause/resume/trigger/remove one cron (same code as
/// `ka-agent cron`).
/// `POST /tasks/calendar`: the router's `/calendar` (add/today).
pub async fn tasks_calendar(runtime: &Runtime, body: &str) -> crate::Response {
    let args = form_field(body, "args").trim().to_string();
    match ka_agent_channels::commands::route(runtime, "web", &format!("/calendar {args}")).await {
        ka_agent_channels::commands::Outcome::Reply(reply) => {
            let ok = !reply.starts_with('!');
            result_page(runtime, "/tasks", "Tasks & cron", &reply, ok)
        }
        _ => result_page(
            runtime,
            "/tasks",
            "Tasks & cron",
            "not a calendar command",
            false,
        ),
    }
}

pub fn tasks_cron(runtime: &Runtime, body: &str) -> crate::Response {
    use ka_agent_runtime::cron_ops;
    let id = form_field(body, "id");
    let action = form_field(body, "action");
    let task = match cron_ops::resolve(runtime.store(), &id) {
        Ok(t) => t,
        Err(e) => return result_page(runtime, "/tasks", "Tasks & cron", &e, false),
    };
    let result = match action.as_str() {
        "pause" => cron_ops::pause(runtime.store(), &task).map(|()| "paused".to_string()),
        "resume" => cron_ops::resume(runtime.store(), &task, &runtime.now_zoned()).map(|at| {
            format!(
                "resumed — next {}",
                ka_agent_core::when::describe(at, &runtime.now_zoned())
            )
        }),
        "trigger" => cron_ops::trigger(runtime.store(), &task)
            .map(|()| "armed for the next tick".to_string()),
        "remove" => cron_ops::remove(runtime.store(), &task).map(|()| "removed".to_string()),
        other => Err(format!("unknown action {other:?}")),
    };
    match result {
        Ok(msg) => result_page(runtime, "/tasks", "Tasks & cron", &msg, true),
        Err(e) => result_page(runtime, "/tasks", "Tasks & cron", &e, false),
    }
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

/// `POST /tasks/snooze`: move one reminder's next fire (`id` + `when` in
/// the `/remind` grammar: `30m`, `18:30`, `tomorrow 9am`, …). Cron rows
/// keep their own cadence.
pub fn tasks_snooze(runtime: &Runtime, body: &str) -> crate::Response {
    let id = form_field(body, "id");
    let expr = form_field(body, "when");
    let now = runtime.now_zoned();
    let tokens: Vec<&str> = expr.split_whitespace().collect();
    let Some(when) = ka_agent_core::when::parse_when(&tokens, &now) else {
        return nav_page(
            runtime,
            "/tasks",
            "Tasks",
            format!(
                "<p>could not read {} as a time — try 30m, 18:30 or tomorrow 9am</p>",
                escape_html(&expr)
            ),
        );
    };
    match runtime.snooze_reminder(&id, when.at_ms) {
        Ok(ka_agent_runtime::CancelOutcome::Cancelled(_)) => Response::see_other("/tasks"),
        Ok(ka_agent_runtime::CancelOutcome::NotFound) => nav_page(
            runtime,
            "/tasks",
            "Tasks",
            format!("<p>no reminder matches {}</p>", escape_html(&id)),
        ),
        Ok(ka_agent_runtime::CancelOutcome::Ambiguous(ids)) => nav_page(
            runtime,
            "/tasks",
            "Tasks",
            format!(
                "<p>ambiguous — matches: {}</p>",
                escape_html(&ids.join(", "))
            ),
        ),
        Err(e) => nav_page(
            runtime,
            "/tasks",
            "Error",
            format!("<p>{}</p>", escape_html(&e.to_string())),
        ),
    }
}

/// `GET /skills`: the library and which engine home has each skill — the
/// admin's and every configured role's (`ka-agent skills list/enable --role`).
pub fn skills(runtime: &Runtime) -> crate::Response {
    let cfg = runtime.config();
    let admin_home = runtime.engine_home();
    let library = ka_agent_runtime::skills::Skills::new(&admin_home).available();
    // One engine home per role (admin first).
    let mut homes: Vec<(String, std::path::PathBuf)> =
        vec![(ka_agent_core::ADMIN_ROLE.to_string(), admin_home.clone())];
    for role in &cfg.roles {
        homes.push((role.name.clone(), runtime.role_engine_home(&role.name)));
    }
    let enabled: Vec<std::collections::HashSet<String>> = homes
        .iter()
        .map(|(_, home)| {
            ka_agent_runtime::skills::Skills::new(home)
                .enabled()
                .into_iter()
                .map(|s| s.name)
                .collect()
        })
        .collect();
    let mut names: Vec<String> = library.iter().map(|s| s.name.clone()).collect();
    for set in &enabled {
        for n in set {
            if !names.contains(n) {
                names.push(n.clone());
            }
        }
    }
    names.sort();
    if names.is_empty() {
        return nav_page_sub(
            runtime,
            "/skills",
            "Skills",
            "Prompt-text capabilities provisioned into each engine home.",
            "<div class=\"empty\">no skills in the library — put them under the profile's <code>skills/</code> or <code>~/.ka-agent/skills</code></div>".into(),
        );
    }
    let mut head = String::from("<th>skill</th>");
    for (role, _) in &homes {
        head.push_str(&format!("<th>{}</th>", escape_html(role)));
    }
    let mut rows = String::new();
    for name in &names {
        let desc = library
            .iter()
            .find(|s| &s.name == name)
            .map(|s| s.description.clone())
            .unwrap_or_default();
        let mut cells = String::new();
        for (i, (role, _)) in homes.iter().enumerate() {
            let on = enabled[i].contains(name);
            let (value, class) = if on { ("0", "sm danger") } else { ("1", "sm") };
            cells.push_str(&format!(
                "<td><form method=post action=/skills/toggle>\
                 <input type=hidden name=name value=\"{}\">\
                 <input type=hidden name=role value=\"{}\">\
                 <input type=hidden name=enable value=\"{value}\">\
                 <button class=\"{class}\">{}</button></form></td>",
                escape_html(name),
                escape_html(role),
                if on { "enabled" } else { "—" }
            ));
        }
        rows.push_str(&format!(
            "<tr><td><b>{}</b>{}</td>{cells}</tr>",
            escape_html(name),
            if desc.is_empty() {
                String::new()
            } else {
                format!("<div class=\"muted small\">{}</div>", escape_html(&desc))
            }
        ));
    }
    let proposals = runtime
        .store()
        .pending_skill_candidates()
        .unwrap_or_default();
    let proposals_html = if proposals.is_empty() {
        String::new()
    } else {
        let rows: String = proposals
            .iter()
            .map(|p| {
                format!(
                    "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td><code>{}</code></td></tr>",
                    escape_html(&p.name),
                    escape_html(&p.description),
                    escape_html(ka_agent_core::ids::short_id(&p.id)),
                    escape_html(&p.id)
                )
            })
            .collect();
        format!(
            "<h2>Proposed skills</h2><p class=\"muted small\">The model proposed these; review them on the admin chat card.</p>\
             <div class=\"tablewrap\"><table><tr><th>name</th><th>description</th><th>id</th><th></th></tr>{rows}</table></div>"
        )
    };
    nav_page_sub(
        runtime,
        "/skills",
        "Skills",
        "Prompt-text capabilities. Each engine home has its own provisioning; a role sees only its own.",
        format!(
            "<div class=\"tablewrap\"><table><tr>{head}</tr>{rows}</table></div>\
             <p class=\"muted small\">The library is the profile's <code>skills/</code> first, <code>~/.ka-agent/skills</code> as fallback. Invoke with <code>/skill &lt;name&gt; &lt;text&gt;</code>.</p>{proposals_html}"
        ),
    )
}

pub fn skills_toggle(runtime: &Runtime, body: &str) -> crate::Response {
    let name = form_field(body, "name");
    let role = form_field(body, "role");
    let role = if role.is_empty() {
        ka_agent_core::ADMIN_ROLE.to_string()
    } else {
        role
    };
    let cfg = runtime.config();
    if role != ka_agent_core::ADMIN_ROLE && !cfg.roles.iter().any(|r| r.name == role) {
        return result_page(runtime, "/skills", "Skills", "unknown role", false);
    }
    let home = if role == ka_agent_core::ADMIN_ROLE {
        runtime.engine_home()
    } else {
        runtime.role_engine_home(&role)
    };
    let enable = form_field(body, "enable") == "1";
    let skills = ka_agent_runtime::skills::Skills::new(&home);
    let result = if enable {
        skills.enable(&name)
    } else {
        skills.disable(&name)
    };
    match result {
        Ok(true) => result_page(
            runtime,
            "/skills",
            "Skills",
            &format!(
                "{} {name} for role {role}",
                if enable { "enabled" } else { "disabled" }
            ),
            true,
        ),
        Ok(false) => result_page(
            runtime,
            "/skills",
            "Skills",
            &format!("no skill {name:?} in the library"),
            false,
        ),
        Err(e) => result_page(runtime, "/skills", "Skills", &e.to_string(), false),
    }
}

/// `GET /contacts`: linked identities + strangers that messaged the bot.
pub fn contacts(runtime: &Runtime, store: &Store) -> crate::Response {
    let linked = store.list_identities().unwrap_or_default();
    let linked_rows: Vec<String> = linked
        .iter()
        .map(|r| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape_html(&r.channel),
                escape_html(&r.address),
                escape_html(&r.role),
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
                "<table><tr><th>channel</th><th>address</th><th>role</th><th>via</th></tr>{}</table>",
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
    nav_page_sub(
        runtime,
        "/contacts",
        "Contacts",
        "Linked channel identities (who may talk to this profile) and strangers who tried.",
        body,
    )
}

/// `POST /memory/forget`: remove one accepted entry.
pub async fn memory_forget(runtime: &Runtime, body: &str) -> crate::Response {
    let id = form_field(body, "id");
    let operator = ka_agent_core::Principal::operator();
    match runtime.forget(&id, &operator) {
        Ok(true) => Response::see_other("/memory"),
        Ok(false) => nav_page(
            runtime,
            "/memory",
            "Memory",
            format!(
                "<p>no memory entry matches {} — it may already be gone</p>\
                 <p><a href=\"/memory\">back</a></p>",
                escape_html(&id)
            ),
        ),
        Err(e) => nav_page(
            runtime,
            "/memory",
            "Error",
            format!(
                "<p>forget not applied: {}</p><p><a href=\"/memory\">back</a></p>",
                escape_html(&e.to_string())
            ),
        ),
    }
}

/// The fetch path of the approval card buttons (chat.js).
pub async fn approvals_decide_json(
    runtime: &Runtime,
    body: &str,
    source: &str,
) -> crate::Response {
    let id = form_field(body, "id");
    let decision = match form_field(body, "decision").as_str() {
        "y" => ApprovalDecision::Allow,
        _ => ApprovalDecision::Deny,
    };
    match runtime.decide(&id, decision, source).await {
        Ok(()) => Response::json(serde_json::json!({"ok": true}).to_string()),
        Err(e) => Response::json(serde_json::json!({"error": e.to_string()}).to_string()),
    }
}

pub fn history(
    runtime: &Runtime,
    query: Option<&str>,
    who: &ka_agent_core::Principal,
) -> crate::Response {
    let form = "<form method=get action=/history class=formrow>\
         <input name=q placeholder='search turns…' style='flex:1'><button class=primary>search</button></form>";
    let Some(q) = query.filter(|q| !q.is_empty()) else {
        let now = runtime.now_zoned();
        let turns = runtime.recent_turns(10, 0, who).unwrap_or_default();
        let rows: String = turns
            .iter()
            .rev()
            .map(|(t, answer)| {
                let at = jiff::Timestamp::from_millisecond(t.created_ms)
                    .map(|ts| ts.to_zoned(now.time_zone().clone()))
                    .map(|ts| {
                        format!(
                            "{:02}-{:02} {:02}:{:02}",
                            ts.month() as u8,
                            ts.day(),
                            ts.hour(),
                            ts.minute()
                        )
                    })
                    .unwrap_or_default();
                let user: String = t.inbound_text.chars().take(90).collect();
                let reply: String = answer.chars().take(90).collect();
                format!(
                    "<tr><td><a href=\"/history/show/{}\"><code>{}</code></a></td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    escape_html(t.id.as_str()),
                    escape_html(ka_agent_core::ids::short_id(t.id.as_str())),
                    escape_html(&at),
                    escape_html(&user),
                    escape_html(&reply)
                )
            })
            .collect();
        let recent = if rows.is_empty() {
            "<div class=\"empty\">no turns in this session yet</div>".to_string()
        } else {
            format!(
                "<div class=\"tablewrap\"><table><tr><th>turn</th><th>when</th><th>question</th><th>answer</th></tr>{rows}</table></div>"
            )
        };
        return nav_page_sub(
            runtime,
            "/history",
            "History",
            "Full-text search over past turns; open a hit for the stored question and answer.",
            format!("{form}<h2>Recent turns</h2>{recent}"),
        );
    };
    match runtime.history_search(q, 20, who) {
        Ok(hits) if hits.is_empty() => {
            let body = format!(
                "{form}<p class=\"muted\">no results for {}</p>",
                escape_html(q)
            );
            nav_page_sub(
                runtime,
                "/history",
                "History",
                "Full-text search over past turns; open a hit for the stored question and answer.",
                body,
            )
        }
        Ok(hits) => {
            let rows: Vec<String> = hits
                .iter()
                .map(|h| {
                    format!(
                        "<tr><td><a href=\"/history/show/{}\"><code>{}</code></a></td><td>{}</td><td>{}</td></tr>",
                        escape_html(&h.turn),
                        escape_html(ka_agent_core::ids::short_id(&h.turn)),
                        escape_html(&h.role),
                        escape_html(&h.snippet)
                    )
                })
                .collect();
            nav_page_sub(
                runtime,
                "/history",
                "History",
                "Full-text search over past turns; open a hit for the stored question and answer.",
                format!(
                    "{form}<div class=\"tablewrap\"><table><tr><th>turn</th><th>role</th><th>snippet</th></tr>{}</table></div>",
                    rows.join("")
                ),
            )
        }
        Err(e) => nav_page_sub(
            runtime,
            "/history",
            "History",
            "",
            format!("<p class=\"bad\">{}</p>", escape_html(&e.to_string())),
        ),
    }
}

pub fn history_show(
    runtime: &Runtime,
    store: &Store,
    turn_id: &str,
    _who: &ka_agent_core::Principal,
) -> crate::Response {
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
    nav_page(runtime, "/history", "Turn", body)
}

pub fn memory(runtime: &Runtime, who: &ka_agent_core::Principal) -> crate::Response {
    let Ok(report) = runtime.memory_report_for(&op()) else {
        return nav_page(runtime, "/memory", "Memory", "<p>memory error</p>".into());
    };
    let files: Vec<String> = report
        .files
        .iter()
        .map(|f| format!("<li>{}: {} bytes</li>", escape_html(&f.name), f.bytes))
        .collect();
    let entries = runtime
        .memory_entries(who)
        .map(|entries| {
            if entries.is_empty() {
                return "<p>no accepted entries</p>".to_string();
            }
            let rows: Vec<String> = entries
                .iter()
                .take(50)
                .map(|e| {
                    format!(
                        "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
                         <td><form method=post action=/memory/forget>\
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
        .memory_compiled_text(who)
        .map(|text| format!("<pre>{}</pre>", escape_html(&text)))
        .unwrap_or_else(|e| format!("<p>compile error: {}</p>", escape_html(&e.to_string())));
    let candidates = render_candidates(runtime, who);
    let log = runtime
        .memory_log(10, who)
        .map(|rows| {
            if rows.is_empty() {
                "<p class=\"muted\">no commits yet</p>".to_string()
            } else {
                let lines: String = rows
                    .iter()
                    .map(|(hash, subject, at)| {
                        format!(
                            "<li><code>{}</code> {} <span class=\"muted small\">{}</span></li>",
                            escape_html(ka_agent_core::ids::short_id(hash)),
                            escape_html(subject),
                            escape_html(at)
                        )
                    })
                    .collect();
                format!("<ul>{lines}</ul>")
            }
        })
        .unwrap_or_else(|e| format!("<p class=\"muted\">{}</p>", escape_html(&e.to_string())));
    let body = format!(
        "<p>memory: {}B / {}B{}</p><ul>{}</ul>\
         <div class=\"grid2\">\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">remember now</span></div>\
         <form method=post action=/memory/remember>\
         <textarea name=text rows=4 placeholder=\"one durable fact — accepted immediately\"></textarea>\
         <p><button class=primary>remember</button></p></form>\
         <p class=\"muted small\">Writes facts.md, recompiles MEMORY.md for the active role and commits.</p></div>\
         <div class=\"card\"><div class=\"cardhead\"><span class=\"k\">propose for review</span></div>\
         <form method=post action=/memory/edit>\
         <textarea name=text rows=4 placeholder=\"one durable fact — queued for review\"></textarea>\
         <p><button>propose</button></p></form></div>\
         </div>\
         {candidates}\
         <h2>accepted entries</h2>{entries}\
         <h2>history</h2>{log}\
         <form method=post action=/memory/undo>\
         <button class=sm>undo the last commit</button></form>\
         <h2>compiled (injected)</h2>{compiled}",
        report.total_bytes,
        report.budget_bytes,
        if report.truncated { " (truncated)" } else { "" },
        files.join("")
    );
    nav_page_sub(
        runtime,
        "/memory",
        "Memory",
        "Durable facts per role: remember, review proposals, inspect the compiled MEMORY.md and undo a commit.",
        body,
    )
}

/// `POST /memory/remember`: accept one fact now (facts.md + commit).
pub fn memory_remember(
    runtime: &Runtime,
    body: &str,
    who: &ka_agent_core::Principal,
) -> crate::Response {
    let text = form_field(body, "text");
    if text.trim().is_empty() {
        return result_page(
            runtime,
            "/memory",
            "Memory",
            "nothing to remember (empty text)",
            false,
        );
    }
    match runtime.remember(&text, "web", who) {
        Ok(id) => result_page(
            runtime,
            "/memory",
            "Memory",
            &format!("remembered {id}"),
            true,
        ),
        Err(e) => result_page(runtime, "/memory", "Memory", &e.to_string(), false),
    }
}

/// `POST /memory/undo`: revert the last memory commit.
pub fn memory_undo(runtime: &Runtime, who: &ka_agent_core::Principal) -> crate::Response {
    match runtime.memory_undo(who) {
        Ok(subject) => result_page(
            runtime,
            "/memory",
            "Memory",
            &format!("reverted: {subject}"),
            true,
        ),
        Err(e) => result_page(runtime, "/memory", "Memory", &e.to_string(), false),
    }
}

pub fn memory_edit(
    runtime: &Runtime,
    body: &str,
    who: &ka_agent_core::Principal,
) -> crate::Response {
    let text = form_field(body, "text");
    if text.trim().is_empty() {
        return Response::html(runtime_page(
            runtime,
            "Memory",
            "<p>nothing to remember (empty text)</p><p><a href=\"/memory\">back</a></p>".into(),
        ));
    }
    match runtime.propose_memory("web", &text, None, who) {
        Ok(_id) => {
            // 303 so a refresh does not re-submit the form.
            let mut resp = Response::redirect("/memory");
            resp.status = 303;
            resp
        }
        Err(e) => Response::html(runtime_page(
            runtime,
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
fn render_candidates(runtime: &Runtime, who: &ka_agent_core::Principal) -> String {
    let rows = runtime.pending_candidates_for(who).unwrap_or_default();
    if rows.is_empty() {
        return "<h2>candidates</h2><p>none awaiting review</p>".into();
    }
    let facts = runtime
        .memory_facts_text(who)
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
                "<div class='card'><p><b>{} (role {})</b> proposes:</p><pre>{diff}</pre>\
                 <form method=post action=/memory/candidate>\
                 <input type=hidden name=id value=\"{}\">\
                 <button name=decision value=y>Accept</button>\
                 <button name=decision value=n>Reject</button></form></div>",
                escape_html(&c.source),
                escape_html(&c.role),
                escape_html(c.id.as_str())
            )
        })
        .collect();
    format!("<h2>candidates</h2>{}", cards.join(""))
}

/// `POST /memory/candidate`: apply a review decision.
pub fn memory_candidate(
    runtime: &Runtime,
    body: &str,
    who: &ka_agent_core::Principal,
) -> crate::Response {
    let id = form_field(body, "id");
    let applied = match form_field(body, "decision").as_str() {
        "y" => runtime.accept_candidate(&id, who),
        _ => runtime.reject_candidate(&id, who),
    };
    match applied {
        // Ok(false): unknown or already-settled candidate — say so instead of
        // silently reporting success.
        Ok(true) => {
            let mut resp = Response::redirect("/memory");
            resp.status = 303;
            resp
        }
        Ok(false) => Response::html(runtime_page(
            runtime,
            "Memory",
            format!(
                "<p>no pending candidate {}: it may already be settled</p>\
                 <p><a href=\"/memory\">back</a></p>",
                escape_html(&id)
            ),
        )),
        Err(e) => Response::html(runtime_page(
            runtime,
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

fn approval_card_html(
    runtime: &Runtime,
    card: &ka_agent_runtime::ApprovalCard,
    escalated: bool,
) -> String {
    let q = escape_html(&card.question);
    let d = card
        .detail
        .as_deref()
        .map(|d| {
            let escaped = escape_html(&d.lines().take(20).collect::<Vec<_>>().join("\n"));
            format!("<pre>{escaped}</pre>")
        })
        .unwrap_or_default();
    let expires = ka_agent_core::fmt_interval(card.expires_at_ms - ka_agent_core::now_ms());
    let _ = runtime;
    format!(
        "<div class='card'><p>{q}</p>{d}<p class='small muted'>{tag} · expires in {expires}</p>\
         <form method=post action=/approvals/decide>\
         <input type=hidden name=id value=\"{id}\">\
         <button class=primary name=decision value=y>Allow</button>\
         <button name=decision value=n>Deny</button>\
         <button name=decision value=session>Allow for this session</button></form></div>",
        tag = if escalated {
            "escalated to the admin"
        } else {
            "yours to decide"
        },
        id = escape_html(&card.approval_id),
    )
}

pub fn approvals(
    runtime: &Runtime,
    _store: &Store,
    who: &ka_agent_core::Principal,
) -> crate::Response {
    let mut cards: Vec<String> = runtime
        .pending_approval_cards(who)
        .unwrap_or_default()
        .iter()
        .map(|c| approval_card_html(runtime, c, false))
        .collect();
    if who.is_admin() {
        for card in runtime.escalated_cards(50).unwrap_or_default() {
            cards.push(approval_card_html(runtime, &card, true));
        }
        let own: Vec<String> = runtime
            .cards_for_user(&who.username)
            .unwrap_or_default()
            .iter()
            .map(|c| approval_card_html(runtime, c, false))
            .collect();
        for c in own {
            if !cards.iter().any(|existing| existing == &c) {
                cards.push(c);
            }
        }
    }
    let body = if cards.is_empty() {
        "<div class=\"empty\">nothing waiting — no engine is blocked on an ask</div>".to_string()
    } else {
        cards.join("")
    };
    nav_page_sub(
        runtime,
        "/approvals",
        "Approvals",
        "Cards the engine is blocked on. Within your own grants you decide your own actions; anything shared, over quota or on your role's escalate list goes to the admin (D9).",
        body,
    )
}

pub async fn approvals_decide(
    runtime: &Runtime,
    body: &str,
    source: &str,
) -> crate::Response {
    let id = form_field(body, "id");
    if form_field(body, "decision") == "session" {
        return match runtime.decide_for_session(&id, source).await {
            Ok(()) => Response::see_other("/approvals"),
            Err(e) => result_page(runtime, "/approvals", "Approvals", &e.to_string(), false),
        };
    }
    let decision = match form_field(body, "decision").as_str() {
        "y" => ApprovalDecision::Allow,
        _ => ApprovalDecision::Deny,
    };
    match runtime.decide(&id, decision, source).await {
        Ok(()) => Response::redirect("/approvals"),
        Err(e) => Response::html(runtime_page(
            runtime,
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
        return nav_page_sub(
            runtime,
            "/audit",
            "Audit",
            "Append-only, hash-chained record of every operator and subject action.",
            "<p>no audit records</p>".into(),
        );
    }
    let rows: Vec<String> = records
        .iter()
        .map(|r| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
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
                escape_html(&r.username),
                escape_html(&r.role),
                escape_html(&r.action),
                escape_html(&r.subject)
            )
        })
        .collect();
    let body = format!(
        "<div class=\"tablewrap\"><table><tr><th>seq</th><th>at</th><th>actor</th><th>user</th><th>role</th><th>action</th><th>subject</th></tr>{}</table></div>",
        rows.join("")
    );
    nav_page_sub(
        runtime,
        "/audit",
        "Audit",
        "Append-only, hash-chained record of every operator and subject action.",
        body,
    )
}

pub fn intents(runtime: &Runtime, store: &Store) -> crate::Response {
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
    nav_page_sub(
        runtime,
        "/intents",
        "Intents",
        "External-write intents: what was attempted, and what may have happened.",
        body,
    )
}

/// `GET /usage`: spend per day, model **and user**, CSS bars, today against
/// the budget. `?format=csv` downloads the same rows.
pub fn usage(runtime: &Runtime, store: &Store, format: Option<&str>) -> crate::Response {
    let cfg = runtime.config();
    let since = jiff::Timestamp::now()
        .checked_sub(jiff::Span::new().days(30))
        .map(|t| t.as_millisecond())
        .unwrap_or(0);
    let rows = store.usage_rows_with_user(since).unwrap_or_default();
    // Bucket by local day, user and model (a turn lands on the day it
    // started).
    let tz = runtime.now_zoned().time_zone().clone();
    let mut buckets: Vec<(String, String, String, u64, u64, f64)> = Vec::new();
    for (at_ms, cost, tokens, model, user) in rows {
        let day = jiff::Timestamp::from_millisecond(at_ms)
            .map(|t| t.to_zoned(tz.clone()).strftime("%Y-%m-%d").to_string())
            .unwrap_or_default();
        let model = model.unwrap_or_else(|| "(not recorded)".into());
        match buckets
            .iter_mut()
            .find(|b| b.0 == day && b.1 == user && b.2 == model)
        {
            Some(b) => {
                b.3 += 1;
                b.4 += tokens;
                b.5 += cost;
            }
            None => buckets.push((day, user, model, 1, tokens, cost)),
        }
    }
    buckets.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
    if format == Some("csv") {
        let mut out = String::from("day,user,model,turns,tokens,cost_usd\n");
        for (day, user, model, turns, tokens, cost) in &buckets {
            out.push_str(&format!(
                "{day},{},\"{}\" ,{turns},{tokens},{cost:.6}\n",
                csv_field(user),
                model.replace('"', "\"\"")
            ));
        }
        let mut r = Response::base(200, "text/csv; charset=utf-8", out);
        r.extra_headers.push((
            "Content-Disposition",
            "attachment; filename=\"ka-agent-usage.csv\"".to_string(),
        ));
        return r;
    }
    let max = buckets
        .iter()
        .map(|b| b.5)
        .fold(0.0_f64, f64::max)
        .max(0.01);
    let cap = cfg.budgets.daily_usd;
    let rows_html: String = buckets
        .iter()
        .rev()
        .map(|(day, user, model, turns, tokens, cost)| {
            let pct = (cost / max * 100.0).clamp(1.0, 100.0);
            let over = cap.is_some_and(|c| *cost > c);
            format!(
                "<tr><td>{day}</td><td><code>{}</code></td><td><code>{}</code></td><td>{turns}</td><td>{tokens}</td>\
                 <td><span class='bar{}' style='width:{}em'></span> <span class='{}'>${:.4}</span></td></tr>",
                escape_html(user),
                escape_html(model),
                if over { " over" } else { "" },
                (pct / 4.0) as u32,
                if over { "bad" } else { "" },
                cost
            )
        })
        .collect();
    let mut per_user: Vec<(String, f64, u64)> = Vec::new();
    for (_, user, _, _, tokens, cost) in &buckets {
        match per_user.iter_mut().find(|u| &u.0 == user) {
            Some(u) => {
                u.1 += cost;
                u.2 += tokens;
            }
            None => per_user.push((user.clone(), *cost, *tokens)),
        }
    }
    per_user.sort_by(|a, b| b.1.total_cmp(&a.1));
    let user_rows: String = per_user
        .iter()
        .map(|(user, cost, tokens)| {
            let quota = store
                .get_user(user)
                .ok()
                .flatten()
                .and_then(|u| u.quota_daily_usd)
                .map(|q| format!("${q:.2}/day"))
                .unwrap_or_else(|| "role/profile budget".into());
            format!(
                "<tr><td><code>{}</code></td><td>{tokens}</td><td>${cost:.4}</td><td>{quota}</td></tr>",
                escape_html(user)
            )
        })
        .collect();
    let today_key = jiff::Timestamp::now()
        .to_zoned(tz.clone())
        .strftime("%Y-%m-%d")
        .to_string();
    let today = buckets
        .iter()
        .filter(|b| b.0 == today_key)
        .map(|b| b.5)
        .sum::<f64>();
    let budget_note = match cap {
        Some(c) => format!("${today:.4} of ${c:.2} today"),
        None => format!("${today:.4} today (no profile cap set)"),
    };
    let table = if buckets.is_empty() {
        "<div class=\"empty\">no usage in the last 30 days</div>".to_string()
    } else {
        format!(
            "<h2>By user</h2><div class=\"tablewrap\"><table><tr><th>user</th><th>tokens</th><th>cost</th><th>quota</th></tr>{user_rows}</table></div>\
             <h2>By day</h2><div class=\"tablewrap\"><table><tr><th>day</th><th>user</th><th>model</th><th>turns</th><th>tokens</th><th>cost</th></tr>{rows_html}</table></div>\
             <p><a href=\"/usage?format=csv\">download CSV</a></p>\
             <style>.bar{{display:inline-block;height:0.8em;background:var(--accent);border-radius:3px;vertical-align:middle}}.bar.over{{background:var(--bad)}}</style>"
        )
    };
    let body = format!("<p class='muted'>{budget_note}</p>{table}");
    nav_page_sub(
        runtime,
        "/usage",
        "Usage",
        "Spend and tokens by user, day and model for the last 30 days (the org pays; quotas cap each user).",
        body,
    )
}

/// One CSV field, quoted when it contains a comma or a quote.
fn csv_field(value: &str) -> String {
    if value.contains(',') || value.contains('"') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// A list field's items: a comma-separated list, optionally wrapped in
/// `[ ]` with per-item quotes.
fn list_values(v: &str) -> Vec<String> {
    let inner = v
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(v);
    inner
        .split(',')
        .map(str::trim)
        .map(|s| s.trim_matches(|c| c == '"' || c == '\'').trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Render one submitted value as TOML, refusing anything the registry's
/// declared kind does not allow. The error names the key; the caller writes
/// nothing at all when any key is refused.
fn render_setting(spec: &ka_agent_core::config_keys::KeySpec, v: &str) -> Result<String, String> {
    use ka_agent_core::config_keys::Kind;
    let refuse = |msg: String| -> Result<String, String> { Err(format!("{}: {msg}", spec.path)) };
    match spec.kind {
        Kind::Bool => match v {
            "true" | "false" => Ok(v.to_string()),
            _ => refuse("must be true or false".into()),
        },
        Kind::Int { min, max } => match v.parse::<i64>() {
            Ok(n) if (min..=max).contains(&n) => Ok(n.to_string()),
            _ => refuse(format!("must be an integer in {min}..={max}")),
        },
        Kind::Float { min } => match v.parse::<f64>() {
            Ok(n) if n.is_finite() && n >= min => Ok(n.to_string()),
            _ => refuse(format!("must be a number >= {min}")),
        },
        Kind::Enum(options) => {
            if options.contains(&v) {
                Ok(format!("{v:?}"))
            } else {
                refuse(format!("must be one of: {}", options.join(", ")))
            }
        }
        // Model selectors go through the same catalog check the CLI uses.
        Kind::Str => match spec.path {
            "model" | "background_model" => ka_agent_port::catalog::validate_selector(v)
                .map(|info| format!("{:?}", info.id))
                .map_err(|e| format!("{}: {e}", spec.path)),
            _ => Ok(format!("{v:?}")),
        },
        Kind::List => {
            let mut items = Vec::new();
            for item in list_values(v) {
                if spec.path == "fallback" {
                    let id = ka_agent_port::catalog::validate_selector(&item)
                        .map_err(|e| format!("{}: {e}", spec.path))?;
                    items.push(format!("{:?}", id.id));
                } else {
                    items.push(format!("{item:?}"));
                }
            }
            Ok(format!("[{}]", items.join(", ")))
        }
        Kind::Table => Ok(v.to_string()),
    }
}

/// The status page with a flash paragraph under its heading (the POST answer:
/// a redirect would drop the message).
fn status_with_flash(runtime: &Runtime, flash: &str) -> crate::Response {
    let mut page = status(runtime, &op());
    if let Some(body_start) = page.body.find("<h1>Status</h1>") {
        let at = body_start + "<h1>Status</h1>".len();
        page.body.insert_str(at, flash);
    }
    page
}

/// `POST /settings`: validate every submitted value against the registry
/// kind, then format-preserving writes and a live apply. Keys that would
/// need a restart are not offered here.
pub async fn settings_apply(runtime: &Runtime, body: &str) -> crate::Response {
    use std::fmt::Write as _;
    let Some(path) = runtime.config_path() else {
        return Response::see_other("/status");
    };
    use ka_agent_core::config_keys::{KEYS, KeySpec};
    let mut pending: Vec<(&'static KeySpec, String)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for spec in KEYS.iter().filter(|k| k.web) {
        let field = spec.path.replace('.', "_");
        let raw = form_field_url(body, &field);
        if raw.trim().is_empty() {
            continue;
        }
        let v = crate::url_decode(&raw.replace('+', " ")).trim().to_string();
        match render_setting(spec, &v) {
            Ok(rendered) => pending.push((spec, rendered)),
            Err(e) => errors.push(e),
        }
    }
    // Validate the whole form before writing anything: one refused key must
    // not leave the rest of the submission half-persisted.
    if !errors.is_empty() {
        let flash = format!(
            "<p class='bad'>refused, nothing was written: {}</p>",
            escape_html(&errors.join("; "))
        );
        return status_with_flash(runtime, &flash);
    }
    let mut applied: Vec<String> = Vec::new();
    for (spec, rendered) in &pending {
        match ka_agent_core::config_file::set(&path, spec.path, rendered) {
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
                    escape_html(&applied.join(", ")),
                    if restart.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " — restart needed for: {}",
                            escape_html(&restart.join(", "))
                        )
                    }
                );
            }
            Err(e) => errors.push(format!("reload: {e}")),
        }
    }
    if !errors.is_empty() {
        let _ = write!(
            flash,
            "<p class='bad'>{}</p>",
            escape_html(&errors.join("; "))
        );
    }
    status_with_flash(runtime, &flash)
}
