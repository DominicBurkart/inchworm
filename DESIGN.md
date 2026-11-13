# Inchworm design

Inchworm is a local coding-agent orchestrator. It runs sequences of small agents
over a repository's GitHub activity (issues, PRs, CI) and escalates only when
it has to.

## Principles

1. **100% local intelligence.** Agents run on local models. No frontier calls
   unless an offramp explicitly says so.
2. **Golden paths, with offramps.** An agent follows a sequence of steps. It may
   fall off that path, and that is fine. What happens next is configurable per
   journey via an *offramp*:
   - `noop`: do nothing
   - `issue`: open a GitHub issue describing the failure
   - `escalate`: hand off to a frontier agent
3. **Agents are fallible.** Every step is assumed to be able to fail, so
   outputs are checked (auditors) rather than trusted.
4. **Journeys are sequences of agents.** Each user journey is a specific
   sequence of agents, and each agent can be given stateful capabilities.
5. **Nanna only for coding agents.** Inchworm uses nanna for every coding
   agent. The nanna agent definitions in a directory are a superset of those
   used by the inchworm sequences.
6. **One daemon per user.** A single user-level daemon spawns agents and
   registers directories that contain an `inchworm.toml`.

## Onboarding

Inchworm should work out of the box. It ships default golden paths plus
"magic" guesses for configuring them. Dedicated **research agents** index a
project (CI setup, review conventions, test layout, branch policy, etc.) and
produce recommended `inchworm.toml` settings, which the user confirms or edits.

## Projects (journeys)

### 1. Issue RPE agents
Research-Plan-Execute agents for GitHub issues, with an **auditor after every
step**.
- Default scope: issues authored by the user running inchworm that have no open PR.
- Config: is there a code review agent to hand PRs to? (asked at onboarding)

### 2. CI failure root cause
Diagnose why CI failed on a PR head.
- Config: re-trigger CI for each of: `flake`, `transient_network`,
  `ambiguous` (possibly transient network/compute).
- Config: post the result as a GitHub comment?

### 3. Merge conflict resolution
Intelligent conflict resolution.
- Config: automatically convert open, unapproved PRs to draft when conflicts
  appear, and mark them ready again once the conflict resolves and checks pass.

### 4. Code review
Review agents, each producing its own report per commit head:
- **Test design review.** Steers contributors to a test-standards markdown file
  or directory.
- **CI root cause report** (project 2), per commit head.
- **Alignment review** against the linked issue, when there is one. Confirms
  the PR closes the issue, or highlights the gap.

### 5. Review addressers / shepherds
A dedicated addresser for each review type in project 4. It takes that
reviewer's findings and drives the PR toward resolving them.

## Config sketch

```toml
# inchworm.toml (per directory; registered with the user-level daemon)
[offramp]
default = "noop"            # noop | issue | escalate

[issues]
authors = ["me"]
skip_with_open_pr = true
code_review_agent = true

[ci]
retrigger = { flake = true, transient_network = true, ambiguous = false }
comment = true

[conflicts]
draft_on_conflict = true

[review]
test_standards = "docs/testing.md"
```

This sketch is illustrative; keys will change as the implementation firms up.

## Open questions

- Exact nanna agent-diff mechanism and how "superset" is checked.
- Auditor design: same model, different prompt, or separate model?
- How state is stored per journey and per capability.

---

Everything above this line is the project's design as written by the maintainer. The rest of this file records how the code relates to it and was added when the draft PRs were reconciled with the design (issue #6).

## Implementation status

