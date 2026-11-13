# inchworm

A daemon and CLI for configuring fallible, repeated agentic processes on local models. inchworm schedules and sequences work per repository and reports success rates and specific failure conditions. It runs agents through [nanna](https://github.com/DominicBurkart/nanna-coder), which sets up sub-agents with fixed capabilities and runs them in per-repo containers.

## Status: partial, and not yet aligned with the project design

This is milestone 1 of an MVP: configuration, run state, a SQLite store and offline validation. It cannot run an agent yet.

It was written from a planning conversation, not from the project's design. The design is issue [#1](https://github.com/DominicBurkart/inchworm/issues/1) and `DESIGN.md`, which exists only on the unmerged branch `ccr-a1899a5f-disb85`. Where the two differ, the design wins. The differences are tracked in issue [#6](https://github.com/DominicBurkart/inchworm/issues/6):

- **D1, not met**: a directory is configured here by a global `[[repo]]` list plus `.inchworm/flows.toml`, not by a per-directory `inchworm.toml`; there is no `register`, `unregister` or `list`.
- **D2, not met**: there are no offramps (`noop`, `issue`, `escalate`).
- **D3, not met**: there are no per-step auditors; the model has judges after a run.
- **D4, partial**: only the issue-to-draft-PR journey exists. The tagged `trigger` is the extension point for the CI-failure and merge-conflict journeys; any other `kind` is rejected with an "unknown variant" error.
- **D5, not addressed**: the marigold/Rust control-flow split and the fate of the prototype branch are undecided.
- **D6, not met**: `DESIGN.md` is not on `main`.

Everything below describes what this code does today.

## Merging has no default

A flow must state its outcome: `outcome` and `outcome.mode` are required and have no default. `merge_when` exists as a mode and is validated, but no shipped example uses it, and `inchworm validate` warns when a flow turns it on. The intended direction is that merging becomes a capability a user grants to a nanna subagent or another agent, with the user defining when and how (nanna-coder issue 1043). Do not enable `merge_when` in a real repository.

## Commands

```
inchworm validate [--config PATH]
```

Exit codes:

- `0`: everything validated
- `1`: at least one error was found, either in a repository or in the global config (for example a missing or non-executable `nanna_bin`, reported with scope `[config]`); all errors are listed on stdout
- `2`: the command could not run: bad usage, no config could be found, or the config could not be read or parsed (the reason is on stderr)

`validate` is offline. It does not talk to GitHub, nanna or a model. It needs `HOME` only when the config uses `~`, and the default config location needs `XDG_CONFIG_HOME` or `HOME`.

For the global config it checks that `nanna_bin` is an executable file or, for a bare name, that it is on `PATH`. For every repository it checks:

- the path is a git repository (a `.git` file or symlink must lead to a gitdir that exists)
- `.inchworm/flows.toml` passes the flow rules below
- every `llm` judge rubric exists and is a regular file inside `.inchworm/`
- `.nanna/protected.toml` follows nanna's entry rules and lists `.inchworm/**`. Its only key is `extra`, for example `extra = [".inchworm/**", "scripts/deploy.sh"]`; any other key (such as `entries`) is rejected as an unknown key
- `.nanna/prompt.md` exists (a warning only)

The repo-controlled files above (`.inchworm`, `.nanna` and everything read from them) are read only if no component of the path under the repository root is a symlink at the time of the check, each is a regular file, and each is read with a size cap: 256 KiB for `flows.toml`, 64 KiB for `protected.toml`, 4 KiB for a `.git` file. The symlink check is a walk of the path before the read, and only the final component is opened without following symlinks, so a directory swapped for a symlink between the walk and the read is not prevented. The global config may itself be a symlink (for example from home-manager), but must resolve to a regular file of at most 256 KiB.

What a passing `validate` proves: the files inchworm reads are well formed, `.inchworm/**` is declared protected, and nothing it reads escapes the repository through a symlink. The `protected.toml` check mirrors `validate_extra` in nanna-coder `harness/src/protected.rs` as of nanna-coder commit `239a1c94`; it does not run nanna's code, so a later change there is not tracked. It does not prove that identities named in flows exist, that labels exist, or that branch protection matches `required_checks`; those online checks are not built yet.

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

- `nanna_bin` and each repo `path` may start with `~`, which needs `HOME` set to an absolute path. After expansion every repo path must be absolute. A path to a binary must exist: the example above resolves `nanna` on `PATH` and fails `validate` if it is not there.
- Defaults: `ollama_url = "http://localhost:11434"`, `concurrency = 1` (1 to 64), `poll_secs = 120` (1 to 86400). `state_db` is optional and, when unset, defaults to `~/.local/state/inchworm/state.db`; `validate` never opens or reads that file, but it does reject a `state_db` that is empty, contains a NUL byte, or is neither absolute nor starting with `~/` (exit 2, with the reason on stderr).
- Repo slugs are `owner/name` (case-insensitively unique), and no two repos may share a path.
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

- Required: `name`, `trigger`, at least one `step`, and `outcome` with its `mode`. Defaults: `base = "main"`, `max_attempts = 3`, `attempt_timeout_secs = 7200`. `retry_on` defaults to **empty**, so nothing is retried unless you list failure classes, even though `max_attempts` defaults to 3. `judge` defaults to none and `outcome.merge_method` to `merge`.
- Names and labels: a flow name is 1 to 64 characters of letters, digits, `.`, `_` or `-`, not starting with `.`, unique ignoring case. Two flows may not share a trigger label (ignoring case).
- Bounds, counted in characters, not bytes: a file holds at most 32 flows; a trigger label is at most 50 characters; a flow has at most 20 steps and at most 20 judges; a step name is at most 64 characters; a model is at most 100 characters; a rubric path is at most 200 characters; a `forbid_paths` glob is at most 200 characters and a diff judge has at most 256 of them; a `required_checks` entry is at most 200 characters and `outcome` has at most 64 of them. Step names, trigger labels, rubric paths, models, base branches, `forbid_paths` entries and `required_checks` entries may not contain control characters (newline, tab and NUL included).
- A rubric must be a relative path under `.inchworm/` (no `..`, no absolute path), so agents, which cannot write `.inchworm/**`, cannot change what judges read.
- `retry_on` may not repeat a class or list `cancelled` or `escalated`, and needs `max_attempts` of at least 2. `max_attempts` is 1 to 20 and `attempt_timeout_secs` is 1 to 604800.
- `mode = "merge_when"` also requires at least one judge and a non-empty `required_checks`. See "Merging has no default" above.
- Unknown keys are rejected everywhere.

## State store

`inchworm::store` is a SQLite store of runs and attempts. No command uses it yet. Runs are keyed by a subject (an issue number, a pull request number or a full commit id) and a flow; only one open run per subject and flow can exist, and a finished subject can run again. Several processes can use the same file. After a crash, `Store::recover_after_crash` needs a `DaemonLock` for the same database, fails attempts the dead daemon left dispatched as `orchestrator_lost`, puts `running` runs back in the queue (or in `awaiting_ci` when a pull request was already recorded) and reports the runs in `awaiting_ci` and `judging` for the daemon to resume. The `DaemonLock` is two exclusive `flock`s: one on the database file itself (identified by device and inode, so symlinks, hard links and a dangling symlink filled in later all lead to one lock) and one on a side file `.<database name>.inchworm-lock` next to it (which keeps excluding a second daemon when the database is unlinked or another file is renamed over it). It refuses in-memory names and every `file:` URI (also one that is not valid UTF-8; `Store::open` refuses them before creating a file), a database file name longer than 240 bytes (its side file name would pass the 255-byte limit) and a name that starts with `.` and ends with `.inchworm-lock` (it would collide with another database's side file). If the database path stops naming the locked file while the lock is held (unlinked, replaced by a restored backup), the lock no longer protects anything and `recover_after_crash` refuses it with `DatabaseMoved`; restart the daemon. Acquire the lock **before** opening any `Store` in the process and keep it for the process lifetime: the lock holds a descriptor on the database file, and on POSIX closing any descriptor of a file releases the process's `fcntl` locks on it, which SQLite uses, so dropping a `DaemonLock` while a `Store` in the same process is mid-transaction can release SQLite's locks. Nothing enforces that order. The lock only protects against processes that also take it. A database created by an earlier unreleased build, or one whose tables, indexes, triggers or views differ from the frozen schema in any way, fails to open with a clear error; delete it.

## Development

The pre-commit hook runs fmt, clippy, tests, doctests, audit, deny, docs, a security review of the staged diff and a coverage no-regression check against the merge base with `origin/main` (or `INCHWORM_COVERAGE_BASE`). The review must start with exactly `VERDICT: SAFE` on its first line and contain no `UNSAFE` verdict, NUL byte or standalone "not safe" phrase (`scripts/review-verdict.sh`). This is a tripwire, not a classifier: it only catches a review that contradicts itself in those exact words. Wording such as "not safely wrapped", "cannot be considered safe", "risky" or "vulnerable" still passes after a `VERDICT: SAFE` first line, so a human still reads the review. `cargo-husky` installs the hook on `cargo test`; in a git worktree it cannot, so copy `.cargo-husky/hooks/pre-commit` into the repository's hooks directory by hand.

The coverage gate compares line coverage with the merge base with `origin/main` (or `INCHWORM_COVERAGE_BASE`, or the pull request's base commit in CI) and fails if it dropped. While `main` is a stub (about 50% coverage) it is trivially satisfied. It is a regression check, not a quality bar.

The toolchain is pinned to the exact version in `rust-toolchain.toml` (1.97.0, with rustfmt and clippy), and CI installs that same version. A test (`tests/toolchain_pin.rs`) fails if the file names a moving channel, if `ci.yml` installs a different version, or if the toolchain running the tests is not the pinned one. Bump the version in both files together. Run the tests with the pinned toolchain: any other (`cargo +nightly test`, or a newer stable) makes `the_pinned_toolchain_is_the_one_running_the_tests` fail on purpose. Pinning exists because the standard library's error wording changed between releases and made a test pass locally and fail in CI.

CI runs fmt, clippy, tests, doctests, audit, deny, the docs build and the coverage comparison with the pull request's base commit; it does not run the security review, which is local only. The coverage gate is a regression check against the merge base with `origin/main`, not a quality bar, and it is trivially satisfied while `main` is a stub.

## License

MIT OR Apache-2.0
