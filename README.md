# inchworm

A daemon and CLI for configuring fallible, repeated agentic processes on local models. inchworm schedules and sequences work per repository and reports success rates and specific failure conditions. It runs agents through [nanna](https://github.com/DominicBurkart/nanna-coder), which sets up sub-agents with fixed capabilities and runs them in per-repo containers.

## Status: partial, and not yet aligned with the project design

Milestones 1 to 4 of the MVP: configuration, run state, the SQLite store, offline and online validation, dispatch of a flow to nanna, CI waiting on the exact head SHA, judges, the outcome policy, the polling daemon and the stats reports. It is functionally complete behind fakes: apart from the `#[ignore]` tests that need a real nanna, nothing in the test suite talks to a real nanna, GitHub or Ollama, so run `inchworm validate --online` and one foreground `inchworm run` on a throwaway issue before trusting the daemon. Offramps and auditors are configuration only: nothing runs an auditor or an offramp. Publishing a pull request is not supported by nanna yet, so a run only reaches `pr_open` when a pull request already exists on the attempt branch (see "How steps hand work to each other").

It was written from a planning conversation, not from the project's design. The design is issue [#1](https://github.com/DominicBurkart/inchworm/issues/1) and `DESIGN.md`, which is now on `main`. Where the two differ, the design wins. The differences are tracked in issue [#6](https://github.com/DominicBurkart/inchworm/issues/6):

- **D1, met except `register`/`unregister`**: a directory is configured by `inchworm.toml` at its root, and `inchworm list` prints the registered directories (one `slug<TAB>path` line each; it refuses a slug or path with a control character, and exits `2` if it cannot write). A directory is registered by adding a `[[repo]]` entry to the global config by hand; `register` and `unregister` are a documented non-goal for now because editing the config without losing its comments needs a TOML-editing dependency. The old `.inchworm/flows.toml` is no longer read: `validate` reports that it moved.
- **D2, configuration only**: `[offramp]` accepts `noop`, `issue` and `escalate` with a default and per-flow values, validated, but nothing executes an offramp yet.
- **D3, configuration only**: `[auditor]` is mandatory, says where the auditor runs and fails closed, but nothing runs an auditor yet. The judges after a run remain.
- **D4, partial**: only the issue-to-draft-PR journey exists. The tagged `trigger` is the extension point for the CI-failure and merge-conflict journeys; any other `kind` is rejected with an "unknown variant" error.
- **D5, not addressed**: the marigold/Rust control-flow split and the fate of the prototype branch are undecided.
- **D6, met**: `DESIGN.md` is on `main` with a table mapping each concept to where it is implemented.

Everything below describes what this code does today.

## Merging has no default

A flow must state its outcome: `outcome` and `outcome.mode` are required and have no default. `merge_when` exists as a mode and is validated, but no shipped example uses it, and `inchworm validate` warns when a flow turns it on. The intended direction is that merging becomes a capability a user grants to a nanna subagent or another agent, with the user defining when and how (nanna-coder issue 1043). Whether merging is possible at all is one switch, `[merge] allow` in `inchworm.toml`, which is `false` unless set. With it `false` a flow in `merge_when` mode is a configuration error, not a warning. `MergeConfig::grant` maps it to `MergeGrantSetting::Denied` or `Granted`, the one place the later policy code reads. Do not enable `merge_when` in a real repository. `inchworm run` and `inchworm daemon` pass the project's `[merge] allow` as the grant, so a `merge_when` flow can only merge in a project that sets `allow = true`; with it unset (the default) such a flow is a configuration error and nothing merges.


## Install

```
cargo install --path .
install -d -m 700 ~/.config/inchworm ~/.local/state/inchworm
$EDITOR ~/.config/inchworm/config.toml
```

Example `~/.config/inchworm/config.toml`:

```toml
nanna_bin = "~/.local/bin/nanna"
ollama_url = "http://localhost:11434"
concurrency = 1
poll_secs = 120
state_db = "~/.local/state/inchworm/state.db"

[[repo]]
slug = "DominicBurkart/marigold"
path = "~/code/marigold"
```

inchworm only needs the `nanna` executable and `gh` (logged in with access to the repos) on the machine. nanna reads its identities from `NANNA_CONFIG_DIR` (for example `~/.config/nanna`, holding the global `agents/` catalog) and builds its container images through `skopeo`. The host skopeo lacks the nix transport, so a skopeo built with the nix transport must come first in `PATH` (for example the one from your Nix profile):

```
export NANNA_CONFIG_DIR=~/.config/nanna
export PATH="$(dirname "$(readlink -f ~/.nix-profile/bin/skopeo)"):$PATH"
```

Create the labels in every configured repo (the trigger label plus the three status labels):

```
for repo in DominicBurkart/marigold; do
  gh label create inchworm -R "$repo" --color 5319e7 --description "inchworm picks this issue up"
  gh label create inchworm:running -R "$repo" --color fbca04 --description "inchworm is working on this"
  gh label create inchworm:failed -R "$repo" --color d73a4a --description "inchworm gave up"
  gh label create inchworm:done -R "$repo" --color 0e8a16 --description "inchworm opened a judged draft PR"
done
```

Every configured repo needs three files before the daemon can work on it (`inchworm validate` checks them):

- `inchworm.toml` at the repository root: the flows, an `[auditor]`, and optionally `[offramp]` and `[merge]` (see Project file and Flows below)
- `.nanna/protected.toml` containing `extra = ["inchworm.toml", ".inchworm/**"]`, so agents cannot edit their own configuration or judges
- `.nanna/prompt.md`, the prompt nanna gives its agents (`validate` only warns when it is missing)

Check everything before the first run:

```
inchworm validate --online
```

Run it as a systemd user service:

```
mkdir -p ~/.config/systemd/user
cp contrib/inchworm.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now inchworm
journalctl --user -u inchworm -f
```

The unit sets `PATH` and `NANNA_CONFIG_DIR`; edit both for your machine. Put `GH_TOKEN` (if you do not rely on `gh auth login`) in `~/.config/inchworm/env`, mode 600, which the unit loads with `EnvironmentFile`; never put it in `Environment=`. The unit restarts on failure every 15 seconds and gives up after 5 starts within 10 minutes (`StartLimitBurst`), so a transient fault recovers while a persistent config error does not loop forever; `systemctl --user reset-failed inchworm` clears the limit. The unit deliberately sets only `NoNewPrivileges`, `RestrictSUIDSGID` and `UMask`: `PrivateTmp`, `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectControlGroups` and `LockPersonality` each make `podman unshare` fail under a user service (`newuidmap: write to uid_map failed`), which breaks rootless podman and with it nanna's containers. The state directory is set to mode 700 and the database to mode 600 on every start, and a lock file next to the database (`.state.db.inchworm-lock`) makes a second daemon on the same database refuse to start. To keep it running while logged out: `loginctl enable-linger $USER`.

## Operational notes

- `inchworm daemon` refuses to start when `nanna_bin` is not an executable, and refuses to start while another `inchworm daemon` or a foreground `inchworm run` holds the same state database (one `DaemonLock`, see State store); `inchworm run` refuses while a daemon holds it.
- Stopping the daemon waits up to 20 seconds for the in-flight run; a run still in flight is marked `orchestrator_lost` on the next start and counts as one attempt. This is a known limitation: there is no cooperative cancel of a dispatched nanna task on shutdown.
- Every `gh` call is bounded by 60 seconds and runs in its own process group that is killed with it and the poll cycle runs on its own thread, so SIGTERM stops the daemon even while `gh` hangs. The SIGTERM and SIGINT handlers are installed before the daemon starts.
- A failed dispatch drops the nanna client and the next attempt starts a fresh `nanna mcp-serve`.
- A foreground `inchworm run` that is killed while waiting for CI leaves an open run in the store; only `inchworm daemon` resumes it, on its next start.
- `GH_TOKEN` from the unit's `EnvironmentFile` is inherited by `nanna mcp-serve` as well as by `gh`. Whether nanna forwards it into its containers is unverified; give the token only the scopes the flows need.
- Re-adding the trigger label to an issue whose run finished (`pr_open`, `merged` or `failed`) starts a new run on purpose, once 60 seconds have passed since that run finished (a poll that lands between a run's state write and its label swap does not queue a second run); remove the label when the work is done.
- If the initial label swap on an issue fails, the daemon remembers that only in memory; after a restart a stale trigger label next to a finished run can start another run until the label is removed by hand. Known limitation.
- Text from nanna, `gh` and judges has control characters and bidirectional override characters replaced and is capped before it is printed by `failures`, `run` or the daemon log, or posted to an issue.
- Branch protection is compared by required check name only. A passing `validate --online` does not check `enforce_admins`, required reviews or app ids, so it says little about merge safety on its own.
- Skipped, neutral, cancelled and unknown check conclusions are treated as red and fail the attempt as `ci_red`, even where GitHub branch protection accepts a skipped job (a path-filtered or conditional job). This holds for every blocking form below, including `blocking = "all"`: a skipped job blocks. Do not list such a job in `required_checks`, and expect `blocking = "all"` to wait forever on a repository with conditional jobs; there is no opt-in for treating `skipped` or `neutral` as green.

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
inchworm validate [--config PATH] [--online] [--build-images] [--skip-branch-protection]
inchworm run <repo-slug-or-path> <issue> [--flow NAME] [--config PATH]
inchworm daemon [--config PATH]
inchworm status [--config PATH]
inchworm stats [--window 24h|7d|30d|all] [--config PATH]
inchworm failures [--class CLASS] [--config PATH]
inchworm list [--config PATH]
```

`daemon` polls every registered repository and runs labelled issues until SIGTERM or SIGINT. `status` lists recent runs. `stats` prints runs, successes and success rate per repo, flow and window from SQL aggregates; a run succeeded if it ended `merged` or `pr_open`. `failures` prints the failure class histogram and the most recent raw errors, optionally for one class; `stats` and `failures` exit `2` on an unknown window or class.

`list` prints one line per registered directory, the slug and the path separated by a tab, and exits `0`, or `2` when the config cannot be found, read or parsed.

Exit codes:

- `0`: everything validated
- `1`: at least one error was found, either in a repository or in the global config (for example a missing or non-executable `nanna_bin`, reported with scope `[config]`); all errors are listed on stdout
- `2`: the command could not run: bad usage, no config could be found, or the config could not be read or parsed (the reason is on stderr)

`validate` is offline unless `--online` or `--build-images` is given (below). Offline, it does not talk to GitHub, nanna or a model. It needs `HOME` only when the config uses `~`, and the default config location needs `XDG_CONFIG_HOME` or `HOME`.

For the global config it checks that `nanna_bin` is an executable file or, for a bare name, that it is on `PATH`. For every repository it checks:

- the path is a git repository (a `.git` file or symlink must lead to a gitdir that exists)
- `inchworm.toml` at the repository root passes the rules below; if only the old `.inchworm/flows.toml` exists, that is an error saying it moved
- every `llm` judge rubric exists and is a regular file inside `.inchworm/`
- `.inchworm` is not a symlink
- `.nanna/protected.toml` follows nanna's entry rules and lists both `inchworm.toml` and `.inchworm/**`, so agents can change neither the configuration nor the rubrics. Its only key is `extra`, for example `extra = ["inchworm.toml", ".inchworm/**", "scripts/deploy.sh"]`; any other key (such as `entries`) is rejected as an unknown key
- `.nanna/prompt.md` exists (a warning only)

The repo-controlled files above (`.inchworm`, `.nanna` and everything read from them) are read only if no component of the path under the repository root is a symlink at the time of the check, each is a regular file, and each is read with a size cap: 256 KiB for `inchworm.toml`, 64 KiB for `protected.toml`, 4 KiB for a `.git` file. The symlink check is a walk of the path before the read, and only the final component is opened without following symlinks, so a directory swapped for a symlink between the walk and the read is not prevented. The global config may itself be a symlink (for example from home-manager), but must resolve to a regular file of at most 256 KiB.

What a passing `validate` proves: the files inchworm reads are well formed, `inchworm.toml` and `.inchworm/**` are declared protected, and nothing it reads escapes the repository through a symlink. The `protected.toml` check mirrors `validate_extra` in nanna-coder `harness/src/protected.rs` as of nanna-coder commit `239a1c94`; it does not run nanna's code, so a later change there is not tracked. Offline, it does not prove that identities named in flows exist or that labels exist (`--online` checks both), and only `validate --online` compares branch protection with the flow's blocking checks.

With `--online` it also checks that nanna has an `auditor` identity (`mcp-serve` refuses to start without one), warns when `ollama_url` is not the address nanna itself always uses (`http://localhost:11434`), checks that every step identity resolves with `nanna agents list --repo <path>` and that the trigger label and the `inchworm:running`, `inchworm:failed` and `inchworm:done` labels exist on GitHub (through `gh`). `--online` also compares each flow's `required_checks` with the required status checks of its base branch protection (`gh api repos/<slug>/branches/<base>/protection/required_status_checks`): a listed check the branch does not require is an error; an extra protected check or an unprotected base is a warning, and an error for `merge_when`; if the protection cannot be read because the repository is private on a plan without branch protection (HTTP 403 "Upgrade to GitHub Pro"), that is a warning for `draft_pr` flows and an error for `merge_when`; `--skip-branch-protection` skips the comparison. `--build-images` implies `--online` and runs `nix build git+file://<repo>#devContainerImage --no-link --no-write-lock-file` for each repo.

**`--build-images` runs the repository's own Nix code on this machine.** Evaluating a flake executes the Nix expressions in `flake.nix` and everything it imports, and builds may fetch from the network. That is the one place inchworm itself runs repo-controlled code on the host, so use the flag only on repositories you trust. It reads tracked files only (`git+file`), so untracked files such as `.env` are not copied into the world-readable Nix store, and it never rewrites the repo's `flake.lock`. Nanna builds the same image for real runs; this flag only checks it early.

`run` executes one flow for one issue in the foreground. Before it creates a run it checks that the issue is open, carries the flow's trigger label, was written by the repository's owner, a member or a collaborator (the issue text is handed to an agent, so text from anyone else is refused), and that no pull request from an earlier attempt is still open on `inchworm/<issue>-a<n>`; otherwise it exits `2`: no run is recorded, nothing is written to GitHub and nanna is not started (the SQLite file `state.db` and its directory are still created when they do not exist). A pull request that is not a draft is rejected as `agent_failed`. Pressing Ctrl-C or sending SIGTERM cancels the running task and ends the run in state `failed` with failure class `cancelled`; a hard kill (SIGKILL, power loss) cannot be handled and leaves the run `running`. It starts `nanna mcp-serve`, sends each step as an `assign_task` to the step's identity (passing the previous step's summary and patch forward), stops at the first failing step, classifies the failure into a `FailureClass`, retries per `retry_on` and `max_attempts`, and records everything in the SQLite store. After the final step it looks up the pull request on `inchworm/<issue>-a<attempt>` with `gh`, records its head SHA, and waits for CI on that head, runs the judges and applies the outcome (see "What a run does"). A step output (or pull request) that touches `.inchworm/**` or an `inchworm.toml` at any depth is rejected as `protected_path_violation` regardless of nanna's own protected set. A step other than the last (or the only step) that produces no changes fails as `empty_patch`. Exit codes: `0` the run ended `pr_open` or `merged`, `1` the run failed, `2` setup error. `run` does not read `.nanna/protected.toml`: only `validate` checks that `inchworm.toml` and `.inchworm/**` are protected, so run `validate` first. `run` does refuse a pull request that touches `inchworm.toml` or anything under `.inchworm/` at any depth (`flow::touches_protected`). With only the old `.inchworm/flows.toml` present, `run` says it has moved to `inchworm.toml`.

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

## Project file

`inchworm.toml` at the root of each registered directory holds `[[flow]]` tables (below) and three more: `[offramp]`, `[auditor]` and `[merge]`. A complete minimal file:

```toml
[auditor]
runs_in = { kind = "nanna", identity = "auditor" }

[[flow]]
name = "issue"
trigger = { kind = "label", label = "inchworm" }

[[flow.step]]
name = "implement"
identity = "rust-implementer"

[[flow.judge]]
kind = "nanna_clean"

[flow.outcome]
mode = "draft_pr"
```

### Offramps

`[offramp] default` is what happens when a flow falls off its path: `noop` (the default when the key is omitted), `issue` or `escalate`. `[offramp.flow]` overrides it per flow, by flow name; names are compared ignoring ASCII case when validating and when looking up, a name that is not a flow is an error, and two names that differ only in case are an error. No offramp executes yet. `issue` is meant to reuse nanna's escalation sink (an open question in `DESIGN.md`, not built). `escalate` would hand the work to a frontier agent; it is the only setting that can leave local models, so it must be named explicitly and `validate` warns on it. It is not implemented: nothing is handed anywhere.

### Auditors

`[auditor]` is required, and a file without it is an error: every agent step needs an auditor and there is no default. `runs_in` says where the auditor runs: `{ kind = "nanna", identity = "..." }` (a nanna sub-agent) or `{ kind = "host" }` (deterministic checks, no model). The host place is reserved and unusable: no host check exists, so `host` is a parse error, both as the project default (a default that cannot run would silently skip every auditor) and as a per-step override (the error names the flow and step). Every step must also have at least one check: a flow with no judges, and no named checks, is an error rather than an auditor that verifies nothing. `[[auditor.step]]` entries pick another place and a subset of the flow's judges (`checks = ["diff", "nanna_clean", "llm"]`) for one step of one flow; every other step uses the default and all of the flow's judges. In `[[auditor.step]]`, `checks = []` (or no `checks` key) means all of the flow's judges, not none; a step always has at least one check. A nanna auditor may not have the identity of the step it audits. There is no setting that skips an auditor. `on_failure` accepts only `block`: `AuditorConfig::decide` turns an auditor that errored into a halt of the step. Nothing runs an auditor yet. A model auditor is not a safety gate (nanna's own scorecard for it fails the zero-false-allow bar); safety rests on container isolation, the protected-path list and draft PRs.

### Protected paths

`validate::protected_path_touched` is the check inchworm applies to a patch itself. It refuses a path that is, or lies under, `inchworm.toml` or `.inchworm/` at the repository root, after this normalisation, and nothing else:

- handled: `./` and empty components, `..` segments (a path that climbs out of the repository is refused), `/` and `\` as separators, ASCII case, trailing dots and spaces on a component, and `:stream` suffixes on a component. Also refused outright: NUL, a leading `/` or `\`, and a drive letter prefix (`C:`).
- not handled: Windows 8.3 short names (`INCHWO~1.TOM`), other whitespace such as tab or no-break space, Unicode look-alike characters (confusables) and Unicode normalisation or case folding beyond ASCII, symlinks and hard links (the path is compared as text and no file system is read), and a path below another directory such as `docs/inchworm.toml` or `sub/.inchworm/x` (the policy covers the repository root; see issue #26).
- not done here: turning a diff into paths (rename and copy headers, quoted names) is the caller's job and is not done or tested.

It is stricter than a POSIX path comparison on purpose, because a blocklist errs on the safe side, but it is not a complete defence against every alias; the protected-path list in `.nanna/protected.toml` and draft pull requests are the other layers. Nothing calls it on a real patch yet.

## Flows

The `[[flow]]` tables of `inchworm.toml`:

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
blocking = "all"
```

- Required: `name`, `trigger`, at least one `step`, and `outcome` with its `mode`. Defaults: `base = "main"`, `max_attempts = 3`, `attempt_timeout_secs = 7200`. `retry_on` defaults to **empty**, so nothing is retried unless you list failure classes, even though `max_attempts` defaults to 3. `judge` defaults to none and `outcome.merge_method` to `merge`.
- Names and labels: a flow name is 1 to 64 characters of letters, digits, `.`, `_` or `-`, not starting with `.`, unique ignoring case. Two flows may not share a trigger label (ignoring case).
- Bounds, counted in characters, not bytes: a file holds at most 32 flows; a trigger label is at most 50 characters; a flow has at most 20 steps and at most 20 judges; a step name is at most 64 characters; a model is at most 100 characters; a rubric path is at most 200 characters; a `forbid_paths` glob is at most 200 characters and a diff judge has at most 256 of them; a `required_checks` entry is at most 200 characters and `outcome` has at most 64 of them. Step names, trigger labels, rubric paths, models, base branches, `forbid_paths` entries and `required_checks` entries may not contain control characters (newline, tab and NUL included).
- A rubric must be a relative path under `.inchworm/` (no `..`, no absolute path), so agents, which cannot write `.inchworm/**`, cannot change what judges read.
- `retry_on` may not repeat a class or list `cancelled` or `escalated`, and needs `max_attempts` of at least 2. `max_attempts` is 1 to 20 and `attempt_timeout_secs` is 1 to 604800.
- `mode = "merge_when"` also requires at least one judge and blocking checks (an explicit empty `required_checks = []` blocks nothing and is refused). See "Merging has no default" above.
- **Blocking checks** (`[flow.outcome]`): which CI checks must be green on the judged head SHA before the judges' outcome applies.
  - `blocking = "required"` (the default when nothing else is set): exactly the required status checks of the flow's base branch, read from GitHub when the decision is made. If the branch protection cannot be read, does not exist or lists no checks, the decision fails closed: it never merges, never treats the run as green, and the attempt ends as `blocking_unresolved`. Add `extra = ["name", ...]` to ratchet up by name: the blocking set is the required checks plus those names (never fewer than required).
  - `blocking = "all"`: every check run and every commit status reported on the judged SHA blocks. It is green only when at least one check exists, none is red or pending, every required name of the branch protection (when it can be read) is present, no check seen earlier has disappeared (that is `ci_red`), and no new check name has appeared for 60 seconds (`CiPolicy::settle`), because checks that have not started are invisible until GitHub reports them. A check that never appears within the 2 h limit fails as `timeout`. `extra` has no effect with `all` and is refused.
  - `required_checks = [...]` is the explicit exact list and keeps its meaning (an empty list blocks nothing). It cannot be combined with `blocking` or `extra` (a config error, not a precedence rule). It can be weaker than the branch protection; `validate --online` warns when it is.
  - For every form: only checks on the exact judged SHA count, a red run stays red even if a later run of the same name and SHA is green, and runs for other SHAs are ignored. `[merge] allow` remains the only grant for merging.
  - `validate --online` prints the effective blocking set per flow as a `note:` line and reports an error when `blocking = "required"` could not resolve (branch protection missing, empty or unreadable).
- Unknown keys are rejected everywhere.

## State store

`inchworm::store` is a SQLite store of runs and attempts. `inchworm run`, `inchworm daemon`, `status`, `stats` and `failures` use it. Runs are keyed by a subject (an issue number, a pull request number or a full commit id) and a flow; only one open run per subject and flow can exist, and a finished subject can run again. Several processes can use the same file. After a crash, `Store::recover_after_crash` needs a `DaemonLock` for the same database, fails attempts the dead daemon left dispatched as `orchestrator_lost`, puts `running` runs back in the queue (or in `awaiting_ci` when a pull request was already recorded) and reports the runs in `awaiting_ci` and `judging` for the daemon to resume. The `DaemonLock` is two exclusive `flock`s: one on the database file itself (identified by device and inode, so symlinks, hard links and a dangling symlink filled in later all lead to one lock) and one on a side file `.<database name>.inchworm-lock` next to it (which keeps excluding a second daemon when the database is unlinked or another file is renamed over it). It refuses in-memory names and every `file:` URI (also one that is not valid UTF-8; `Store::open` refuses them before creating a file), a database file name longer than 240 bytes (its side file name would pass the 255-byte limit) and a name that starts with `.` and ends with `.inchworm-lock` (it would collide with another database's side file). If the database path stops naming the locked file while the lock is held (unlinked, replaced by a restored backup), the lock no longer protects anything and `recover_after_crash` refuses it with `DatabaseMoved`; restart the daemon. Acquire the lock **before** opening any `Store` in the process and keep it for the process lifetime: the lock holds a descriptor on the database file, and on POSIX closing any descriptor of a file releases the process's `fcntl` locks on it, which SQLite uses, so dropping a `DaemonLock` while a `Store` in the same process is mid-transaction can release SQLite's locks. Nothing enforces that order. The lock only protects against processes that also take it. A database created by an earlier unreleased build, or one whose tables, indexes, triggers or views differ from the frozen schema in any way, fails to open with a clear error; delete it.

### How steps hand work to each other

Each step is its own nanna task, and nanna creates a fresh worktree from `base` for every task. A later step therefore does not see an earlier step's files. It receives the earlier step's summary, file list and patch as text in its description. The patch is handed over whole or not at all: if a step other than the last produces a patch larger than 100,000 bytes, the flow stops with `patch_apply_failed` and says so; nothing is truncated.

Inchworm itself never applies, commits or pushes anything, and **publishing a pull request is not supported yet, so with nanna as it is a successful `run` (state `pr_open`) is not reachable: nothing can create the pull request it looks for.** `run` looks for a draft pull request on `inchworm/<issue>-a<attempt>` after the last step, but nothing in this repository makes one exist, and with nanna as it is today no step can: nanna creates each task's worktree with `git worktree add <path> <branch>` (no `-b`), so the base branch (default `main`) is either already checked out or gives a detached HEAD; its `git_commit` and `git_push_branch` tools refuse a detached HEAD and `main`/`master`; the `pr-shepherd` card has no branch-creating tool; and a step that does not commit hands the next step only patch text. A flow whose steps finish without such a pull request ends the attempt as `agent_failed` ("no pull request exists") and is not retried, because replaying the steps cannot create one. This is a flaw in the flow model, not something to work around by weakening nanna's protections; it needs nanna to create the per-attempt branch and carry one step's changes to the next, and is an input for the reconciliation work. Everything about the hand-off in this section is therefore design intent, not tested behaviour; what is verified is the wire protocol with the real binary (`tests/real_nanna.rs`) and the flow logic against a fake nanna.

## What a run does

1. Each step is an `assign_task` to nanna (previous summary and patch forwarded, previous failure on retries). The first failing step stops the attempt; the failure is classified into a `FailureClass` and retried per `retry_on` and `max_attempts`. A step output touching `.inchworm/**` or an `inchworm.toml` is rejected as `protected_path_violation`.
2. The last step must leave a PR on `inchworm/<issue>-a<n>`. inchworm records its number and head SHA and the nanna report (denials, QA) and the attempt becomes `published`.
3. inchworm resolves the flow's blocking checks (see "Blocking checks" above) and polls the check runs, plus the commit statuses for `blocking = "all"`, of that exact SHA until they are all green (30 s interval, 2 h limit). A red blocking check fails the attempt as `ci_red`, an unresolvable set as `blocking_unresolved`, a timeout as `timeout`.
4. The judges (`diff`, `nanna_clean`, `llm`) run against the PR patch fetched from GitHub; the head must still equal the recorded SHA. Verdicts are stored per attempt. The LLM judge shares the single model permit with nanna dispatch.
5. `policy::decide` picks the outcome. `draft_pr`: comment on the issue with the outcome, swap the labels to `inchworm:done`, leave the PR a draft, run ends `pr_open`. `merge_when` (only in a project that sets `[merge] allow = true`; otherwise the flow is a configuration error): `gh pr ready` then `gh pr merge <n> --match-head-commit <sha> --merge|--squash|--rebase`; never `--admin`; run ends `merged`. A rejection fails the attempt as `judge_rejected` or `merge_blocked` and retries if allowed.
6. After the last attempt the issue gets `inchworm:failed` and a comment with the failure class.

## Known limitations

- CI is read from check-runs on the head SHA. Commit-status contexts (some coverage bots post those) are read only for `blocking = "all"`; for `required` and for an explicit list a name that branch protection satisfies with a status context would wait until the CI timeout and fail as `timeout`.
- The PR of a rejected attempt stays open. A merge refused after `gh pr ready` marks the PR a draft again; if that undo fails the failure text says so.
- The merge grant is `Denied` unless the project sets `[merge] allow = true`; no shipped config does.

## Daemon

`inchworm daemon` polls every `poll_secs`. On start it marks attempts that were still `dispatched` as `orchestrator_lost`, then resumes queued, running, `awaiting_ci` and `judging` runs from the store (published attempts continue from their stored head SHA; a lost dispatch retries if `orchestrator_lost` is in `retry_on`, else the run fails). Each poll lists open issues with the flow's trigger label, creates a run, and swaps `inchworm` for `inchworm:running`. Each leased run executes on its own thread, so one repo waiting on CI (up to 2 hours) does not block the others. One run is active per repo (FIFO) and `concurrency` (default 1) is the number of model permits shared across repos: nanna dispatch and the LLM judge wait for a permit, CI waiting and GitHub calls do not. A run that errors on GitHub or the store, or panics, is retried with a delay of 1, 2, 4 and then 8 times `poll_secs` (at most 15 minutes) instead of waiting for a restart. SIGTERM or ctrl-c stop polling and give an in-flight run 20 seconds; a run still in flight is recovered on the next start. To retry an issue after `inchworm:failed`, add the `inchworm` label again.

## LLM judge safety

The `llm` judge fails closed. Ollama being down, refusing the connection, answering with a non-2xx status, hanging past the timeout, returning a body that is not the expected envelope, or returning a reply that is not exactly `{"score": <integer 0-10>, "reason": <string>}` (extra keys, duplicate keys, prose, code fences, trailing text, out-of-range or non-integer scores, a response body over 1 MiB, replies over 8 KiB) all produce a failed verdict with no score. Patches over 64 KiB are not judged and fail. Every model call is bounded by a timeout and holds the global model permit only while it runs.

The patch is untrusted input. It is placed in a delimited data section whose begin and end markers carry a fresh random value for every call, every line is prefixed with `| ` and control characters are escaped, so the patch cannot forge the section end marker, a new rubric, or a reply. The model is told the section is data, that instructions inside it must not change the rubric, and the reply format is restated after the data. The rubric comes from `.inchworm/`, which `validate` requires to be protected from agents.

A policy decision matches the verdicts to the configured judge list by kind and index and requires every one to pass exactly once, so a passing `llm` verdict can never override a failed `diff` or `nanna_clean` verdict and two `diff` passes cannot stand in for a missing `llm` judge. A corpus of injection diffs is covered by tests.

Residual risk: this reduces but does not eliminate prompt injection. A small local model can still be persuaded to emit a well-formed high score for a bad patch, and the strict schema only guarantees the reply shape, not the judgement. The `llm` judge is a score, not a security boundary: keep the deterministic `diff` and `nanna_clean` judges and CI in front of any merge, and treat a high `llm` score as advisory.

## Judge and policy contract

- The `diff` judge only accepts `git diff` output (`diff --git` headers). A patch without them, a binary change, a hunk whose lines do not match its declared counts (too many, or too few because the patch is truncated), a patch that does not end in a newline, a `diff --git` header naming two different paths with no `rename to` or `copy to` line, and any quoted, backslash-escaped or non-ASCII path (git's default quoting) fail closed, so a file with such a name cannot be merged automatically. The input must be exactly one patch (`git diff` output, as the net pull request diff is judged). After the last hunk, a single-commit `git format-patch` trailer is tolerated: a line that is exactly `-- ` followed by one version-looking line, meaning a line that starts with an ASCII digit, contains a dot and uses only ASCII letters and digits, `.`, `-`, `_`, `+`, `~`, `(`, `)` and spaces (so `2.54.0`, `2.11.0~rc1` and `2.39.5 (Apple Git-154)` pass, but so does arbitrary text of that shape such as `1 FAIL.`). That line is ignored and carries no path or change; every later line is parsed like any other, so a `diff --git` header, `---`/`+++` line, rename or copy line or a `+` or `-` line is still checked and no path can hide behind the trailer, while other non-patch lines (for example mbox framing) are ignored. A `-- ` with anything else after it (including no version line) fails closed, a multi-commit `git format-patch` mbox fails closed, and inside a hunk `-- ` is an ordinary hunk line. A patch cut exactly at a file or hunk boundary, or before a binary marker, cannot be detected.
- `forbid_paths` entries are [globset](https://docs.rs/globset) globs matched against the exact repository-relative path (forward slashes, no normalisation), including both sides of renames and copies. Syntax: `*` and `**` cross `/`, `?` is one character, `[abc]` and `[a-z]` are classes, `[!a]` or `[^a]` negate a class, `{a,b}` is alternation, `\` escapes the next character, and a `!` outside a class is literal (it does not negate the pattern). Matching is case-sensitive. A malformed or empty entry is rejected when the flow file is loaded (and by `validate`), and the judge also fails closed on one.
- Patterns that are valid but silently match nothing: a leading `./` or `/` (`./a.rs`, `/a.rs` never match `a.rs`), a bare directory (`src` and `src/` do not match `src/a.rs`; use `src/**`), a different case (`SRC/**` does not match `src/a.rs`), and a bare file name below the root (`publish.sh` does not match `sub/publish.sh`; use `**/publish.sh`). `dir/**` does not match a path named exactly `dir` (a file or symlink replacing the directory), so list `.github` next to `.github/**` to cover that case.
- `policy::decide` rejects with `judge_rejected` unless the verdicts equal the configured judges by kind and index, all passed. If every failed verdict is an infrastructure failure (Ollama down, timeout, bad envelope) the class is `model_error` instead. Judges take precedence over CI: a failed judge with a red check is `judge_rejected`.
- CI: a required check must be green on the exact judged SHA. A failed run followed by a green rerun with the same name and SHA stays `ci_red`. GitHub conclusions map fail closed: only `success` is green, `skipped`, `neutral`, `cancelled`, `timed_out`, `action_required`, `stale` and anything unknown are red, and anything not `completed` is pending.
- `policy::decide` takes a caller-supplied `MergeGrant` (`Denied` by default, `Granted`) meaning that a repository-declared rule currently grants this orchestrator the right to merge there. `Merge` is returned only when the mode is `merge_when`, the grant is `Granted` and every safety condition holds; otherwise the result is a draft pull request. The grant is the config type `MergeGrantSetting`, produced from `[merge] allow` in `inchworm.toml` by `MergeConfig::grant` (the single mapping; `allow = false` is `Denied`, and `merge_when` with `allow = false` is a config error). `decide` reads no file, and no production caller of `decide` exists in this change, so merging is unreachable until nanna-coder#1043 defines the capability and a later change wires `Granted` to it. Every shipped default and example uses `draft_pr`; nothing enables `merge_when`.
- The scheduler gives each repo one active run at a time. The global `concurrency` limit is a semaphore around model calls, not around leases, so a run that holds its lease can still judge. Only the LLM judge takes it in this change; dispatch does not exist yet and is expected to take the same permit when it lands.
- Per-directory configuration (`inchworm.toml`), offramps and a per-step auditor (D1 to D3) exist only as configuration (see "Project file"); nothing runs an offramp or an auditor, and `policy::decide` does not read them. `inchworm run` requires and validates them (a file without an `[auditor]`, a flow without a judge or a `host` auditor is refused with exit `2`) but never executes them. `Verdict` carries an optional `step` and the judge types are `non_exhaustive`, but a per-step verdict (D3) additionally needs a policy change: `policy::decide` matches verdicts by (kind, index) and requires exactly one per configured judge, so a second verdict with the same kind and index and a different `step` is currently rejected as a duplicate.

## Development

The ten tests in `tests/real_nanna.rs` are `#[ignore]` and are **not run by CI**; run them by hand with `cargo test --test real_nanna -- --ignored` (needs `nanna` installed or `INCHWORM_NANNA_BIN`).

The pre-commit hook runs fmt, clippy, tests, doctests, audit, deny, docs, a security review of the staged diff and a coverage no-regression check against the merge base with `origin/main` (or `INCHWORM_COVERAGE_BASE`). The review must start with exactly `VERDICT: SAFE` on its first line and contain no `UNSAFE` verdict, NUL byte or standalone "not safe" phrase (`scripts/review-verdict.sh`). This is a tripwire, not a classifier: it only catches a review that contradicts itself in those exact words. Wording such as "not safely wrapped", "cannot be considered safe", "risky" or "vulnerable" still passes after a `VERDICT: SAFE` first line, so a human still reads the review. `cargo-husky` installs the hook on `cargo test`; in a git worktree it cannot, so copy `.cargo-husky/hooks/pre-commit` into the repository's hooks directory by hand.

The coverage gate compares line coverage with the merge base with `origin/main` (or `INCHWORM_COVERAGE_BASE`, or the pull request's base commit in CI) and fails if it dropped. While `main` is a stub (about 50% coverage) it is trivially satisfied. It is a regression check, not a quality bar.

The toolchain is pinned to the exact version in `rust-toolchain.toml` (1.97.0, with rustfmt and clippy), and CI installs that same version. A test (`tests/toolchain_pin.rs`) fails if the file names a moving channel, if `ci.yml` installs a different version, or if the toolchain running the tests is not the pinned one. Bump the version in both files together. Run the tests with the pinned toolchain: any other (`cargo +nightly test`, or a newer stable) makes `the_pinned_toolchain_is_the_one_running_the_tests` fail on purpose. Pinning exists because the standard library's error wording changed between releases and made a test pass locally and fail in CI.

CI runs fmt, clippy, tests, doctests, audit, deny, the docs build and the coverage comparison with the pull request's base commit; it does not run the security review, which is local only. The coverage gate is a regression check against the merge base with `origin/main`, not a quality bar, and it is trivially satisfied while `main` is a stub.

## License

MIT OR Apache-2.0
