---
name = "review"
description = "Review a diff or patch carefully before it merges"
---

# Code review

Apply a careful, evidence-first review pass:

1. Read the whole diff once without commenting; form a hypothesis of intent.
2. Walk it again hunk by hunk. For each, ask: what invariant does this
   change, and who depends on that invariant?
3. Classify findings: correctness bugs, resource concerns (allocation,
   copying, wakeups), then style. Report in that order; never bury a bug
   under style notes.
4. Cite exact file and line for every finding. No finding, no comment —
   do not invent work to look thorough.
5. End with a one-line verdict: approve, approve-with-nits, or block with
   the blocking reason first.
