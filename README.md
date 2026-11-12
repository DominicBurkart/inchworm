# inchworm

A daemon and CLI for configuring fallible, repeated agentic processes on local models. inchworm schedules and sequences work per repository and reports success and failure rates and the specific failure conditions. It runs agents through [nanna](https://github.com/DominicBurkart/nanna-coder), which sets up sub-agents with fixed capabilities and runs them in per-repo containers.

## Status

The MVP is functionally complete behind fakes: configuration, run state, the SQLite store, offline and online validation, dispatch of a flow to nanna, CI waiting on the exact head SHA, judges, the outcome policy, the polling daemon and the stats reports.

Not done or deliberately off:

- `merge_when` is built and tested but no shipped config enables it. Every repo should start at `draft_pr`.
- The human gate (nanna `.nanna/policy.toml` `human_gate`) is not consulted; the policy input is always `NotConsulted`. This is a pending decision, see the plan.
- Nothing in the test suite talks to a real nanna, GitHub or Ollama. Run `inchworm validate --online` and one foreground `inchworm run` on a throwaway issue before trusting the daemon.

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

Every configured repo needs three files before the daemon can work on it:

- `.inchworm/flows.toml`, the flows (see Flows below)
- `.nanna/protected.toml` containing `extra = [".inchworm/**"]`, so agents cannot edit their own flows or judges
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

The unit sets `PATH` and `NANNA_CONFIG_DIR`; edit both for your machine. Put `GH_TOKEN` (if you do not rely on `gh auth login`) in `~/.config/inchworm/env`, mode 600, which the unit loads with `EnvironmentFile`; never put it in `Environment=`. The unit restarts on failure every 15 seconds and gives up after 5 starts within 10 minutes (`StartLimitBurst`), so a transient fault recovers while a persistent config error does not loop forever; `systemctl --user reset-failed inchworm` clears the limit. The unit deliberately sets only `NoNewPrivileges`, `RestrictSUIDSGID` and `UMask`: `PrivateTmp`, `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectControlGroups` and `LockPersonality` each make `podman unshare` fail under a user service (`newuidmap: write to uid_map failed`), which breaks rootless podman and with it nanna's containers. The state directory is created mode 700 and the database mode 600, and a lock file next to the database (`state.db.lock`) makes a second daemon on the same database refuse to start. To keep it running while logged out: `loginctl enable-linger $USER`.

## Operational notes

- `inchworm daemon` refuses to start when `nanna_bin` is not an executable.
- Stopping the daemon waits up to 20 seconds for the in-flight run; a run still in flight is marked `orchestrator_lost` on the next start and counts as one attempt. This is a known limitation: there is no cooperative cancel of a dispatched nanna task on shutdown.
- Every `gh` call is bounded by 60 seconds and runs in its own process group that is killed with it and the poll cycle runs on its own thread, so SIGTERM stops the daemon even while `gh` hangs. The SIGTERM and SIGINT handlers are installed before the daemon starts.
- A failed dispatch drops the nanna client and the next attempt starts a fresh `nanna mcp-serve`.
- Re-adding the trigger label to an issue whose run finished (`pr_open`, `merged` or `failed`) starts a new run on purpose; remove the label when the work is done.
- If the initial label swap on an issue fails, the daemon remembers that only in memory; after a restart a stale trigger label next to a finished run can start another run until the label is removed by hand. Known limitation.
- Text from nanna, `gh` and judges has control characters and bidirectional override characters replaced and is capped before it is printed by `failures`, `run` or the daemon log, or posted to an issue.
- Branch protection is compared by required check name only. A passing `validate --online` does not check `enforce_admins`, required reviews or app ids, so it says little about merge safety on its own.
- Skipped, neutral, cancelled and unknown check conclusions are treated as red and fail the attempt as `ci_red`, even where GitHub branch protection accepts a skipped job (a path-filtered or conditional job). Do not list such a job in `required_checks`; there is no opt-in for treating `skipped` or `neutral` as green.

## Commands

```
inchworm validate [--config PATH] [--online] [--build-images] [--skip-branch-protection]
inchworm run <repo-slug-or-path> <issue> [--flow NAME] [--config PATH]
inchworm daemon [--config PATH]
inchworm status [--config PATH]
inchworm stats [--window 24h|7d|30d|all] [--config PATH]
inchworm failures [--class CLASS] [--config PATH]
```

Exit codes: `0` ok, `1` the repos have errors or the run failed, `2` the config could not be read or a setup error.

`validate` checks, for every repo in the global config:

- the path is a git repository
- `.inchworm/flows.toml` parses and passes its rules
- every rubric file referenced by an `llm` judge exists
- `.nanna/protected.toml` lists `.inchworm/**` so agents cannot edit their own flows or judges
- `.nanna/prompt.md` exists (warning only)

With `--online` it also checks that every step identity resolves with `nanna agents list --repo <path>`, that the trigger label and the `inchworm:running`, `inchworm:failed` and `inchworm:done` labels exist on GitHub, and that each flow's `required_checks` match the required status checks of its base branch protection (`gh api repos/<slug>/branches/<base>/protection/required_status_checks`). `required_checks` names must be checks that the base branch protection requires whenever it is readable. A listed check the branch does not require is an error; an extra protected check or an unprotected base is a warning, and an error for `merge_when`. If the protection cannot be read because the repository is private on a plan without branch protection (HTTP 403 "Upgrade to GitHub Pro"), that is a warning with the reason for `draft_pr` flows and an error for `merge_when`; every other failure stays an error. `--skip-branch-protection` skips that lookup. `--build-images` implies `--online` and runs `nix build path:<repo>#devContainerImage --no-link` for each repo; it only builds.

`run` executes one flow for one issue in the foreground and takes it all the way to an outcome (see below). `daemon` does the same for every labelled issue.

`status` lists recent runs. `stats` prints runs, successes and success rate per repo, flow and window (24h, 7d, 30d, all) from SQL aggregates; a run succeeded if it ended `merged` or `pr_open`. `failures` prints the failure class histogram and the most recent raw errors, optionally for one class.

## What a run does

1. Each step is an `assign_task` to nanna (previous summary and patch forwarded, previous failure on retries). The first failing step stops the attempt; the failure is classified into a `FailureClass` and retried per `retry_on` and `max_attempts`. A step output touching `.inchworm/**` is rejected as `protected_path_violation`.
2. The last step must leave a PR on `inchworm/<issue>-a<n>`. inchworm records its number and head SHA and the nanna report (denials, QA) and the attempt becomes `published`.
3. inchworm polls the check-runs of that exact SHA until every name in `required_checks` has a finished run (30 s interval, 2 h limit). A red required check fails the attempt as `ci_red`, a timeout as `timeout`.
4. The judges (`diff`, `nanna_clean`, `llm`) run against the PR patch fetched from GitHub; the head must still equal the recorded SHA. Verdicts are stored per attempt. The LLM judge shares the single model permit with nanna dispatch.
5. `policy::decide` picks the outcome. `draft_pr`: comment on the issue with the outcome, swap the labels to `inchworm:done`, leave the PR a draft, run ends `pr_open`. `merge_when`: `gh pr ready` then `gh pr merge <n> --match-head-commit <sha> --merge|--squash|--rebase`; never `--admin`; run ends `merged`. A rejection fails the attempt as `judge_rejected` or `merge_blocked` and retries if allowed.
6. After the last attempt the issue gets `inchworm:failed` and a comment with the failure class.

## Known limitations

- CI is read from check-runs on the head SHA. Legacy commit-status contexts (some coverage bots post those) are not read, so a `required_checks` entry that branch protection satisfies with a status context would wait until the CI timeout and fail as `timeout`.
- The PR of a rejected attempt stays open. A merge refused after `gh pr ready` marks the PR a draft again; if that undo fails the failure text says so.
- The human gate is never consulted.

## Daemon

`inchworm daemon` polls every `poll_secs`. On start it marks attempts that were still `dispatched` as `orchestrator_lost`, then resumes queued, running, `awaiting_ci` and `judging` runs from the store (published attempts continue from their stored head SHA; a lost dispatch retries if `orchestrator_lost` is in `retry_on`, else the run fails). Each poll lists open issues with the flow's trigger label, creates a run, and swaps `inchworm` for `inchworm:running`. Each leased run executes on its own thread, so one repo waiting on CI (up to 2 hours) does not block the others. One run is active per repo (FIFO) and `concurrency` (default 1) is the number of model permits shared across repos: nanna dispatch and the LLM judge wait for a permit, CI waiting and GitHub calls do not. A run that errors on GitHub or the store, or panics, is retried with a delay of 1, 2, 4 and then 8 times `poll_secs` (at most 15 minutes) instead of waiting for a restart. SIGTERM or ctrl-c stop polling and give an in-flight run 20 seconds; a run still in flight is recovered on the next start. To retry an issue after `inchworm:failed`, add the `inchworm` label again.

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
- The scheduler gives each repo one active run at a time. The global `concurrency` limit is a semaphore around model calls (dispatch and the LLM judge), not around leases, so runs in different repos wait on CI in parallel and a run that holds its lease can still judge.
- Not implemented: per-directory configuration, offramps and a per-step auditor (D1 to D3). `Verdict` carries an optional `step` and is `non_exhaustive`; `config::Judge` and `config::Trigger` are not, so adding a variant is a breaking change for code that matches them exhaustively.

## Development

The pre-commit hook (installed by `cargo-husky` on `cargo test`, or copy `.cargo-husky/hooks/pre-commit` into the git hooks directory when working in a worktree) runs fmt, clippy, tests, doctests, audit, deny, docs, a security review of the staged diff and a coverage no-regression check against `main`.

## License

MIT OR Apache-2.0
