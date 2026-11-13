//! Account, group and knowledge-base pages (ADR-0004, M1).
//!
//! Every handler here is admin-only (the router enforces it) and every
//! mutation is a POST validated by the same store APIs the CLI uses, so the
//! panel and `ka-agent user|kb|group` cannot drift.

use crate::{Response, escape_html};
use ka_agent_runtime::Runtime;
use ka_agent_store::{KbDocInfo, Store};

/// A parsed `application/x-www-form-urlencoded` body.
pub struct Form {
    pairs: Vec<(String, String)>,
}

impl Form {
    pub fn parse(body: &[u8]) -> Self {
        let text = String::from_utf8_lossy(body);
        let mut pairs = Vec::new();
        for chunk in text.split('&') {
            if chunk.is_empty() {
                continue;
            }
            let (k, v) = chunk.split_once('=').unwrap_or((chunk, ""));
            pairs.push((crate::url_decode(k), crate::url_decode(v)));
        }
        Self { pairs }
    }

    /// The first value for `key`, or `""`.
    pub fn get(&self, key: &str) -> &str {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }

    /// Every value for `key` (checkbox groups).
    pub fn all(&self, key: &str) -> Vec<String> {
        self.pairs
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// A comma/whitespace separated list value (group names, globs).
    pub fn list(&self, key: &str) -> Vec<String> {
        self.get(key)
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }
}

fn flash(kind: &str, text: &str) -> String {
    format!("<div class=\"flash {kind}\">{}</div>", escape_html(text))
}

fn page(title: &str, subtitle: &str, body: String) -> Response {
    Response::html(format!(
        "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{t}</title><style>\
         body{{font:15px/1.5 system-ui,-apple-system,'Segoe UI',sans-serif;background:#f6f6f4;color:#16181d;margin:0;display:grid;place-items:center;min-height:100vh}}\
         .card{{background:#fff;border:1px solid #e3e4e8;border-radius:14px;padding:1.6rem 1.8rem;width:min(420px,92vw);box-shadow:0 8px 24px rgba(16,18,23,.06)}}\
         h1{{font-size:1.25rem;margin:0 0 .2rem}} p.sub{{color:#6b7280;margin:.1rem 0 1rem;font-size:.9rem}}\
         label{{display:block;margin:.7rem 0}} label span{{display:block;font-size:.8rem;color:#6b7280;margin-bottom:.2rem}}\
         input{{width:100%;font:inherit;padding:.5rem .6rem;border:1px solid #cfd1d8;border-radius:8px;box-sizing:border-box}}\
         button{{font:inherit;width:100%;margin-top:1rem;padding:.55rem;border:0;border-radius:8px;background:#4f46e5;color:#fff;cursor:pointer}}\
         .flash{{border-left:3px solid #b91c1c;background:#fef2f2;padding:.5rem .7rem;border-radius:8px;margin:.4rem 0;font-size:.9rem}}\
         a{{color:#4f46e5}} small{{color:#6b7280}}</style></head><body><div class=card>\
         <h1>{t}</h1><p class=sub>{s}</p>{body}</div></body></html>",
        t = escape_html(title),
        s = escape_html(subtitle),
    ))
}

/// `GET /login`.
pub fn login_page(error: Option<&str>) -> Response {
    let flash = error.map(|e| flash("bad", e)).unwrap_or_default();
    page(
        "Sign in",
        "Your account on this ka-agent. No account yet? An admin sends an invite code.",
        format!(
            "{flash}<form method=post action=/login>\
             <label><span>username</span><input name=username autocomplete=username autofocus required></label>\
             <label><span>password</span><input name=password type=password autocomplete=current-password required></label>\
             <button>Sign in</button></form>\
             <p><small>Have an invite code? <a href=\"/invite\">Create your account</a>.</small></p>\
             <script src=\"/static/ui.js\"></script>"
        ),
    )
}

/// `GET /invite`.
pub fn invite_page(error: Option<&str>) -> Response {
    let flash = error.map(|e| flash("bad", e)).unwrap_or_default();
    page(
        "Create your account",
        "Paste the invite code an admin gave you, choose a username and a password.",
        format!(
            "{flash}<form method=post action=/invite>\
             <label><span>invite code</span><input name=code autocomplete=off required></label>\
             <label><span>username</span><input name=username autocomplete=username required></label>\
             <label><span>password (8+ characters)</span><input name=password type=password autocomplete=new-password required></label>\
             <button>Create account</button></form>\
             <p><small>Already have an account? <a href=\"/login\">Sign in</a>.</small></p>\
             <script src=\"/static/ui.js\"></script>"
        ),
    )
}

fn admin_page(title: &str, subtitle: &str, active: &str, body: String) -> Response {
    // Reuse the panel's chrome so the account pages look like the rest.
    crate::pages::admin_shell(active, title, subtitle, body)
}

/// `GET /users`.
pub fn users(_runtime: &Runtime, store: &Store, msg: Option<&str>) -> Response {
    let users = store.list_users().unwrap_or_default();
    let groups = store.list_groups().unwrap_or_default();
    let group_names: Vec<String> = groups.iter().map(|g| g.name.clone()).collect();
    let mut rows = String::new();
    for u in &users {
        let mut memberships = String::new();
        for g in &group_names {
            let checked = if store
                .groups_of(&u.username)
                .unwrap_or_default()
                .contains(g)
            {
                " checked"
            } else {
                ""
            };
            memberships.push_str(&format!(
                "<label class=small><input type=checkbox name=groups value=\"{}\"{checked}> {}</label> ",
                escape_html(g),
                escape_html(g)
            ));
        }
        let quota = u
            .quota_daily_usd
            .map(|q| format!("${q:.2}"))
            .unwrap_or_else(|| "role/profile".into());
        let state = if u.disabled {
            "<span class=\"bad\">disabled</span>"
        } else {
            "<span class=\"ok\">active</span>"
        };
        let identity_count = store
            .identities_for_user(&u.username)
            .map(|v| v.len())
            .unwrap_or(0);
        rows.push_str(&format!(
            "<tr><td><code>{name}</code><div class=\"small muted\">{display}</div></td>\
             <td>{role}</td><td>{state}</td><td>{identities}</td><td class=nowrap>{quota}</td>\
             <td><form method=post action=/users/save style=display:inline>\
             <input type=hidden name=username value=\"{name}\">\
             <input name=role value=\"{role}\" size=10>\
             <input name=quota value=\"{quota_raw}\" size=6 placeholder=\"usd\">\
             <div>{memberships}</div>\
             <button class=sm>save</button></form></td>\
             <td class=actions nowrap>\
             {toggle}\
             <form method=post action=/users/reset style=display:inline><input type=hidden name=username value=\"{name}\"><button class=sm>reset password</button></form>\
             <form method=post action=/users/export style=display:inline><input type=hidden name=username value=\"{name}\"><button class=sm>export</button></form>\
             <form method=post action=/users/purge style=display:inline><input type=hidden name=username value=\"{name}\"><input name=confirm placeholder=\"type name\" size=8><button class=\"sm danger\">purge</button></form>\
             </td></tr>",
            name = escape_html(&u.username),
            display = escape_html(&u.display_name),
            role = escape_html(&u.role),
            state = state,
            identities = identity_count,
            quota = escape_html(&quota),
            quota_raw = u.quota_daily_usd.map(|q| format!("{q}")).unwrap_or_default(),
            memberships = if memberships.is_empty() {
                "<span class=small muted>no groups</span>".to_string()
            } else {
                memberships
            },
            toggle = if u.disabled {
                format!("<form method=post action=/users/enable style=display:inline><input type=hidden name=username value=\"{}\"><button class=sm>enable</button></form>", escape_html(&u.username))
            } else {
                format!("<form method=post action=/users/disable style=display:inline><input type=hidden name=username value=\"{}\"><button class=sm>disable</button></form>", escape_html(&u.username))
            },
        ));
    }
    if rows.is_empty() {
        rows.push_str("<tr><td colspan=7 class=muted>no accounts yet</td></tr>");
    }
    let _group_options: String = group_names
        .iter()
        .map(|g| format!("<label class=small><input type=checkbox name=group value=\"{}\"> {}</label> ", escape_html(g), escape_html(g)))
        .collect();
    let flash_line = msg.map(|m| flash("ok", m)).unwrap_or_default();
    let invites = store.list_invites().unwrap_or_default();
    let mut invite_rows = String::new();
    for i in &invites {
        let used = if i.used_ms.is_some() { "used" } else { "open" };
        let when = jiff::Timestamp::from_millisecond(i.expires_ms)
            .map(|t| t.strftime("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        invite_rows.push_str(&format!(
            "<tr><td>{used}</td><td>{role}</td><td>{groups}</td><td>{when}</td></tr>",
            used = used,
            role = escape_html(&i.role),
            groups = escape_html(&i.groups.join(", ")),
            when = escape_html(&when),
        ));
    }
    if invite_rows.is_empty() {
        invite_rows.push_str("<tr><td colspan=4 class=muted>no invites</td></tr>");
    }
    admin_page(
        "Users",
        "Accounts, roles, groups and quotas. Disabling pauses everything; purge deletes the data trees (export first).",
        "/users",
        format!(
            "{flash_line}\
             <div class=card><h3>Mint an invite</h3>\
             <form method=post action=/users/invite class=formrow>\
             <label class=field><span>role</span><input name=role value=member size=10></label>\
             <label class=field><span>groups</span><input name=groups placeholder=\"dev, ops\" size=18></label>\
             <button class=primary>create code</button></form>\
             <p class=small muted>The code is shown once; the person redeems it at <code>/invite</code> choosing their username and password (24 h).</p></div>\
             <div class=tablewrap><table><thead><tr><th>user</th><th>role</th><th>state</th><th>identities</th><th>quota</th><th>edit</th><th>actions</th></tr></thead><tbody>{rows}</tbody></table></div>\
             <h2>Invites</h2><div class=tablewrap><table><thead><tr><th>state</th><th>role</th><th>groups</th><th>expires</th></tr></thead><tbody>{invite_rows}</tbody></table></div>"
        ),
    )
}

/// `POST /users/invite`: mint a code and show it once.
pub fn users_invite(store: &Store, body: &[u8]) -> Response {
    let form = Form::parse(body);
    let role = form.get("role").trim();
    let role = if role.is_empty() { "member" } else { role };
    let groups = form.list("groups");
    let code = hex(16);
    let invite = ka_agent_store::InviteRow {
        code_hash: crate::sha256_hex(&code),
        role: role.to_string(),
        username: None,
        groups: groups.clone(),
        created_ms: ka_agent_core::now_ms(),
        expires_ms: ka_agent_core::now_ms() + 24 * 60 * 60 * 1000,
        used_ms: None,
        created_by: ka_agent_core::OPERATOR_USER.to_string(),
    };
    if let Err(e) = store.insert_invite(&invite) {
        return admin_page(
            "Users",
            "",
            "/users",
            flash("bad", &format!("invite not stored: {e}")),
        );
    }
    let mut rec = ka_agent_core::AuditRecord::new(
        "web",
        "user.invite",
        role,
        Some(format!("groups {}", groups.join(","))),
    )
    .with_user(ka_agent_core::OPERATOR_USER);
    let _ = store.append_audit(&mut rec);
    admin_page(
        "Invite code",
        "Show this once. The person opens /invite, pastes it, and chooses a username and password.",
        "/users",
        format!(
            "<div class=card><p>Role <b>{}</b> · groups <b>{}</b> · valid 24 h</p>\
             <pre style=\"font-size:1.1rem\">{}</pre>\
             <p class=small muted>Link a channel identity afterwards: <code>ka-agent identity link telegram &lt;id&gt;</code></p>\
             <p><a href=\"/users\">back to users</a></p></div>",
            escape_html(role),
            escape_html(&if groups.is_empty() { "none".to_string() } else { groups.join(", ") }),
            escape_html(&code),
        ),
    )
}

/// `POST /users/save`: role, quota and group membership in one form.
pub fn users_save(runtime: &Runtime, store: &Store, body: &[u8]) -> Response {
    let form = Form::parse(body);
    let username = form.get("username").trim();
    if store.get_user(username).ok().flatten().is_none() {
        return admin_page("Users", "", "/users", flash("bad", "no such user"));
    }
    let role = form.get("role").trim();
    if !role.is_empty() {
        let _ = store.set_user_role(username, role);
    }
    let quota = form.get("quota").trim();
    let quota = if quota.is_empty() {
        None
    } else {
        quota.parse::<f64>().ok().filter(|q| *q > 0.0)
    };
    let _ = store.set_user_quota(username, quota);
    let groups = form.all("groups");
    for g in &groups {
        let _ = store.ensure_group(g);
    }
    let _ = store.set_user_groups(username, &groups);
    let mut rec = ka_agent_core::AuditRecord::new("web", "user.edit", username, None)
        .with_user(ka_agent_core::OPERATOR_USER);
    let _ = store.append_audit(&mut rec);
    users(runtime, store, Some("saved"))
}

/// `POST /users/disable|enable` (offboarding, D10).
pub fn users_state(store: &Store, body: &[u8], disable: bool) -> Response {
    let form = Form::parse(body);
    let username = form.get("username").trim();
    if username == ka_agent_core::OPERATOR_USER {
        return admin_page(
            "Users",
            "",
            "/users",
            flash("bad", "refusing to disable the operator's own account"),
        );
    }
    let Ok(Some(user)) = store.get_user(username) else {
        return admin_page("Users", "", "/users", flash("bad", "no such user"));
    };
    if disable {
        let _ = store.set_user_disabled(username, true);
        let _ = store.delete_web_sessions_for(username);
        // Their crons pause (the admin decides what to reassign).
        if let Ok(tasks) = store.cron_tasks_for_user(username) {
            for task in tasks {
                let _ = store.set_task_status(&task.id, "interrupted");
            }
        }
    } else {
        let _ = store.set_user_disabled(username, false);
    }
    let action = if disable { "user.disable" } else { "user.enable" };
    let mut rec = ka_agent_core::AuditRecord::new("web", action, username, None)
        .with_role(&user.role)
        .with_user(ka_agent_core::OPERATOR_USER);
    let _ = store.append_audit(&mut rec);
    admin_page(
        "Users",
        "",
        "/users",
        flash(
            "ok",
            &format!(
                "{username} {}",
                if disable {
                    "disabled: sessions cleared, identities silent, crons paused"
                } else {
                    "enabled"
                }
            ),
        ),
    )
}

/// `POST /users/reset`: clear the password and mint a new invite.
pub fn users_reset(store: &Store, body: &[u8]) -> Response {
    let form = Form::parse(body);
    let username = form.get("username").trim();
    let Ok(Some(user)) = store.get_user(username) else {
        return admin_page("Users", "", "/users", flash("bad", "no such user"));
    };
    // The old password and every open browser session go now; the account
    // is reopened by a reset code that only works for this username.
    let _ = store.set_user_password(username, None);
    let _ = store.delete_web_sessions_for(username);
    let code = hex(16);
    let invite = ka_agent_store::InviteRow {
        code_hash: crate::sha256_hex(&code),
        role: user.role.clone(),
        username: Some(username.to_string()),
        groups: Vec::new(),
        created_ms: ka_agent_core::now_ms(),
        expires_ms: ka_agent_core::now_ms() + 24 * 60 * 60 * 1000,
        used_ms: None,
        created_by: ka_agent_core::OPERATOR_USER.to_string(),
    };
    let _ = store.insert_invite(&invite);
    let mut rec = ka_agent_core::AuditRecord::new("web", "user.reset", username, None)
        .with_role(&user.role)
        .with_user(ka_agent_core::OPERATOR_USER);
    let _ = store.append_audit(&mut rec);
    admin_page(
        "Password reset",
        "The old password and sessions are gone. Give this code to the person: it only sets a new password for this account.",
        "/users",
        format!(
            "<div class=card><p>reset code for <b>{name}</b> · valid 24 h · single use</p>\
             <pre style=\"font-size:1.1rem\">{code}</pre>\
             <p class=small muted>They open <code>/invite</code>, paste it, and choose a new password.</p>\
             <p><a href=\"/users\">back</a></p></div>",
            name = escape_html(username),
            code = escape_html(&code),
        ),
    )
}

/// `POST /users/export`.
pub fn users_export(runtime: &Runtime, body: &[u8]) -> Response {
    let form = Form::parse(body);
    let username = form.get("username").trim();
    match ka_agent_runtime::user_ops::export_user(&runtime.paths(), username) {
        Ok(path) => admin_page(
            "Export",
            "",
            "/users",
            flash("ok", &format!("exported to {}", path.display())),
        ),
        Err(e) => admin_page("Export", "", "/users", flash("bad", &e)),
    }
}

/// `POST /users/purge`: delete the user's data trees (account stays).
pub fn users_purge(runtime: &Runtime, body: &[u8]) -> Response {
    let form = Form::parse(body);
    let username = form.get("username").trim();
    if form.get("confirm").trim() != username {
        return admin_page(
            "Purge",
            "",
            "/users",
            flash("bad", "type the username to confirm"),
        );
    }
    let warning = match ka_agent_runtime::user_ops::latest_export(&runtime.paths(), username) {
        Some(p) => format!(" (last export: {})", p.display()),
        None => " (no export on record — this is irreversible)".to_string(),
    };
    match ka_agent_runtime::user_ops::purge_user(&runtime.paths(), username) {
        Ok(removed) => admin_page(
            "Purge",
            "",
            "/users",
            flash(
                "ok",
                &format!("purged {}: {}{}", username, removed.join(", "), warning),
            ),
        ),
        Err(e) => admin_page("Purge", "", "/users", flash("bad", &e)),
    }
}

/// `GET /groups`.
pub fn groups(store: &Store) -> Response {
    let groups = store.list_groups().unwrap_or_default();
    let mut rows = String::new();
    for g in &groups {
        let members = store.group_members(&g.name).unwrap_or_default();
        let kbs: Vec<String> = store
            .list_kbs()
            .unwrap_or_default()
            .into_iter()
            .filter(|k| store.kb_access(&k.name).unwrap_or_default().contains(&g.name))
            .map(|k| k.name)
            .collect();
        rows.push_str(&format!(
            "<tr><td><code>{name}</code></td>\
             <td>{members}</td>\
             <td><form method=post action=/groups/save class=formrow>\
             <input type=hidden name=name value=\"{name}\">\
             <input name=models_allow value=\"{allow}\" placeholder=\"anthropic/*, openai/gpt*\" size=26>\
             <input name=models_default value=\"{default}\" placeholder=\"default model\" size=18>\
             <input name=members value=\"{member_list}\" placeholder=\"members\" size=16>\
             <button class=sm>save</button></form></td>\
             <td>{kbs}</td>\
             <td><form method=post action=/groups/delete><input type=hidden name=name value=\"{name}\"><button class=\"sm danger\">delete</button></form></td></tr>",
            name = escape_html(&g.name),
            members = escape_html(&members.join(", ")),
            member_list = escape_html(&members.join(",")),
            allow = escape_html(&g.models_allow.join(", ")),
            default = escape_html(g.models_default.as_deref().unwrap_or("")),
            kbs = escape_html(&kbs.join(", ")),
        ));
    }
    if rows.is_empty() {
        rows.push_str("<tr><td colspan=5 class=muted>no groups yet</td></tr>");
    }
    admin_page(
        "Groups",
        "Membership, model allowlists and default models. A group with no allowlist grants every catalog model.",
        "/groups",
        format!(
            "<div class=card><h3>New group</h3>\
             <form method=post action=/groups/save class=formrow>\
             <label class=field><span>name</span><input name=name placeholder=dev size=12></label>\
             <label class=field><span>models_allow</span><input name=models_allow placeholder=\"anthropic/*\" size=26></label>\
             <label class=field><span>models_default</span><input name=models_default placeholder=\"anthropic/claude\" size=18></label>\
             <button class=primary>create</button></form></div>\
             <div class=tablewrap><table><thead><tr><th>group</th><th>members</th><th>models</th><th>knowledge</th><th></th></tr></thead><tbody>{rows}</tbody></table></div>"
        ),
    )
}

/// `POST /groups/save`: upsert the policy and, when given, the members.
pub fn groups_save(store: &Store, body: &[u8]) -> Response {
    let form = Form::parse(body);
    let name = form.get("name").trim().to_ascii_lowercase();
    if !ka_agent_store::valid_username(&name) {
        return admin_page(
            "Groups",
            "",
            "/groups",
            flash("bad", "group name: lowercase letters, digits, dot, dash or underscore"),
        );
    }
    let allow = form.list("models_allow");
    let default = form.get("models_default").trim();
    let _ = store.upsert_group(&name, &allow, (!default.is_empty()).then_some(default));
    if !form.get("members").trim().is_empty() || form.get("members").is_empty() {
        // Add listed members (never remove implicitly).
        for member in form.list("members") {
            if store.get_user(&member).ok().flatten().is_some() {
                let _ = store.add_user_group(&member, &name);
            }
        }
    }
    let mut rec = ka_agent_core::AuditRecord::new("web", "group.edit", &name, None)
        .with_user(ka_agent_core::OPERATOR_USER);
    let _ = store.append_audit(&mut rec);
    admin_page("Groups", "", "/groups", flash("ok", "group saved"))
}

/// `POST /groups/delete`.
pub fn groups_delete(store: &Store, body: &[u8]) -> Response {
    let form = Form::parse(body);
    let name = form.get("name").trim();
    let _ = store.delete_group(name);
    admin_page("Groups", "", "/groups", flash("ok", "group deleted"))
}

/// `GET /kb`: knowledge bases with access, documents and the upload form.
pub fn kb_page(runtime: &Runtime, store: &Store) -> Response {
    let _ = runtime;
    let kbs = store.list_kbs().unwrap_or_default();
    let mut cards = String::new();
    for kb in &kbs {
        let docs: Vec<KbDocInfo> = store.kb_list_docs(&kb.name).unwrap_or_default();
        let access = store.kb_access(&kb.name).unwrap_or_default();
        let mut doc_rows = String::new();
        for d in &docs {
            doc_rows.push_str(&format!(
                "<tr><td><code>{path}</code></td><td>{title}</td><td>{bytes}B</td><td>{source}</td>\
                 <td><form method=post action=/kb/rm><input type=hidden name=kb value=\"{kb}\"><input type=hidden name=path value=\"{path}\"><button class=\"sm danger\">remove</button></form></td></tr>",
                path = escape_html(&d.path),
                title = escape_html(&d.title),
                bytes = d.bytes,
                source = escape_html(&d.source),
                kb = escape_html(&kb.name),
            ));
        }
        if doc_rows.is_empty() {
            doc_rows.push_str("<tr><td colspan=5 class=muted>no documents</td></tr>");
        }
        cards.push_str(&format!(
            "<div class=card><div class=cardhead><h3>{display} <code>{name}</code></h3><span class=small muted>owner {owner}</span></div>\
             <form method=post action=/kb/access class=formrow><input type=hidden name=kb value=\"{name}\">\
             <label class=field><span>access groups</span><input name=groups value=\"{access}\" size=28></label>\
             <button class=sm>save access</button></form>\
             <form method=post action=/kb/{name}/docs>\
             <div class=formrow>\
             <label class=field><span>document title</span><input name=title placeholder=\"Q3 plan\" size=20></label>\
             <label class=field><span>path</span><input name=path placeholder=\"plan/q3.md\" size=22></label>\
             </div>\
             <label class=field><span>content (.md .txt .csv .json, ≤10 MiB)</span><textarea name=content rows=6 placeholder=\"paste the document text\"></textarea></label>\
             <button class=sm primary>upload</button></form>\
             <div class=tablewrap><table><thead><tr><th>path</th><th>title</th><th>size</th><th>source</th><th></th></tr></thead><tbody>{doc_rows}</tbody></table></div>\
             <form method=post action=/kb/rm><input type=hidden name=kb value=\"{name}\"><button class=\"sm danger\">delete knowledge base</button></form></div>",
            display = escape_html(&kb.display_name),
            name = escape_html(&kb.name),
            owner = escape_html(&kb.owner_username),
            access = escape_html(&access.join(", ")),
        ));
    }
    if cards.is_empty() {
        cards.push_str("<div class=empty>no knowledge bases yet</div>");
    }
    admin_page(
        "Knowledge",
        "Documents grouped per knowledge base, readable by the listed groups (no per-document ACLs — the KB is the unit of sharing).",
        "/kb",
        format!(
            "<div class=card><h3>New knowledge base</h3>\
             <form method=post action=/kb/create class=formrow>\
             <label class=field><span>name</span><input name=name placeholder=handbook size=14></label>\
             <label class=field><span>display name</span><input name=display_name placeholder=\"Team handbook\" size=22></label>\
             <label class=field><span>groups</span><input name=groups placeholder=\"dev, ops\" size=20></label>\
             <button class=primary>create</button></form></div>{cards}"
        ),
    )
}

/// `POST /kb/create`.
pub fn kb_create(store: &Store, body: &[u8], who: &ka_agent_core::Principal) -> Response {
    let form = Form::parse(body);
    let name = form.get("name").trim().to_ascii_lowercase();
    if !ka_agent_store::valid_username(&name) {
        return admin_page("Knowledge", "", "/kb", flash("bad", "kb name: lowercase word"));
    }
    let display = form.get("display_name").trim();
    let display = if display.is_empty() { &name } else { display };
    match store.create_kb(&name, display, &who.username) {
        Ok(true) => {
            let groups = form.list("groups");
            let _ = store.set_kb_access(&name, &groups);
            admin_page("Knowledge", "", "/kb", flash("ok", "knowledge base created"))
        }
        Ok(false) => admin_page("Knowledge", "", "/kb", flash("bad", "that KB already exists")),
        Err(e) => admin_page("Knowledge", "", "/kb", flash("bad", &e.to_string())),
    }
}

/// `POST /kb/access`.
pub fn kb_access(store: &Store, body: &[u8]) -> Response {
    let form = Form::parse(body);
    let kb = form.get("kb").trim();
    let groups = form.list("groups");
    let _ = store.set_kb_access(kb, &groups);
    admin_page("Knowledge", "", "/kb", flash("ok", "access saved"))
}

/// `POST /kb/rm` (with a path) or `POST /kb/rm` (kb only).
pub fn kb_rm(store: &Store, body: &[u8]) -> Response {
    let form = Form::parse(body);
    let kb = form.get("kb").trim();
    let path = form.get("path").trim();
    if path.is_empty() {
        let _ = store.delete_kb(kb);
        admin_page("Knowledge", "", "/kb", flash("ok", "knowledge base deleted"))
    } else {
        let _ = store.kb_remove_doc(kb, path);
        admin_page("Knowledge", "", "/kb", flash("ok", "document removed"))
    }
}

/// `POST /kb/<name>/docs`: one document. Two shapes: a form (`path`,
/// `title`, `content` — the panel's textarea) or a raw body with the
/// `X-Ka-Filename` header (the CLI/JS upload path).
pub fn kb_upload(
    store: &Store,
    kb: &str,
    body: &[u8],
    raw_name: Option<&str>,
    raw_title: Option<&str>,
) -> Response {
    let form = Form::parse(body);
    let (content, path, title) = match raw_name {
        Some(name) if !name.trim().is_empty() => {
            let Ok(text) = std::str::from_utf8(body) else {
                return admin_page(
                    "Knowledge",
                    "",
                    "/kb",
                    flash("bad", "the document is not UTF-8 text (PDF/binary is not supported yet)"),
                );
            };
            (text, name.trim(), raw_title.unwrap_or(name).trim())
        }
        _ => (form.get("content"), form.get("path").trim(), form.get("title").trim()),
    };
    if content.trim().is_empty() {
        return admin_page(
            "Knowledge",
            "",
            "/kb",
            flash("bad", "no content — paste the document text in `content`"),
        );
    }
    if path.is_empty() {
        return admin_page("Knowledge", "", "/kb", flash("bad", "a document needs a path"));
    }
    if content.len() > 10 * 1024 * 1024 {
        return admin_page("Knowledge", "", "/kb", flash("bad", "document exceeds 10 MiB"));
    }
    let digest = crate::sha256_hex(content);
    let title = if title.is_empty() { path } else { title };
    match store.kb_upsert_doc(
        kb,
        path,
        title,
        content,
        &digest,
        "upload",
        ka_agent_core::now_ms(),
    ) {
        Ok(_) => admin_page("Knowledge", "", "/kb", flash("ok", "document indexed")),
        Err(e) => admin_page("Knowledge", "", "/kb", flash("bad", &e.to_string())),
    }
}

/// `GET /agents`: the shared agents this user can see, with their level.
pub fn agents(store: &Store, who: &ka_agent_core::Principal) -> Response {
    let views = store.agents_for_user(&who.username, &who.role).unwrap_or_default();
    let mut cards = String::new();
    for view in &views {
        let a = &view.agent;
        let grants: Vec<String> = store
            .agent_access(&a.name)
            .unwrap_or_default()
            .into_iter()
            .map(|g| format!("{}={}", g.group_name, g.level))
            .collect();
        let can_edit = matches!(view.level.as_str(), "editor" | "owner");
        let manage = if view.level == "owner" {
            format!(
                "<form method=post action=/agents/rm style=display:inline>\
                 <input type=hidden name=name value=\"{name}\"><button class=\"sm danger\">delete</button></form>",
                name = escape_html(&a.name)
            )
        } else {
            String::new()
        };
        cards.push_str(&format!(
            "<div class=card><div class=cardhead><h3>{name}</h3><span class=\"chip\"><b>{level}</b></span></div>\
             <p class=small muted>owner {owner} · model {model} · skills [{skills}] · knowledge [{kbs}]</p>\
             <pre>{persona}</pre>\
             <p class=small>groups: {grants}</p>\
             {edit}\
             <p class=small><code>/agent {name}</code> in a chat runs your conversation as this agent.</p>\
             {manage}</div>",
            name = escape_html(&a.name),
            level = escape_html(&view.level),
            owner = escape_html(&a.owner_username),
            model = escape_html(a.model.as_deref().unwrap_or("user default")),
            skills = escape_html(&a.skills.join(",")),
            kbs = escape_html(&a.kbs.join(",")),
            persona = escape_html(&a.persona),
            grants = escape_html(&grants.join(", ")),
            edit = if can_edit {
                format!(
                    "<form method=post action=/agents/save>\
                     <input type=hidden name=name value=\"{name}\">\
                     <label class=field><span>persona</span><textarea name=persona rows=4>{persona}</textarea></label>\
                     <div class=formrow>\
                     <label class=field><span>model</span><input name=model value=\"{model_raw}\" size=24></label>\
                     <label class=field><span>skills</span><input name=skills value=\"{skills}\" size=20></label>\
                     <label class=field><span>knowledge bases</span><input name=kbs value=\"{kbs}\" size=20></label>\
                     <button class=sm>save</button></div></form>",
                    name = escape_html(&a.name),
                    persona = escape_html(&a.persona),
                    model_raw = escape_html(a.model.as_deref().unwrap_or("")),
                    skills = escape_html(&a.skills.join(",")),
                    kbs = escape_html(&a.kbs.join(",")),
                )
            } else {
                String::new()
            },
        ));
    }
    if cards.is_empty() {
        cards.push_str("<div class=empty>no shared agents you can see yet</div>");
    }
    admin_page(
        "Shared agents",
        "A definition several people can chat with: persona, skills, knowledge and model. Run state stays per person.",
        "/agents",
        format!(
            "<div class=card><h3>New agent</h3>\
             <form method=post action=/agents/create>\
             <div class=formrow>\
             <label class=field><span>name</span><input name=name placeholder=reviewer size=14></label>\
             <label class=field><span>model</span><input name=model placeholder=\"anthropic/claude\" size=22></label>\
             <label class=field><span>skills</span><input name=skills placeholder=\"review\" size=18></label>\
             <label class=field><span>knowledge bases</span><input name=kbs placeholder=handbook size=18></label>\
             </div>\
             <label class=field><span>persona</span><textarea name=persona rows=4 placeholder=\"You review changes and cite the handbook.\"></textarea></label>\
             <label class=field><span>share with group (viewer | editor | owner)</span><input name=group placeholder=dev size=12></label>\
             <button class=primary>create</button></form></div>{cards}"
        ),
    )
}

/// `POST /agents/create`.
pub fn agents_create(store: &Store, body: &[u8], who: &ka_agent_core::Principal) -> Response {
    let form = Form::parse(body);
    let name = form.get("name").trim().to_ascii_lowercase();
    if !ka_agent_store::valid_username(&name) {
        return admin_page("Shared agents", "", "/agents", flash("bad", "agent name: lowercase word"));
    }
    let agent = ka_agent_store::AgentRow {
        name: name.clone(),
        persona: {
            let p = form.get("persona").trim();
            if p.is_empty() {
                "You are a shared team agent. Answer carefully and cite the knowledge you use."
                    .to_string()
            } else {
                p.to_string()
            }
        },
        model: (!form.get("model").trim().is_empty()).then(|| form.get("model").trim().to_string()),
        skills: form.list("skills"),
        kbs: form.list("kbs"),
        owner_username: who.username.clone(),
        created_ms: ka_agent_core::now_ms(),
    };
    if let Err(e) = store.create_agent(&agent) {
        return admin_page("Shared agents", "", "/agents", flash("bad", &e.to_string()));
    }
    let group = form.get("group").trim();
    if !group.is_empty() {
        let _ = store.ensure_group(group);
        let _ = store.set_agent_access(
            &name,
            &[ka_agent_store::AgentAccess {
                group_name: group.to_string(),
                level: "viewer".into(),
            }],
        );
    }
    admin_page("Shared agents", "", "/agents", flash("ok", "agent created"))
}

/// `POST /agents/save` (editor or owner).
pub fn agents_save(store: &Store, body: &[u8], who: &ka_agent_core::Principal) -> Response {
    let form = Form::parse(body);
    let name = form.get("name").trim();
    let Some(level) = store.agent_level(name, &who.username, &who.role).ok().flatten() else {
        return admin_page("Shared agents", "", "/agents", flash("bad", "no access to that agent"));
    };
    if level == "viewer" {
        return admin_page("Shared agents", "", "/agents", flash("bad", "viewers cannot edit an agent"));
    }
    let Some(mut agent) = store.get_agent(name).ok().flatten() else {
        return admin_page("Shared agents", "", "/agents", flash("bad", "no such agent"));
    };
    agent.persona = form.get("persona").trim().to_string();
    agent.model = (!form.get("model").trim().is_empty()).then(|| form.get("model").trim().to_string());
    agent.skills = form.list("skills");
    agent.kbs = form.list("kbs");
    match store.update_agent(&agent) {
        Ok(()) => admin_page("Shared agents", "", "/agents", flash("ok", "agent saved")),
        Err(e) => admin_page("Shared agents", "", "/agents", flash("bad", &e.to_string())),
    }
}

/// `POST /agents/rm` (owner only; the owner account is the admin or the
/// creator).
pub fn agents_rm(store: &Store, body: &[u8], who: &ka_agent_core::Principal) -> Response {
    let form = Form::parse(body);
    let name = form.get("name").trim();
    let level = store.agent_level(name, &who.username, &who.role).ok().flatten();
    if level.as_deref() != Some("owner") {
        return admin_page("Shared agents", "", "/agents", flash("bad", "only an owner can delete an agent"));
    }
    let _ = store.delete_agent(name);
    admin_page("Shared agents", "", "/agents", flash("ok", "agent deleted"))
}

/// Random lowercase hex (invite codes).
fn hex(bytes: usize) -> String {
    use rand_core::RngCore;
    let mut buf = vec![0u8; bytes];
    rand_core::OsRng.fill_bytes(&mut buf);
    let mut out = String::with_capacity(bytes * 2);
    for b in buf {
        out.push_str(&format!("{b:02x}"));
    }
    out
}
