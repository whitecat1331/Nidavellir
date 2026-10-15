---
name: nidavellir-init
description: First-run onboarding for a fresh Zed user. Sets up the skills directory, scaffolds the plan system, installs the optional dev-flow "rails" so Zed guides the development process, and prints a post-setup map of what Zed will do automatically vs. what stays manual. Use on a new machine, or when a new user wants Zed to drive their workflow until they learn its features and are ready to relax the rails.
---

# nidavellir-init

You are onboarding a person who is **new to this Zed setup**. Your job is not to
toss files around — it is to walk them through setup one phase at a time, so they
understand what each piece is, what it buys them, and what they give up if they
skip it. Work **ask-then-act**: for each phase, explain first, ask, then do only
what they chose.

Three standing rules for the whole skill:

1. **Cross-machine generic.** Do not reference any particular machine, hostname,
   IP, VM, repo, or person. This skill runs on a machine that is not the author's.
2. **No credentials, ever.** You never create, copy, or store API keys or
   passwords. When a credential is required, you point at where to configure it
   and leave the value to the user.
3. **Nothing is silently installed.** Every change is explained and confirmed.

## The phases at a glance

| # | Phase | Default | What it gives the user |
|---|-------|---------|------------------------|
| 0 | Orient | — | Correct defaults for everything after it |
| 1 | Skills install | on | The workflow skills become `@`-invokable |
| 2 | Plan scaffolding | on | A plan lifecycle the agent can guide them through |
| 3 | Rails | on | Zed guides plan → verify → merge and stops for the human decisions |
| 4 | Config surface map | — | A map so rails can be relaxed one at a time later |
| 5 | Verify + smoke | on | Proof the setup actually works |
| 6 | Post-setup map | — | The "how do I keep going" reference, printed last |

Run the phases in order. Each phase below is written as four labelled beats:

- **What it does** — the concrete action.
- **What it adds** — the capability they gain if it runs.
- **If skipped** — what stays off or goes unguided.
- **Ask** — the question(s) to put to the user before acting.

---

## Phase 0 — Orient

**What it does.** Establishes where the user is starting from, so every later
default is right instead of guessed. Confirm: (a) the build is running and is the
expected one, (b) where skills should live, (c) what they build, and (d) whether
the language model is configured.

**What it adds.** Correct install/rail decisions, and it catches the one hard
dependency before any of the other phases: a configured language-model provider.
Nothing else in this skill can be exercised end-to-end until the model works.

**If skipped.** Later phases fall back to safe defaults, but may install skills
the user won't use, or leave the model unconfigured so every later "verify" step
fails for the wrong reason.

**Ask.**
- Is this a brand-new machine, or does it already have skills/config from before?
- What are you planning to build? (Language and rough shape steer the starter rails.)
- Zed only, or Zed plus a terminal/Neovim workflow too?
- Is your language-model provider already configured in Zed (Settings → AI /
  Assistant)? If not, point them there now and note it as **still-manual** for
  Phase 6 — you do not configure it for them.

---

## Phase 1 — Skills install

**What it does.** Explains the two places skills live, then installs the bundled
skills into the global skills directory (and any project-local copies the user
asks for):

- **Global** (`~/.agents/skills/<name>/SKILL.md`) — available in every project.
- **Project-local** (`<repo>/.agents/skills/<name>/SKILL.md`) — scoped to one repo.

For each bundled skill, state in one line what it does and when to `@`-invoke it.
Do not install a skill without naming it.

**What it adds.** The workflow skills — planning, atomic commits, docs, search,
refactoring, security review — become `@skill-name`-invokable, which is what lets
the agent drive the later phases (especially planning and rails) rather than the
user hand-holding each step.

**If skipped.** The agent falls back to raw Zed: it can still edit and run
commands, but there is no plan lifecycle, no commit discipline, and no shared
vocabulary — "start work" means "figure out where to begin" every time.

**Ask.** Install the bundled skills globally? Which project(s), if any, should
get project-local copies too?

---

## Phase 2 — Plan scaffolding ("plan init")

**What it does.** Creates the plan folders in the user's chosen project(s) —
`plans/initial-plans/`, `plans/in-progress-plans/`, `plans/completed-plans/`,
`plans/rejected-plans/` — plus a `plans/PLAN_INTEGRATION.md` index. If the user
has no project yet, explain that the plan skill creates these on demand the first
time a plan is started, and offer to seed them in a starter repo instead.

**What it adds.** A durable plan lifecycle the agent can walk them through:
`draft → plan → in-progress → completed (or rejected)`, each plan carrying a
`Stage`, a `Status`, and the evidence it is grounded in. "What should I do next"
becomes an answerable question instead of an open one.

**If skipped.** Work is ad-hoc. There is no record of what was decided, no way to
pick up a half-finished idea later, and nothing for the agent to check against
before it merges — which is exactly the failure the rails in Phase 3 exist to
prevent.

**Ask.** Which project(s) should get plan folders? (Default: the current project.)

---

## Phase 3 — Rails (opt-in, default ON)

**What it does.** Installs the starter rails — a project `AGENTS.md` (and, if the
user wants, a matching `.rules` file) — so **Zed guides the development process
while the user is still learning it**. The rails encode the loop the agent should
enforce:

1. **Plan before you build** — start work as a plan (`@plan-manager`), not a
   silent edit.
