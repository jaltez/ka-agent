# Skills contract — SEED.md §9.1 (minimal mechanism)

Status: implemented, 2026-10-18, verified by e2e (`skills_enable_list_invoke`)
and unit tests. A skill is **text, not code** (SEED.md §2.6): it changes what
the model knows how to do, never what it is allowed to do.

## Shape and discovery

- One directory per skill under the operator's library:
  `~/.config/ka-agent/skills/<name>/SKILL.md` (`$XDG_CONFIG_HOME`-aware).
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

- `ka-agent skills list` — the operator's library with `[enabled]` markers,
  plus `[enabled, no source copy]` for provisioned-without-source entries
  (e.g. after a restore).
- `ka-agent skills enable <name>` — provision. `disable <name>` — remove
  the provisioning only.

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

## What skills never get

No scopes, no grants, no sandbox carve-outs, no marketplace. Installing a
skill grants nothing; the engine's permission gates apply to skill turns
exactly as to any other turn.
