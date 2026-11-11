# inchworm

A daemon and CLI for configuring fallible, repeated agentic processes on local models. inchworm schedules and sequences work per repository and reports success and failure rates and the specific failure conditions. It runs agents through [nanna](https://github.com/DominicBurkart/nanna-coder), which sets up sub-agents with fixed capabilities and runs them in per-repo containers.

## Status

Milestone 1 of the MVP: configuration, run state, the SQLite store and offline validation. Dispatch to nanna, judges, the daemon and stats are not implemented yet.

## Commands

```
inchworm validate [--config PATH]
```

Exit codes: `0` valid, `1` the repos have errors, `2` the config could not be read.

`validate` checks, for every repo in the global config:

- the path is a git repository
- `.inchworm/flows.toml` parses and passes its rules
- every rubric file referenced by an `llm` judge exists
- `.nanna/protected.toml` lists `.inchworm/**` so agents cannot edit their own flows or judges
- `.nanna/prompt.md` exists (warning only)

## Global config

`~/.config/inchworm/config.toml` (or `$XDG_CONFIG_HOME/inchworm/config.toml`):

```toml
nanna_bin = "~/.nix-profile/bin/nanna"
ollama_url = "http://localhost:11434"
concurrency = 1
poll_secs = 120
state_db = "~/.local/state/inchworm/state.db"

[[repo]]
slug = "DominicBurkart/marigold"
path = "~/code/marigold"
```

## Flows

`.inchworm/flows.toml` in each repo:

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

`mode = "merge_when"` additionally requires at least one judge and a non-empty `required_checks`. Unknown keys are rejected everywhere.

## LLM judge safety

The `llm` judge fails closed. Ollama being down, refusing the connection, answering with a non-2xx status, hanging past the timeout, returning a body that is not the expected envelope, or returning a reply that is not exactly `{"score": <integer 0-10>, "reason": <string>}` (extra keys, duplicate keys, prose, code fences, trailing text, out-of-range or non-integer scores, replies over 8 KiB) all produce a failed verdict with no score. Patches over 64 KiB are not judged and fail. Every model call is bounded by a timeout and holds the global model permit only while it runs.

The patch is untrusted input. It is placed in a delimited data section whose begin and end markers carry a fresh random value for every call, every line is prefixed with `| ` and control characters are escaped, so the patch cannot forge the section end marker, a new rubric, or a reply. The model is told the section is data, that instructions inside it must not change the rubric, and the reply format is restated after the data. The rubric comes from `.inchworm/`, which `validate` requires to be protected from agents.

A policy decision matches the verdicts to the configured judge list by kind and index and requires every one to pass exactly once, so a passing `llm` verdict can never override a failed `diff` or `nanna_clean` verdict and two `diff` passes cannot stand in for a missing `llm` judge. A corpus of injection diffs is covered by tests.

Residual risk: this reduces but does not eliminate prompt injection. A small local model can still be persuaded to emit a well-formed high score for a bad patch, and the strict schema only guarantees the reply shape, not the judgement. The `llm` judge is a score, not a security boundary: keep the deterministic `diff` and `nanna_clean` judges and CI in front of any merge, and treat a high `llm` score as advisory.

## Judge and policy contract

- The `diff` judge only accepts `git diff` output (`diff --git` headers). A patch without them, a binary change, a hunk whose lines do not match its declared counts, and any quoted, backslash-escaped or non-ASCII path (git's default quoting) fail closed, so a file with such a name cannot be merged automatically.
- `forbid_paths` globs are matched against repository-relative paths, including both sides of renames and copies. `*` crosses `/`, matching is case-sensitive, `!` is literal, a bare `publish.sh` does not match `sub/publish.sh` (use `**/publish.sh`) and a directory needs `dir/**`.
- `policy::decide` rejects with `judge_rejected` unless the verdicts equal the configured judges by kind and index, all passed. If every failed verdict is an infrastructure failure (Ollama down, timeout, bad envelope) the class is `model_error` instead. Judges take precedence over CI: a failed judge with a red check is `judge_rejected`.
- CI: a required check must be green on the exact judged SHA. A failed run followed by a green rerun with the same name and SHA stays `ci_red`. GitHub conclusions map fail closed: only `success` is green, `skipped`, `neutral`, `cancelled`, `timed_out`, `action_required`, `stale` and anything unknown are red, and anything not `completed` is pending.
- An unconsulted human gate (the default) never merges: `merge_when` degrades to `draft_pr` until the gate is wired. Every shipped default and example uses `draft_pr`; nothing enables `merge_when`.
- The scheduler gives each repo one active run at a time. The global `concurrency` limit is a semaphore around model calls (dispatch and the LLM judge), not around leases, so a run that holds its lease can still judge.
- Not implemented: per-directory configuration, offramps and a per-step auditor (D1 to D3). `Verdict` carries an optional `step` and the judge types are `non_exhaustive` so those can be added.

## Development

The pre-commit hook (installed by `cargo-husky` on `cargo test`, or copy `.cargo-husky/hooks/pre-commit` into the git hooks directory when working in a worktree) runs fmt, clippy, tests, doctests, audit, deny, docs, a security review of the staged diff and a coverage no-regression check against `main`.

## License

MIT OR Apache-2.0