Status values: **implemented** (code and tests on `main`), **config only** (parsed and validated, nothing executes it), **not implemented**, **open** (a decision is pending). The daemon does not exist yet. Dispatch exists as `inchworm run` (one flow for one issue, in the foreground; PR #4) and the judges, policy and scheduler exist as library code (PR #3), but `run` calls neither, and no auditor or offramp is ever run.

| Design concept | Where | Status |
|---|---|---|
| One daemon per user | not built; `poll_secs` and `concurrency` exist in the global config | not implemented (PR #5) |
| Registered directories with an `inchworm.toml` | `config::ProjectConfig`, `PROJECT_FILE`; `validate`; the global `[[repo]]` list is the registry and `inchworm list` prints it | implemented; `register` / `unregister` are a non-goal until a TOML-editing dependency is accepted |
| Golden path: a sequence of steps | `config::Flow`, `Step` (an ordered list of nanna identities); `flow`, `run` (PR #4) dispatch the steps to nanna in order | implemented for one issue in the foreground; publishing a pull request is not supported by nanna yet |
| Journey 1: issue RPE agents | `Trigger::Label`, one `[[flow]]`; research, plan and execute are the user's three steps | `inchworm run` runs it for one issue; no daemon and no auditor |
| Journey 2: CI failure root cause | the tagged `Trigger` enum is the extension point (issue #13 for a schedule trigger) | not implemented |
| Journey 3: merge conflict resolution | same extension point | not implemented |
| Journeys 4 and 5: code review, review addressers | flows with other identities; nothing specific | not implemented |
| Offramps `noop` / `issue` / `escalate` | `config::Offramp`, `OfframpConfig` (default plus per-flow); `validate` warns on `escalate` | config only. `noop` is the default and `escalate` must be named. Whether `issue` reuses nanna's escalation sink is **open**; `escalate` hands nothing anywhere |
| 100% local intelligence | the only setting that can leave local models is `escalate`, which is off | config only |
| Agents are fallible; auditors after every step | `config::AuditorConfig`, `AuditorPlace`, `StepAudit`, `AuditorConfig::plan_for` and `decide` | config only. The auditor is mandatory, cannot be skipped and cannot be the audited step's own identity; an auditor error halts the step. Where it runs is a nanna identity; `host` is rejected everywhere (project default or per step) because no host check exists. Every step needs at least one check, either the flow's judges or a named subset; a flow with no judges is an error because its auditor would verify nothing. Auditor design (same model, other prompt, other model) is **open** |
| Judges after a run | `config::Judge` (`diff`, `nanna_clean`, `llm`); `judges`, `policy::decide` (PR #3) | implemented as library code; `inchworm run` does not call them yet |
| Nanna only for coding agents | step identities are nanna identities; `validate` checks their syntax and `validate --online` (PR #4) asks nanna whether they exist and checks the labels | implemented |
| Merging | `[merge] allow` (default `false`), `MergeConfig::grant` giving `MergeGrantSetting::{Denied, Granted}`; a `merge_when` flow with `allow = false` is rejected | config only. This is the single mapping point for `policy::MergeGrant` (PR #3 re-exports `MergeGrantSetting` under that name): `allow = false` is `Denied`, `true` is `Granted`, and there is no second concept. Aligns with nanna-coder #1043, where the capability is still being designed |
| Agents cannot edit their own configuration | `validate` requires `inchworm.toml` and `.inchworm/**` in `.nanna/protected.toml`; `validate::protected_path_touched` is inchworm's own check of a patch | implemented as checks; nothing applies the patch check yet |
| State per journey and capability | `store` (SQLite runs and attempts, `DaemonLock`) | implemented for runs and attempts; per-capability state is **open** |
| Failure taxonomy and rates | `model::FailureClass`, `store` | implemented as data; no reporting command yet |
| Onboarding and research agents | none | not implemented |
| Prototype branch `ccr-a1899a5f-disb85` (a second GitHub client, nanna MCP client and daemon) | overlaps PRs #3, #4 and #5 | **open**: retire or harvest (delta D5) |
| Marigold or Rust control flow | undecided | **open** (delta D5) |

## Reading the table

- The config sketch in the design above is illustrative. Its tables `[issues]`, `[ci]`, `[conflicts]` and `[review]` are **not parsed yet**: `inchworm.toml` rejects unknown keys, so a file containing them fails validation until those journeys are built.

- "Config only" means a file can say it and `inchworm validate` checks it, but no running code acts on it. Do not read a configured auditor or offramp as protection.
- Safety today rests on three things that exist: container isolation in nanna, the protected-path list, and draft pull requests. A model auditor is not a safety gate: the nanna reviewer's scorecard for nanna's model auditor fails the zero-false-allow bar.
- The open questions at the top of this file stay open; this section does not answer them.
