# inchworm

A daemon and CLI for configuring fallible, repeated agentic processes on local models. inchworm schedules and sequences work per repository and reports success and failure rates and the specific failure conditions. It runs agents through [nanna](https://github.com/DominicBurkart/nanna-coder), which sets up sub-agents with fixed capabilities and runs them in per-repo containers.

## Status

Milestones 1 and 2 of the MVP: configuration, run state, the SQLite store, offline and online validation, and dispatch of a flow to nanna through `inchworm run`. CI waiting, judges, the outcome policy, the daemon and stats are not implemented yet.

## Platforms

Linux and macOS. CI builds and tests on Ubuntu only; macOS is expected to work but is not tested. Windows is not supported and the crate refuses to compile there (nanna itself needs podman and nix, and the tests need a POSIX shell, `kill` and, on Linux, `/proc`).

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

Exit codes: `0` valid, `1` the repos have errors, `2` the config could not be read.

`validate` checks, for every repo in the global config:

- the path is a git repository
- `.inchworm/flows.toml` parses and passes its rules
- every rubric file referenced by an `llm` judge exists
- `.nanna/protected.toml` lists `.inchworm/**` so agents cannot edit their own flows or judges
- `.nanna/prompt.md` exists (warning only)

With `--online` it also checks that nanna has an `auditor` identity (`mcp-serve` refuses to start without one), warns when `ollama_url` is not the address nanna itself always uses (`http://localhost:11434`), checks that every step identity resolves with `nanna agents list --repo <path>` and that the trigger label and the `inchworm:running`, `inchworm:failed` and `inchworm:done` labels exist on GitHub (through `gh`). `--build-images` implies `--online` and runs `nix build git+file://<repo>#devContainerImage --no-link --no-write-lock-file` for each repo.

**`--build-images` runs the repository's own Nix code on this machine.** Evaluating a flake executes the Nix expressions in `flake.nix` and everything it imports, and builds may fetch from the network. That is the one place inchworm itself runs repo-controlled code on the host, so use the flag only on repositories you trust. It reads tracked files only (`git+file`), so untracked files such as `.env` are not copied into the world-readable Nix store, and it never rewrites the repo's `flake.lock`. Nanna builds the same image for real runs; this flag only checks it early.

`run` executes one flow for one issue in the foreground. Before it creates a run it checks that the issue is open, carries the flow's trigger label, was written by the repository's owner, a member or a collaborator (the issue text is handed to an agent, so text from anyone else is refused), and that no pull request from an earlier attempt is still open on `inchworm/<issue>-a<n>`; otherwise it exits `2` and changes nothing. A pull request that is not a draft is rejected as `agent_failed`. Pressing Ctrl-C or sending SIGTERM cancels the running task and ends the run as `cancelled`; a hard kill (SIGKILL, power loss) cannot be handled and leaves the run `running`. It starts `nanna mcp-serve`, sends each step as an `assign_task` to the step's identity (passing the previous step's summary and patch forward), stops at the first failing step, classifies the failure into a `FailureClass`, retries per `retry_on` and `max_attempts`, and records everything in the SQLite store. After the final step it looks up the pull request on `inchworm/<issue>-a<attempt>` with `gh`, records its head SHA, and leaves the run in `awaiting_ci`. A step output that touches `.inchworm/**` is rejected as `protected_path_violation` regardless of nanna's own protected set. A step other than the last (or the only step) that produces no changes fails as `empty_patch`. Exit codes: `0` PR recorded, `1` the run failed, `2` setup error.

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

### How steps hand work to each other

Each step is its own nanna task, and nanna creates a fresh worktree from `base` for every task. A later step therefore does not see an earlier step's files. It receives the earlier step's summary, file list and patch as text in its description. The patch is handed over whole or not at all: if a step other than the last produces a patch larger than 100,000 bytes, the flow stops with `patch_apply_failed` and says so; nothing is truncated.

Inchworm itself never applies, commits or pushes anything. The publish step (for example `pr-shepherd`) has to apply the patch it was given and push `inchworm/<issue>-a<attempt>` with nanna's own tools, and inchworm then looks that branch up with `gh`. **That hand-over has not been run end to end against a real nanna, a model and a container**; what is verified is the wire protocol with the real binary (see `tests/real_nanna.rs`) and the flow logic against a fake nanna. Treat the first real run as an experiment.

## Development

The pre-commit hook (installed by `cargo-husky` on `cargo test`, or copy `.cargo-husky/hooks/pre-commit` into the git hooks directory when working in a worktree) runs fmt, clippy, tests, doctests, audit, deny, docs, a security review of the staged diff and a coverage no-regression check against `main`.

## License

MIT OR Apache-2.0
