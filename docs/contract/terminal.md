# Terminal chat contract (plan 3.1, redesigned v0.11)

`ka-agent chat` on a TTY uses an inline terminal client: the transcript
scrolls above, one live area stays at the bottom, and the draft is edited in
place. The piped path (no TTY on either end) keeps the line reader, so
scripts and tests are unaffected.

The banner names `/help` and `/keys`; `/keys` prints the key table below,
so every binding is discoverable in-app.

## Layout

```
☥ Ka · no model · local session · /help · /keys · Ctrl-C stops a reply, Ctrl-D quits   ← banner (once)
» hello there                                                                 ← your submitted line
▸ running a command echo hi                                                   ← tool row while it runs
  ✓ bash 9ms                                                                  ← the row, settled
pong                                                                          ← the answer, rendered Markdown
· 1.2s · $0.0012 · 8 tok                                                      ← turn footer
⠋ ☥ Ka · ctx 12% · $0.0012 · plan 1/3                                          ← status line (live)
» draft                                                    
```

- The live area is the activity row (only while a tool runs), the status
  line, and the draft's soft-wrapped rows. Redraws are wrapped in
  synchronized-output markers (`ESC[?2026h/l`) and clear from the area's
  first row, so a shrink leaves no stale rows.
- Activity: `▸ <tool label> <detail>` steps to `▸ N tools running` for
  concurrent tools; `∴ thinking… <chars>` while reasoning streams before any
  answer text. Thinking text itself prints only with toggle (Ctrl-T) on.
- Status line: spinner while a turn runs, the persona name, the model or
  context percentage, today's spend, `⏸ paused`/`⚠ degraded` while the
  session sits in that state, and `plan done/total` when the model
  published one (an empty plan shows nothing). Truncated to the terminal
  width with an ellipsis.
- Answers render through `ka_agent_core::markdown` (`Target::Ansi`): the
  same renderer, headings, fences, links and inline styles as the piped
  console. The streamed draft is a plain preview; the final text replaces it
  as the permanent block. Command replies render the same way; notes,
  notices, cross-channel inbound lines and session moves print as
  transcript rows (`· …`).
- `NO_COLOR` (set in the environment) turns the TUI's own dim/bold/red
  styling into plain text; layout and Markdown rendering are unchanged.

## Keys

| Key | Effect |
|---|---|
| Enter | submit (empty drafts are ignored) |
| Shift-Enter / Alt-Enter / Ctrl-J | newline inside the draft — typed newlines fold to a space; **pasted** text keeps its real newlines (shown as `⏎`, submitted verbatim) |
| ←/→, Home/End, Ctrl-A | cursor movement |
| Alt-b / Alt-f, Ctrl-←/→ | word movement |
| Backspace / Delete | delete |
| Ctrl-W | kill the word before the cursor |
| Ctrl-K / Ctrl-U | kill to end / to start |
| Ctrl-Y | yank the last kill back into the draft |
| ↑/↓ | history; the walk filters by the draft it started with, and ↓ past the newest entry restores that draft |
| Ctrl-R | incremental reverse search (Enter accepts the hit into the draft; Esc or Ctrl-G cancels) |
| Tab | complete `/command` names and aliases, `@path` in the workspace, or — for the word right after `/model ` — model selectors: vendors first (`vendor/`), then that vendor's full `vendor/model` ids with the recommended one flagged (≤ 10 candidates; a single match inserts, several print above the live area and each further Tab cycles through them) |
| Ctrl-C | a non-empty draft is cleared; on an empty draft a running turn is stopped; twice on an idle prompt (within 2 s) quits |
| Esc | while a turn runs, stop it (the same as `/stop` — the engine's retry notes name this key); idle, dismiss the completion list |
| Ctrl-D | quit on an empty draft (EOF) |
| Ctrl-L | clear the screen, keep the live area |
| Ctrl-O | toggle tool detail rows (the invocation while a tool runs, and a failed call's error excerpt when it settles) |
| Ctrl-T | toggle streamed thinking |
| Ctrl-E | expand the last finished tool — invocation, duration and the output the runtime surfaced — as a permanent block |
| Ctrl-G | edit the draft in `$VISUAL`/`$EDITOR` (raw mode suspended and the stdin reader paused, so the editor owns every keystroke) |
| Focus in/out (`ESC[?1004h`) | a reply finishing while unfocused rings the terminal bell and sends a desktop notification (`ESC]9;…`) |

Raw mode keeps output post-processing (`OPOST`) on so `\n` still starts at
column 0, enables bracketed paste and focus reporting, and restores the
terminal state on drop and on panic (including the panic hook).

A lone `ESC` is emitted after a 30 ms pause: an arrow key arrives whole and
decodes immediately, while `Esc` followed by `Enter` inside that pause is
read as `Alt-Enter` (a newline in the draft, folded to a space). Control
bytes with no binding (`Ctrl-\`, `Ctrl-]`, `Ctrl-^`, `Ctrl-_`, NUL) are
consumed and ignored instead of stalling the decoder.

## Approval and proposal cards

Cards print as permanent blocks above the live area: `y` allows, `n` denies,
`s` allows for the session (the one-letter answers resolve against the
newest pending card; the full forms `/y <id> [session]`, `/n <id>`,
`/memory accept|reject <id>`, `/skills accept|reject <id>` still work).
Answering a card discharges it: a later bare `y`/`n`/`s` is an ordinary
message to the model, never a command against an already-settled id.

A turn that ends in a retryable error (`rate_limit`, `network`) prints
`· /retry re-sends the question; nothing runs automatically` under the
`! [class] …` row; `/retry` re-submits the newest `error`/`interrupted`
turn's stored text (never automatically).

## Wiring and files

`apps/ka-agent/src/term/`: `raw.rs` (termios guard), `keys.rs` (byte decoder
with split-read buffering, the ESC pause, the control-byte skip, and the
cooperative pause the external editor needs), `editor.rs` (chars, cursor,
wrapping, history, kill ring, the verbatim paste insert), `complete.rs`
(commands, `@paths`, and the `/model` selector table — vendors and
models in the picker's display order, built once at startup from the
models.dev registry cache, catalog fallback, no fetch),
`view.rs` (status line, activity, redraw, Markdown flush, the absorb arms
for every event the transcript shows), `cards.rs`, `paint.rs` (drawing ops
plus the `NO_COLOR` styling switch), `mod.rs` (the driver the chat loop
polls beside `backend.next()`). Only one reader owns stdin: the terminal
when it is active, the line reader otherwise — and while Ctrl-G's editor
runs, neither does (the reader is paused and drained).

## Verification

Unit tests cover the decoder (split reads, UTF-8, paste, modifyOtherKeys,
unbound control bytes, the ESC-pause flush), the editor (kills, yank,
wrapping by display width, pasted newlines, history walk and search,
newest-entry load, draft restore), the view (activity steps, status
truncation, tool rows, reply/note/notice/session arms, the empty-plan
guard, verbose tool rows), the completion of commands, paths and
`/model` selectors (vendor order, recommended flag, prefix filtering,
first-argument-only), the reverse-search state machine, the paint
`NO_COLOR` op map, and the chat's one-letter card answers (rewritten only
while the card is armed). The real binary is smoked in a tmux pane: banner,
draft, Enter, tool row, answer, footer — and the same session under
`script` with a piped stdin for the split-read path.
