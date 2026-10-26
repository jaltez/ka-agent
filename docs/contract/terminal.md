# Terminal chat contract (plan 3.1, redesigned v0.11)

`ka-agent chat` on a TTY uses an inline terminal client: the transcript
scrolls above, one live area stays at the bottom, and the draft is edited in
place. The piped path (no TTY on either end) keeps the line reader, so
scripts and tests are unaffected.

## Layout

```
☥ Ka · no model · local session · /help · Ctrl-C stops a reply, Ctrl-D quits   ← banner (once)
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
  context percentage, today's spend, and `plan done/total` when the model
  published one. Truncated to the terminal width with an ellipsis.
- Answers render through `ka_agent_core::markdown` (`Target::Ansi`): the
  same renderer, headings, fences, links and inline styles as the piped
  console. The streamed draft is a plain preview; the final text replaces it
  as the permanent block.

## Keys

| Key | Effect |
|---|---|
| Enter | submit (empty drafts are ignored) |
| Shift-Enter / Alt-Enter / Ctrl-J | newline inside the draft (folded to a space) |
| ←/→, Home/End, Ctrl-A/E | cursor movement |
| Alt-b / Alt-f, Ctrl-←/→ | word movement |
| Backspace / Delete | delete |
| Ctrl-W | kill the word before the cursor |
| Ctrl-K / Ctrl-U | kill to end / to start |
| ↑/↓ | history, filtered by the draft's prefix |
| Ctrl-R | incremental reverse search (Enter accepts, Ctrl-G cancels) |
| Tab | complete `/command` names and aliases, or `@path` in the workspace (≤ 10 shown, a single match inserts) |
| Ctrl-C | clear the draft; with an empty draft, stop the running turn; twice on an idle prompt quits |
| Ctrl-D | quit (EOF) |
| Ctrl-L | clear the screen, keep the live area |
| Ctrl-O | toggle tool detail rows |
| Ctrl-T | toggle streamed thinking |
| Ctrl-G | edit the draft in `$VISUAL`/`$EDITOR` (raw mode suspended) |
| Focus in/out (`ESC[?1004h`) | a reply finishing while unfocused rings the terminal bell and sends a desktop notification (`ESC]9;…`) |

Raw mode keeps output post-processing (`OPOST`) on so `\n` still starts at
column 0, enables bracketed paste and focus reporting, and restores the
terminal state on drop, on Ctrl-C and on panic.

## Approval and proposal cards

Cards print as permanent blocks above the live area: `y` allows, `n` denies,
`s` allows for the session (the one-letter answers resolve against the
newest pending card; the full forms `/y <id> [session]`, `/n <id>`,
`/memory accept|reject <id>`, `/skills accept|reject <id>` still work).

## Wiring and files

`apps/ka-agent/src/term/`: `raw.rs` (termios guard), `keys.rs` (byte decoder
with split-read buffering and an ESC timeout), `editor.rs` (chars, cursor,
wrapping, history, reverse search), `complete.rs`, `view.rs` (status line,
activity, redraw, Markdown flush), `cards.rs`, `mod.rs` (the driver the chat
loop polls beside `backend.next()`). Only one reader owns stdin: the
terminal when it is active, the line reader otherwise.

## Verification

Unit tests cover the decoder (split reads, UTF-8, paste, modifyOtherKeys),
the editor (kills, wrapping by display width, history walk and search), and
the view (activity steps, status truncation, tool rows). The real binary is
smoked in a tmux pane: banner, draft, Enter, tool row, answer, footer — and
the same session under `script` with a piped stdin for the split-read path.
