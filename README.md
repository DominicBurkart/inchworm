# inchworm

A daemon and CLI for configuring fallible, repeated agentic processes on local models. inchworm schedules and sequences work per repository and reports success rates and specific failure conditions. It runs agents through [nanna](https://github.com/DominicBurkart/nanna-coder), which sets up sub-agents with fixed capabilities and runs them in per-repo containers.

## Status: partial, and not yet aligned with the project design

Milestones 1 and 2 of the MVP: configuration, run state, the SQLite store, offline and online validation, and dispatch of a flow to nanna through `inchworm run`. CI waiting, judges, the outcome policy, the daemon and stats are not implemented yet.

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

## Platforms

Linux and macOS. CI builds and tests on Ubuntu only; macOS is expected to work but is not tested. Windows is not supported: the crate refuses to compile there (`compile_error!`), so the `#[cfg(unix)]` guards in the tests are always true on supported targets (nanna itself needs podman and nix, and the tests need a POSIX shell, `kill` and, on Linux, `/proc`).

## What nanna does that inchworm has to live with

Each item says whether it was observed by running the real `nanna mcp-serve` in a sandbox (`tests/real_nanna.rs`, run with `--ignored`) or only read from nanna's source:

- **Nobody is notified of escalations by default** (observed). Unless `NANNA_ESCALATION_REPO` (with a token) or a webhook is configured, nanna only records an escalation in a local `escalations.jsonl` and warns at start-up that no human-facing sink exists. Running inchworm without a sink means a human only finds out by reading that file.
- **A corrupt `escalations.jsonl` stops nanna from starting** (observed). It parses the log at start-up and refuses to run on a malformed record (`missing field` errors), so nanna fails closed there.
- **The installed build emits no `failure_kind`** (observed for refusals, which carry only a message; for failed task results it is inferred from the build predating nanna-coder #1030, because no task was run to completion). Failed results then carry only the `error_type` label inside their text, so `error_type` labels (and, last, text heuristics) are the live classification path. The `failure_kind` path comes from nanna-coder #1030 and is tested only against reconstructed fixtures (see `tests/fixtures/nanna/README.md`).
- **Logs can reach stdout** (observed). With `RUST_LOG` set, nanna writes ANSI tracing lines to stdout between its JSON-RPC replies. inchworm removes `RUST_LOG` from the child's environment and skips (and counts) any non-JSON line.
- **State files.** Where the files land is observed: with `NANNA_QUEUE_PATH` set, `audit.jsonl` and the queue go to that directory and the lease, escalation and budget files go where their own variables point. That nanna restores its queue on start and re-runs unfinished tasks under the same task id, and that the lease, escalation, budget, audit and telemetry files default to siblings of the queue file, is read from nanna's source and **not observed**, because proving it needs a task that actually runs (a model and a container). inchworm therefore gives every spawn a uniquely named queue file in `<state_db dir>/nanna` (never reused, removed on exit) and pins the lease, escalation and budget files to nanna's shared defaults, so spend caps, leases and the escalation trail are shared with your other nanna sessions and survive restarts.
- **`mcp-serve` needs an `auditor` card** in the catalog (`NANNA_CONFIG_DIR/agents`) or it exits, and it always talks to Ollama at `localhost:11434` (both from nanna's source, not observed); inchworm's `ollama_url` applies only to inchworm's own calls.

## Commands

```
inchworm validate [--config PATH] [--online] [--build-images]
inchworm run <repo-slug-or-path> <issue> [--flow NAME] [--config PATH]
```

Exit codes:

- `0`: everything validated
- `1`: at least one error was found, either in a repository or in the global config (for example a missing or non-executable `nanna_bin`, reported with scope `[config]`); all errors are listed on stdout
- `2`: the command could not run: bad usage, no config could be found, or the config could not be read or parsed (the reason is on stderr)

`validate` is offline unless `--online` or `--build-images` is given (below). Offline, it does not talk to GitHub, nanna or a model. It needs `HOME` only when the config uses `~`, and the default config location needs `XDG_CONFIG_HOME` or `HOME`.

For the global config it checks that `nanna_bin` is an executable file or, for a bare name, that it is on `PATH`. For every repository it checks:

- the path is a git repository (a `.git` file or symlink must lead to a gitdir that exists)
- `.inchworm/flows.toml` passes the flow rules below
- every `llm` judge rubric exists and is a regular file inside `.inchworm/`
- `.nanna/protected.toml` follows nanna's entry rules and lists `.inchworm/**`. Its only key is `extra`, for example `extra = [".inchworm/**", "scripts/deploy.sh"]`; any other key (such as `entries`) is rejected as an unknown key
- `.nanna/prompt.md` exists (a warning only)

The repo-controlled files above (`.inchworm`, `.nanna` and everything read from them) are read only if no component of the path under the repository root is a symlink at the time of the check, each is a regular file, and each is read with a size cap: 256 KiB for `flows.toml`, 64 KiB for `protected.toml`, 4 KiB for a `.git` file. The symlink check is a walk of the path before the read, and only the final component is opened without following symlinks, so a directory swapped for a symlink between the walk and the read is not prevented. The global config may itself be a symlink (for example from home-manager), but must resolve to a regular file of at most 256 KiB.

What a passing `validate` proves: the files inchworm reads are well formed, `.inchworm/**` is declared protected, and nothing it reads escapes the repository through a symlink. The `protected.toml` check mirrors `validate_extra` in nanna-coder `harness/src/protected.rs` as of nanna-coder commit `239a1c94`; it does not run nanna's code, so a later change there is not tracked. It does not prove that identities named in flows exist, that labels exist, or that branch protection matches `required_checks`; those online checks are not built yet.

With `--online` it also checks that nanna has an `auditor` identity (`mcp-serve` refuses to start without one), warns when `ollama_url` is not the address nanna itself always uses (`http://localhost:11434`), checks that every step identity resolves with `nanna agents list --repo <path>` and that the trigger label and the `inchworm:running`, `inchworm:failed` and `inchworm:done` labels exist on GitHub (through `gh`). `--build-images` implies `--online` and runs `nix build git+file://<repo>#devContainerImage --no-link --no-write-lock-file` for each repo.

**`--build-images` runs the repository's own Nix code on this machine.** Evaluating a flake executes the Nix expressions in `flake.nix` and everything it imports, and builds may fetch from the network. That is the one place inchworm itself runs repo-controlled code on the host, so use the flag only on repositories you trust. It reads tracked files only (`git+file`), so untracked files such as `.env` are not copied into the world-readable Nix store, and it never rewrites the repo's `flake.lock`. Nanna builds the same image for real runs; this flag only checks it early.

`run` executes one flow for one issue in the foreground. Before it creates a run it checks that the issue is open, carries the flow's trigger label, was written by the repository's owner, a member or a collaborator (the issue text is handed to an agent, so text from anyone else is refused), and that no pull request from an earlier attempt is still open on `inchworm/<issue>-a<n>`; otherwise it exits `2`: no run is recorded, nothing is written to GitHub and nanna is not started (the SQLite file `state.db` and its directory are still created when they do not exist). A pull request that is not a draft is rejected as `agent_failed`. Pressing Ctrl-C or sending SIGTERM cancels the running task and ends the run in state `failed` with failure class `cancelled`; a hard kill (SIGKILL, power loss) cannot be handled and leaves the run `running`. It starts `nanna mcp-serve`, sends each step as an `assign_task` to the step's identity (passing the previous step's summary and patch forward), stops at the first failing step, classifies the failure into a `FailureClass`, retries per `retry_on` and `max_attempts`, and records everything in the SQLite store. After the final step it looks up the pull request on `inchworm/<issue>-a<attempt>` with `gh`, records its head SHA, and leaves the run in `awaiting_ci`. A step output (or pull request) that touches `.inchworm/**` or an `inchworm.toml` at any depth is rejected as `protected_path_violation` regardless of nanna's own protected set. A step other than the last (or the only step) that produces no changes fails as `empty_patch`. Exit codes: `0` a draft pull request was found and recorded, `1` the run failed, `2` setup error.

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

### How steps hand work to each other

Each step is its own nanna task, and nanna creates a fresh worktree from `base` for every task. A later step therefore does not see an earlier step's files. It receives the earlier step's summary, file list and patch as text in its description. The patch is handed over whole or not at all: if a step other than the last produces a patch larger than 100,000 bytes, the flow stops with `patch_apply_failed` and says so; nothing is truncated.

Inchworm itself never applies, commits or pushes anything, and **publishing a pull request is not supported yet, so with nanna as it is a successful `run` (state `awaiting_ci`) is not reachable: nothing can create the pull request it looks for.** `run` looks for a draft pull request on `inchworm/<issue>-a<attempt>` after the last step, but nothing in this repository makes one exist, and with nanna as it is today no step can: nanna creates each task's worktree with `git worktree add <path> <branch>` (no `-b`), so the base branch (default `main`) is either already checked out or gives a detached HEAD; its `git_commit` and `git_push_branch` tools refuse a detached HEAD and `main`/`master`; the `pr-shepherd` card has no branch-creating tool; and a step that does not commit hands the next step only patch text. A flow whose steps finish without such a pull request ends the attempt as `agent_failed` ("no pull request exists") and is not retried, because replaying the steps cannot create one. This is a flaw in the flow model, not something to work around by weakening nanna's protections; it needs nanna to create the per-attempt branch and carry one step's changes to the next, and is an input for the reconciliation work. Everything about the hand-off in this section is therefore design intent, not tested behaviour; what is verified is the wire protocol with the real binary (`tests/real_nanna.rs`) and the flow logic against a fake nanna.

## Development

The ten tests in `tests/real_nanna.rs` are `#[ignore]` and are **not run by CI**; run them by hand with `cargo test --test real_nanna -- --ignored` (needs `nanna` installed or `INCHWORM_NANNA_BIN`).

The pre-commit hook runs fmt, clippy, tests, doctests, audit, deny, docs, a security review of the staged diff and a coverage no-regression check against the merge base with `origin/main` (or `INCHWORM_COVERAGE_BASE`). The review must start with exactly `VERDICT: SAFE` on its first line and contain no `UNSAFE` verdict, NUL byte or standalone "not safe" phrase (`scripts/review-verdict.sh`). This is a tripwire, not a classifier: it only catches a review that contradicts itself in those exact words. Wording such as "not safely wrapped", "cannot be considered safe", "risky" or "vulnerable" still passes after a `VERDICT: SAFE` first line, so a human still reads the review. `cargo-husky` installs the hook on `cargo test`; in a git worktree it cannot, so copy `.cargo-husky/hooks/pre-commit` into the repository's hooks directory by hand.

The coverage gate compares line coverage with the merge base with `origin/main` (or `INCHWORM_COVERAGE_BASE`, or the pull request's base commit in CI) and fails if it dropped. While `main` is a stub (about 50% coverage) it is trivially satisfied. It is a regression check, not a quality bar.

The toolchain is pinned to the exact version in `rust-toolchain.toml` (1.97.0, with rustfmt and clippy), and CI installs that same version. A test (`tests/toolchain_pin.rs`) fails if the file names a moving channel, if `ci.yml` installs a different version, or if the toolchain running the tests is not the pinned one. Bump the version in both files together. Run the tests with the pinned toolchain: any other (`cargo +nightly test`, or a newer stable) makes `the_pinned_toolchain_is_the_one_running_the_tests` fail on purpose. Pinning exists because the standard library's error wording changed between releases and made a test pass locally and fail in CI.

CI runs fmt, clippy, tests, doctests, audit, deny, the docs build and the coverage comparison with the pull request's base commit; it does not run the security review, which is local only. The coverage gate is a regression check against the merge base with `origin/main`, not a quality bar, and it is trivially satisfied while `main` is a stub.

## License

MIT OR Apache-2.0
