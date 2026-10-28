# Ka Agent

A resident assistant on the [Ka](https://github.com/jaltez/ka-harness) coding
engine: one always-on daemon per operator that keeps working across sessions
and channels — durable memory, scheduled tasks, approvals, Telegram, Matrix —
while all model reasoning and tool execution stay in Ka engines that Ka Agent
starts, watches and stops.

## Quickstart

Linux and macOS (Windows: see [Platforms](#platforms)):

```sh
curl -fsSL https://raw.githubusercontent.com/jaltez/ka-agent/main/scripts/install.sh | sh
ka-agent                              # first run: the setup wizard
```

The wizard takes about three minutes: it picks a provider, verifies the API
key live, picks a model, detects your timezone, offers to pair Telegram or
Matrix by one-time code, offers web search, offers to install the background
service, and ends with a real first reply and a "you're live" card. Nothing
is written until you confirm. From source instead: `cargo install --path
apps/ka-agent`.

Installer knobs (all optional): `KA_AGENT_INSTALL_DIR` (default
`~/.local/bin`), `KA_AGENT_NO_SETUP=1`, `KA_AGENT_INSTALL_FROM=<tarball>`.

After setup, four things cover most days:

```sh
ka-agent                              # chat (attaches to the background service)
ka-agent dashboard                    # web control panel in your browser
ka-agent status                       # one-glance report
ka-agent doctor                       # full health check; exit 1 = fix something
```

Two rules of the house, both enforced by code:

- **Every config is explicit.** Missing or invalid config is an error, never
  a silent default (`ka-agent setup` writes the first one).
- **Nothing external happens unapproved.** External writes need a grant, are
  logged as intents (deduplicated by content hash), and approvals deny on
  expiry.

## Talking to it

Plain text is a prompt; `/commands` are optional (`/help` lists them, and
`@file`, `@dir/`, `@turn:<id>` or `@https://…` injects context). The terminal
and every paired phone share one session: an answer asked on Telegram shows
in your terminal too.

| | |
|---|---|
| `/status` | model, spend vs limits, queue, next task |
| `/mode [m]` | ask (default, asks) · accept_edits · allow · readonly (read-only) — badge when not ask; the old names guarded/free/plan still work |
| `/model provider/model` | switch model, applied live |
| `/new` · `/stop` · `/resume` | fresh session · abort reply · lift a pause |
| `/remember <fact>` · `/forget <id>` | durable memory (git-backed; `/memory` lists ids) |
| `/history [query]` · `/history show <id>` | past turns |
| `/remind <when> <text>` · `/reminders` | notifications, no model turn |
| `/cron add …` · `/calendar add <url>` | scheduled prompts · daily digest |
| `/approvals` · `/y [id]` · `/n [id]` | decide pending approvals |
| `/skills` · `/skill <name> <text>` | one turn with a skill loaded |

Scheduling times are always your local time:

```
/remind in 2h30m stretch
/remind at 18:30 call the dentist
/remind tomorrow 9am send the invoice
/remind fri 17:00 week notes
/cron add every 1h check the build
/cron add every day at 08:00 summarize my inbox
/cron add every mon at 09:00 plan the week
```

In a terminal the chat is a live client: the transcript scrolls above while
the status line (spinner, context, spend, plan), tool rows and the draft stay
in place below; `Tab` completes commands and `@paths`, `Ctrl-R` searches the
history, `Ctrl-G` opens your editor on the draft, and `y`/`n`/`s` answer a
pending approval card. `Ctrl-C` stops a running reply, twice quits; `/quit`
leaves. Details: `docs/contract/terminal.md`.

## What it can do

Out of the box Ka works in its workspace (read, write, edit, run commands —
behind approvals in the default `ask` mode), keeps notes and memory,
and schedules. Three settings widen its reach:

```sh
ka-agent setup search                 # web search: Tavily (free tier) or Brave
ka-agent mcp add notes -- npx -y some-notes-mcp     # any MCP server's tools
ka-agent mcp add home --url http://homeassistant.local:8123/mcp \
  --header Authorization=secret:ha.token            # secrets stay in the store
ka-agent config set fallback '["openai/gpt-5.1"]'   # if the main model fails
```

MCP tools appear to the model as `<server>__<tool>`; in `ask` mode each
call asks first (the card names the tool). `secret:<name>` values come from
`ka-agent secret set <name>` when the agent starts and never touch the
config file. `ka-agent status` shows a `tools:` line with what is on, and
`ka-agent doctor` checks the keys and secrets they need.

## Channels (phone)

```sh
ka-agent pair telegram        # prints /pair XXXX-XXXX to send your bot
ka-agent pair matrix          # same idea for Matrix
```

Pairing happens once; the bot then answers only you (unknown senders stay
silent, and anything they tried shows in `ka-agent identity pending` with the
exact `identity link` command). Telegram setup needs a bot token from
@BotFather — the wizard asks for it and verifies it; Matrix asks for a bot
account and logs in once. Photos and files sent on Telegram land in
`<workspace>/attachments/` with an `@reference` you can paste back.

On the phone, replies stream in as they are written (Telegram drafts),
every message you send gets a 👀 receipt, and Matrix shows formatted text.
When Ka proposes remembering something, you get a card — ✅ Remember /
✖ Discard — instead of a queue to check. Voice notes work once you pick a
transcriber:

```sh
ka-agent setup voice     # OpenAI or Groq Whisper, or a local Whisper server
```

The voice note is transcribed, shown back to you (`🎙 “…”`), and handled
like typed text.

## Daily briefing

```sh
ka-agent briefing on 07:30    # every morning: calendar, reminders, what waits for you
ka-agent briefing now         # one right away
ka-agent briefing off
```

One message a day instead of a stream of nudges: it lists what needs
action and says "Nothing notable today." when that is the truth.

Scheduled prompts take local times and days — `/cron add every weekday at 9
standup notes`, `every mon,wed,fri at 18:00 …`, `every month on the 1st at 9
…` — from chat or from the shell (`ka-agent cron add …`, `cron list`, `cron
pause|resume|run|runs <id>`). Every fire is recorded; a job that cannot run
(no model, budget reached) is paused or skipped without spending anything.
`ka-agent config set notify.quiet_hours '"22:00-07:00"'` keeps scheduled
prompts quiet at night (reminders still ring).

## Configuration

Ka Agent keeps everything under `~/.ka-agent` (override with
`$KA_AGENT_HOME`), like other agents:

```
~/.ka-agent/
  config.toml                  # the default profile's config ($KA_AGENT_CONFIG overrides)
  skills/                      # your SKILL.md library (shared by all profiles)
  active_profile               # written by `ka-agent profile use`
  profiles/default/            # data for the default profile
    workspace/  state.db  memory/  secrets/  backups/  logs/  …
  profiles/work/               # another profile: same tree + its own config.toml
```

`$KA_AGENT_DATA_DIR` replaces the data root wholesale (tests, service
units). The config is strict — unknown keys are errors.

**Profiles** are independent instances — data, memory, secrets, workspace,
config, web port, channels and service units:

```sh
ka-agent profile create work          # config from default, minus channels; next free web port
ka-agent profile create work --copy-keys   # also copy the provider keys
ka-agent -p work setup                # any command takes -p/--profile
ka-agent profile use work             # make it the default for commands without -p
ka-agent profile list | show [name] | remove <name> --yes
```

Precedence: `-p` > `$KA_AGENT_PROFILE` > `profile use` > `default`.
`ka-agent -p work service install` installs `ka-agent-work.service` /
`dev.ka-agent.work.daemon`, so profiles run side by side. A new profile
starts without Telegram/Matrix — one bot token can only be polled by one
daemon; give each profile its own bot. A fully
commented copy of the schema is in
[`examples/config/ka-agent.toml`](examples/config/ka-agent.toml). Edit it
with `ka-agent config set <key> <value>` / `config edit` — comments and key
order survive, safe changes apply to the running agent live, the rest tells
you to `ka-agent restart`. Key settings: `model`, `mode` (`ask` default),
`timezone`, `budgets.daily_usd` / `daily_tokens`, `worker_idle_timeout_secs`,
`[web]`, `[calendar]`.

Secrets never go in the file:

- Provider key: `ka-agent key set ANTHROPIC_API_KEY` (0600, in the engine's
  `.env` — exactly what ka's own key prompt would write).
- Channel tokens: `ka-agent secret set <name>` (0600 under `<data>/secrets/`,
  referenced by name from the config).

A pre-0.3 install (`~/.local/share/ka-agent` + `~/.config/ka-agent`) moves
itself to the new layout on the next run.

## Staying current, switching in

```sh
$ ka-agent update check
0.8.1 → 0.9.0
$ ka-agent update
0.8.1 → 0.9.0
✓ verified
✓ installed · service restarting
```

The checksum must match before anything moves; a mismatch installs nothing.
`ka-agent import hermes` (also `openclaw`, `claude`, `codex`) brings memory
and skills from another agent.

Imports respect the rules: another agent's memory arrives as **candidates**
you accept one by one, skills land in the shared library.

## Running it for real

```sh
ka-agent service install      # units + start at login + nightly 03:30 backup timer
ka-agent service status       # service-manager state + daemon answering?
ka-agent logs -f              # the daemon's journal
ka-agent restart              # graceful stop; the service brings it back
ka-agent service uninstall    # stop and remove the units
```

`service install` is idempotent: run it again after upgrading and it rewrites
the unit files with the new binary and restarts. It refuses while a
foreground `ka-agent run` or chat holds the data root. On Linux it uses
systemd user units (`sudo loginctl enable-linger $USER` keeps it alive after
logout — the installer tells you if that is missing); on macOS, launchd.
Without a service manager (containers, WSL without systemd) run
`ka-agent run` inside a terminal multiplexer.

Backups land in `<data>/backups/<timestamp>` and rotate to the newest 7
(`--keep N` overrides; a named output is never pruned). Restore with
`ka-agent restore <dir>` — it refuses a non-empty data root unless `--force`.

## When something looks wrong

```sh
ka-agent doctor               # one line per check, each with the fix; exit 1 = fix something
ka-agent doctor --online      # also verify the provider key and channels live
ka-agent logs -n 200          # what the daemon just did
```

A chat that answers "no model configured" needs `ka-agent setup` (or
`/model provider/model`); a key rejection names the exact
`ka-agent key set …` to run. `ka-agent status` shows pauses, crashes and the
next scheduled task.

## Web control panel

On by default at `http://127.0.0.1:8480` (loopback only, `[web]` in the
config):

```sh
ka-agent dashboard            # prints + opens a one-shot login URL (10 min TTL)
```

Chat with the agent live (streaming over SSE), browse status, tasks, memory,
skills, approvals, history and contacts. The URL sets an
`HttpOnly; SameSite=Strict` session cookie; mutations are POST-only.
Details: `docs/contract/web-ui.md`.

## Platforms

Linux first-class (plugins use bubblewrap; services use systemd user units)
and macOS (launchd; plugins need Linux). Windows x86_64 builds and runs:
the admin endpoint is a named pipe, workers are killed with `taskkill /T`,
the service is a Task Scheduler entry, and the terminal chat uses the console
API (line mode where raw mode is unavailable); plugins stay Linux-only.
Details: `docs/contract/windows.md`.

---

## For contributors

**Layout.** `apps/ka-agent/` is the binary (daemon, chat, setup, admin CLI,
hidden `__worker`/`__mcp`); `crates/` holds `ka-agent-core` (domain types,
no I/O), `ka-agent-store` (SQLite, audit chain, FTS5), `ka-agent-port` (the
`KaPort` trait — the only crate beside the binary that may touch `ka-*`),
`ka-agent-runtime` (sessions, queue, approvals, budgets, worker supervisor,
scheduler), `ka-agent-memory` (git-backed Markdown compiled into the
engine's `MEMORY.md`), `ka-agent-channels` (Telegram + Matrix + shared
router), `ka-agent-web`, `ka-agent-plugins` (signed archives, bubblewrap
sandbox). `docs/contract/` is one behavior contract per subsystem; `doctor`
embeds their hash so support can compare builds.

**The ka pin.** The `ka-*` engine crates are pinned to one ka-harness git
revision, `193ad5d` (the `v0.4.1` line noted in `Cargo.toml`), which
`doctor` reports as `KA_PIN`. Upgrades are deliberate: bump the revision
in every `ka-*` dependency together, keep the contract suite green
(SEED §11.3); `scripts/check-ka-deps.sh` fails CI if any one of those
pins or the boundary drifts.

**Build & test.**

```sh
cargo build --workspace && cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
./scripts/check-ka-deps.sh
```

The workspace denies `unwrap`/`expect`/`todo`/`unimplemented`/`dbg` and
forbids `unsafe`; CI runs fmt, clippy, tests and the dependency gate on
Linux and macOS, and builds the binary on Windows
(`.github/workflows/ci.yml`); `release.yml` cuts tarballs for four targets
plus a Windows zip on `v*` tags, each with a SHA-256 file and a Sigstore
build-provenance attestation (`gh attestation verify ka-agent-<target>.tar.gz
--repo jaltez/ka-agent`). Windows installs with
`irm https://raw.githubusercontent.com/jaltez/ka-agent/main/scripts/install.ps1 | iex`.

**Further reading.** `SEED.md` (architecture seed),
`docs/contract/` (subsystem contracts), `docs/adr/0001-instance-per-user.md`,
`docs/ka-interface-validation.md`, `examples/plugins/probe` (a plugin, end to
end).