2. **Verify before you merge** — the agent runs the checks itself; it only stops
   for the *human decisions* (approve completion, approve a launch, approve a
   merge/release).
3. **Never skip testing on the way to merge** — if the agent proposes to jump
   straight to merge/release with a gate unrun, it must be told to stop.
4. **Commit small** — `@atomic-commits` splits work into small scoped commits.
5. **You keep the decisions** — the agent does the work; the user approves the
   human gates.

Point at the `dev-flow` skill as the canonical map of this loop. If `dev-flow` is
not installed, summarize the loop inline from the rails above instead.

**What it adds.** A beginner gets the same guardrails an experienced user has:
the agent will plan first, run tests/builds before merging, and pause for the
decisions that are genuinely the user's. They do not have to remember the process
— the rails make Zed remember it for them.

**If skipped.** Zed is fully permissive. It will happily edit, and — because
nothing says otherwise — skip verification and jump straight to merge/release. A
new user has no signal that anything is missing until a broken change ships.

**Ask.** Enable the rails? (Strongly recommend yes.) If yes, which language(s)
should the starter rails be tuned for?

Starter `AGENTS.md` text (generic — edit the "what you build" line to match
Phase 0):

```markdown
# Development workflow (starter rails)

Zed is guiding your process. These rails are optional — relax them one at a time
as you get comfortable. Each rule names the skill that enforces it.

## Plan before you build
- Start work as a plan, not a silent edit: `@plan-manager` (draft → plan → in-progress).
- Ask the agent "what's the next step?" — it consults the plan and the dev-flow map.

## Verify before you merge
- The agent runs tests/builds/checks itself. It only stops to ask you at the
  human decisions: approve plan completion, approve a build launch, approve a
  merge/release.
- Never merge or release until the full plan is verified and accepted. If the
  agent proposes to skip testing or jump straight to merge, stop it.

## Commit small
- `@atomic-commits` splits work into small scoped commits.

## You keep the decisions
- The agent does the work; you approve the human gates.
```

---

## Phase 4 — Config surface map

**What it does.** Explains **where** the rails and workflow actually live, so the
user can relax them deliberately later instead of tearing config out wholesale.
Walk through, in this order:

1. **Skills** — `~/.agents/skills/` (global) and `<repo>/.agents/skills/`
   (project). Adding/removing a skill changes what the agent can be `@`-asked to
   do.
2. **`AGENTS.md` / `.rules`** — the rails and per-project instructions. Deleting
   or editing these is how you loosen the workflow guardrails.
3. **Settings** — Zed's own settings (AI provider, tool toggles). Most "turn
   that feature off" requests land here, not in skills.
4. **Keymap** — keybindings; the least workflow-critical surface, and the easiest
   to change safely.

**What it adds.** A mental model of "what do I touch to change which behavior" —
so removing a rail later is a one-line, understood change, not a scary reset.

**If skipped.** No map. When the user gets comfortable and wants to relax a rail,
they are likely to delete a whole file or factory-reset, and lose the parts they
wanted to keep.

**Ask.** None required — this is informational. (Offer to open the settings file
if they want to look now.)

---

## Phase 5 — Verify + smoke

**What it does.** Confirms the setup actually works, without reverting anything.
Have the agent: start a trivial plan, promote it one step, make a one-file change,
commit it small, and run the checks — pausing exactly at the human-decision points
the rails define. Then report which rails fired and which did not.

**What it adds.** Proof the plan lifecycle, skills, and rails are live — so the
user's first real task doesn't also double as the test run.

**If skipped.** The setup is unverified; the first real task will surface any
misconfiguration mid-work.

**Ask.** Run the smoke check now? (Recommended yes; it is read-mostly and makes no
permanent changes to their project beyond the test commit, which can be undone.)

---

## Phase 6 — Print the post-setup map

**What it does.** This is the last thing you do, and the artifact the user keeps.
Print a summary **derived from what was actually enabled in this run** — not a
static list. Do not include a feature they skipped, and do not claim a rail is
active if it was not installed.

For each enabled feature, print three lines:

- **Trigger** — how to invoke it (`@plan-manager`, `@dev-flow` / the dev-flow map,
  `@atomic-commits`, plan promotion, "what's the next step?").
- **What Zed does automatically** — the work the agent now performs without
  asking (planning, running tests/builds, committing small, checking gates).
- **What still needs you** — the human decision(s) this feature deliberately
  leaves to the user.

Then print a consolidated **still-manual** list, so the user knows what they will
always do by hand:

- **Human decision gates** — approve plan completion / pick the next plan /
  approve a build launch / approve a merge & release. The agent proposes; you
  dispose.
- **Credentials & accounts** — the language-model provider key (and any other
  third-party auth) is yours to configure; the agent never handles it.
- **Anything skipped this run** — for each skipped phase, one line on what it
  would have added and how to enable it later (re-run `@nidavellir-init`, or the
  specific skill named).

Finally, one paragraph on **relaxing the rails**: start by editing `AGENTS.md`/
`.rules` (remove one rule at a time), then revisit settings, and only touch skills
last — each rail is independent, so they loosen incrementally as confidence grows.

**What it adds.** The single "how do I keep going without asking what to do next"
reference. After the skill exits, this printed map is what the user re-reads.

**If skipped.** The user is left with a working setup and no compass — they won't
know what fires automatically, what they must approve, or how to turn the rails
down when they're ready.

**Ask.** None — this is the closing summary. Print it, then stop.
