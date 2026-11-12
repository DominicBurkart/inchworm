# inchworm

A daemon and CLI for configuring fallible, repeated agentic processes on local models. inchworm schedules and sequences work per repository and reports success rates and specific failure conditions. It runs agents through [nanna](https://github.com/DominicBurkart/nanna-coder), which sets up sub-agents with fixed capabilities and runs them in per-repo containers.

## Status: partial, and not yet aligned with the project design

This is milestone 1 of an MVP: configuration, run state, a SQLite store and offline validation. It cannot run an agent yet.

It was written from a planning conversation, not from the project's design. The design is issue [#1](https://github.com/DominicBurkart/inchworm/issues/1) and `DESIGN.md`, which exists only on the unmerged branch `ccr-a1899a5f-disb85`. Where the two differ, the design wins. The differences are tracked in issue [#6](https://github.com/DominicBurkart/inchworm/issues/6) and are **not met** here:

- **D1**: a directory is configured here by a global `[[repo]]` list plus `.inchworm/flows.toml`, not by a per-directory `inchworm.toml`; there is no `register`, `unregister` or `list`.
- **D2**: there are no offramps (`noop`, `issue`, `escalate`).
- **D3**: there are no per-step auditors; the model has judges after a run.
- **D6**: `DESIGN.md` is not on `main`.

Everything below describes what this code does today.

## Merging is off

`merge_when` exists as a flow outcome and is validated, but it is **off by default and no shipped example enables it**. `inchworm validate` warns when a flow turns it on. The intended direction is that merging becomes a capability a user grants to a nanna subagent or another agent, with the user defining when and how (nanna-coder issue 1043). Do not enable `merge_when` in a real repository.

## Commands

```
inchworm validate [--config PATH]
```

Exit codes:

- `0`: everything validated
- `1`: at least one repository has errors (listed on stdout)
- `2`: the command could not run: bad usage, no config could be found, or the config could not be read or parsed (the reason is on stderr)

`validate` is offline. It does not talk to GitHub, nanna or a model. For the global config it checks that `nanna_bin` is an executable file, or, for a bare name, that it is on `PATH`. For every repository it checks:

- the path is a git repository (a `.git` file must point at a gitdir that exists)
- `.inchworm/flows.toml` is a regular file of at most 256 KiB and passes the flow rules below
- every `llm` judge rubric is a regular file, not a symlink, that resolves inside `.inchworm/`
- `.nanna/protected.toml` is a regular file of at most 64 KiB, has only the key `extra`, at most 256 entries that are normalised relative literal paths (optionally ending in `/**`), and lists `.inchworm/**`
- `.nanna/prompt.md` exists (a warning only)

What a passing `validate` proves: the files inchworm reads are well formed and `.inchworm/**` is declared protected. It does not prove nanna accepts the same files (it mirrors the documented loader rules, not its code), that identities named in flows exist, that labels exist, or that branch protection matches `required_checks`; those online checks are not built yet.

## Global config

`~/.config/inchworm/config.toml` (or `$XDG_CONFIG_HOME/inchworm/config.toml`):

```toml
nanna_bin = "nanna"
ollama_url = "http://localhost:11434"
concurrency = 1
poll_secs = 120

[[repo]]
slug = "DominicBurkart/marigold"
path = "~/code/marigold"
```

- `nanna_bin` and each repo `path` may start with `~`, which needs `HOME` set to an absolute path. A path to a binary must exist: the example above resolves `nanna` on `PATH` and fails `validate` if it is not there.
- Defaults: `ollama_url = "http://localhost:11434"`, `concurrency = 1` (1 to 64), `poll_secs = 120` (1 to 86400). `state_db` is optional and, when unset, defaults to `~/.local/state/inchworm/state.db`; `validate` never reads it.
- Unknown keys are rejected everywhere.

## Flows

`.inchworm/flows.toml` in each repository:

```toml
[[flow]]
name = "issue"
trigger = { kind = "label", label = "inchworm" }
base = "main"
max_attempts = 3
retry_on = ["model_error", "agent_failed", "tests_failed", "orchestrator_lost", "judge_rejected"]
attempt_timeout_secs = 7200

[[flow.step]]
name = "implement"
identity = "rust-implementer"

[[flow.step]]
name = "publish"
identity = "pr-shepherd"

[[flow.judge]]
kind = "diff"
max_lines = 400
forbid_paths = [".github/**"]

[[flow.judge]]
kind = "nanna_clean"

[[flow.judge]]
kind = "llm"
rubric = ".inchworm/rubric.md"
model = "gemma4:e4b"
min_score = 7

[flow.outcome]
mode = "draft_pr"
required_checks = ["Tests"]
```

- Defaults: `base = "main"`, `max_attempts = 3`, `attempt_timeout_secs = 7200`. `retry_on` defaults to **empty**, so nothing is retried unless you list failure classes, even though `max_attempts` defaults to 3. `judge` defaults to none and `outcome.merge_method` to `merge`.
- A rubric must be a relative path under `.inchworm/` (no `..`, no absolute path), so agents, which cannot write `.inchworm/**`, cannot change what judges read.
- `retry_on` may not repeat a class or list `cancelled` or `escalated`, and needs `max_attempts` of at least 2.
- `mode = "merge_when"` also requires at least one judge and a non-empty `required_checks`. See "Merging is off" above.
- Unknown keys are rejected everywhere.

## Development

The pre-commit hook runs fmt, clippy, tests, doctests, audit, deny, docs, a security review of the staged diff (it blocks unless the review answers exactly `VERDICT: SAFE`) and a coverage no-regression check against the merge base with `origin/main` (or `INCHWORM_COVERAGE_BASE`). `cargo-husky` installs it on `cargo test`; in a git worktree it cannot, so copy `.cargo-husky/hooks/pre-commit` into the repository's hooks directory by hand.

The toolchain is the `stable` channel with rustfmt and clippy. It is not pinned to a version, so it moves with each stable release. CI runs the same checks, and compares coverage with the pull request's base commit.

## License

MIT OR Apache-2.0
