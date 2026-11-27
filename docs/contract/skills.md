# Skills contract — SEED.md §9.1 (minimal mechanism)

Status: implemented; B12 frontmatter/doctor added 2026-11-19, verified by the
unit suites and the `skills_doctor_e2e` chat e2e (`skills_enable_list_invoke`
covers the original mechanism). A skill is **text, not code** (SEED.md §2.6): it changes what
the model knows how to do, never what it is allowed to do.

## Shape and discovery

- One directory per skill under a skill library. Precedence: the active
  profile's own library `<data root>/skills/<name>/SKILL.md` first, the
  global `~/.ka-agent/skills/<name>/SKILL.md` as the fallback
  (`$KA_AGENT_HOME`-aware; a pre-0.3 `$XDG_CONFIG_HOME/ka-agent/skills`
  still wins while it exists). A name present in both keeps the profile's
  copy.
- `SKILL.md` is the ecosystem format ka already reads: TOML frontmatter
  (`name`, `description`) + Markdown body. A sample lives at
  `examples/skills/review/` in this repository.
- Names: `[a-z0-9-]{1,48}`. Anything else is ignored by scans and refused
  by enable (a `../escape` cannot be provisioned).
- Discovery is ka's own user scope: `ka-agent skills enable` **copies** the
  directory into `<data>/engine-home/.config/ka/skills/<name>/`, which the
  worker's synthetic `HOME` exposes to every engine. Progressive disclosure
  is ka's (name + description in the prompt; body read on demand).
- Bounds: ≤ 256 KiB and ≤ 64 files per skill directory; enable refuses
  beyond them. Re-enable replaces the provisioning (staged copy + swap);
  the operator's source copy is never modified or deleted.

## CLI

- `ka-agent skills list` — both libraries with `[enabled]` (admin) and
  `[enabled@<role>]` markers, plus `[enabled, no source copy]` for
  provisioned-without-source entries (e.g. after a restore).
- `ka-agent skills enable <name> [--role <r>]` — provision into the admin's
  engine home (default) or the role's. `disable <name> [--role <r>]` —
  remove the provisioning only; the source copy is never touched.
- Each role's engine home is `<data>/roles/<r>/engine-home`, so a skill
  enabled for one role is invisible to the others and to admin.

## Surfaces (shared router)

- `/skills` — one line per enabled skill: `name — description` (name alone
  when the frontmatter has no description), or a hint when none are
  enabled.
- `/skill <name> <text>` — submit **one turn** with that skill attached.
  The runtime sends the name in the worker protocol's per-turn `skills`
  field (`Command::Prompt.skills`); the engine injects the full SKILL.md
  body for this turn only. The reply streams back like any turn.
  - The TUI's `/skill:<name>` prompt interception does not exist headless;
    this command is the headless equivalent, driven by the router so every
    channel has it.
  - Fail-closed rules: the skill must be enabled (otherwise
    `skill "…" is not enabled — ka-agent skills enable …`), the session
    must be idle (a skill turn is never queued — the queue carries plain
    text only), and empty text is a usage error. Nothing is submitted on
    refusal.

## Frontmatter: triggers, paths, allowed_tools

Beyond `name`/`description`, the operator's source `SKILL.md` may carry three
ka-agent keys (TOML arrays):

- `triggers = ["deploy", "release"]` — up to 8, each `[a-z0-9-]{1,32}`. A
  plain-text message fires the skill when any trigger appears as a
  case-insensitive whole word (`deploy` does not fire inside `deployment`).
  All hits attach, name-ordered, at most 3 skills per turn, whether the
  message submits directly or drains from the durable queue. `/skill` turns
  union the explicit name (always first) with trigger hits under the same
  cap. Cron, briefing and other scheduled submits are exempt: conversational
  turns only.
- `paths = ["docs/*"]` — up to 8 flat `*` globs (the house matcher,
  `ka_agent_store::model_matches`; no new grammar). A tool call whose
  argument summary touches a matching path parks the skill; the **next**
  conversational submit attaches it (never mid-cycle — one writer per turn).
  The once-per-conversation marker (`skillpath:<session>:<skill>` in the
  store meta table) is written **at attach time**, not at touch time, so a
  daemon restart between touch and attach simply forgets that touch. A
  `Compacting { done: true }` clears the session's markers: the next matching
  touch re-parks and the next submit re-attaches. Nothing is re-injected
  eagerly across compaction, so no unbudgeted bytes cross the port.
- `allowed_tools = ["read", "grep"]` (the Claude-style `allowed-tools`
  spelling is accepted too) — up to 8 exact hand names. A
  `/skill <name> <text>` turn submits them as the worker protocol's
  `allowed_tools`; the runtime intersects them with the role's toolset
  (`intake.rs`, the cron-tools precedent), so a skill can only **narrow** a
  turn, never widen it. `${SKILL_DIR}`-style substitution is not supported:
  the protocol takes exact names.

**Provenance join.** Provisioning copies the directory into the engine home
and strips exactly those three keys from the copied `SKILL.md` with
`toml_edit` (comments and unrelated keys survive; the pinned engine never
sees unknown keys). The operator's source copy keeps them. Trigger, path and
tool binding therefore read `resolved()`: names/descriptions from the
provisioned engine copies, firing frontmatter from the operator library under
`user_dirs` precedence. An `[enabled, no source copy]` skill resolves with
empty firing metadata (explicit `/skill` only) and is marked by the doctor.

## `/skill-doctor`

Per resolved skill: estimated advertised cost (`~N tok`, name+description
bytes / 4 — the engine assembles the prompt, so this is an estimate), the
provisioned body size (`XKiB`), attach counts by route (`N× (explicit)`,
`N× (trigger)`, `N× (paths)`), last attach, and the skill's trigger/path
echo. Skills with no source copy carry a `no source copy` mark; triggerless
skills show no trigger echo. A `standing:` line totals the advertised tokens
and body bytes. Counts live in the store's `skill_usage` table (schema v15)
and are bumped only when a submit actually attached the skill.

**Shell lines are refused.** The engine runs every `SKILL.md` line shaped
`!`command`` (after leading whitespace; fenced blocks count) through `sh -c`
when the skill is invoked — no permission ask, no ledger entry. A skill is
text, not code, so such a skill is refused everywhere it enters, never
stripped: `skills enable` fails with
`skill "deploy" runs shell commands without approval (lines 12, 40): remove the !`…` lines`,
`ka-agent import` skips it (dry run prints the reason), `/skills accept`
refuses it, and an engine-home copy that predates the rule (or was edited in
place) is never put in `Submit.skills` — not by trigger, `paths` or
`/skill`, which answers with the same reason — and `/skill-doctor` marks it
`REFUSED, never attached`.

## What skills never get

No scopes, no grants, no sandbox carve-outs, no marketplace. Installing a
skill grants nothing; the engine's permission gates apply to skill turns
exactly as to any other turn.
