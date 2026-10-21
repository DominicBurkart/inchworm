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
