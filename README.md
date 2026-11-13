# inchworm

A daemon and CLI for configuring fallible, repeated agentic processes on local models. inchworm schedules and sequences work per repository and reports success rates and specific failure conditions. It runs agents through [nanna](https://github.com/DominicBurkart/nanna-coder), which sets up sub-agents with fixed capabilities and runs them in per-repo containers.

## Status: partial, and not yet aligned with the project design

This is milestone 1 of an MVP: configuration, run state, a SQLite store and offline validation. It cannot run an agent yet.

It was written from a planning conversation, not from the project's design. The design is issue [#1](https://github.com/DominicBurkart/inchworm/issues/1) and `DESIGN.md`, which is now on `main`. Where the two differ, the design wins. The differences are tracked in issue [#6](https://github.com/DominicBurkart/inchworm/issues/6):

- **D1, met except `register`/`unregister`**: a directory is configured by `inchworm.toml` at its root, and `inchworm list` prints the registered directories (one `slug<TAB>path` line each; it refuses a slug or path with a control character, and exits `2` if it cannot write). A directory is registered by adding a `[[repo]]` entry to the global config by hand; `register` and `unregister` are a documented non-goal for now because editing the config without losing its comments needs a TOML-editing dependency. The old `.inchworm/flows.toml` is no longer read: `validate` reports that it moved.
- **D2, configuration only**: `[offramp]` accepts `noop`, `issue` and `escalate` with a default and per-flow values, validated, but nothing executes an offramp yet.
- **D3, configuration only**: `[auditor]` is mandatory, says where the auditor runs and fails closed, but nothing runs an auditor yet. The judges after a run remain.
- **D4, partial**: only the issue-to-draft-PR journey exists. The tagged `trigger` is the extension point for the CI-failure and merge-conflict journeys; any other `kind` is rejected with an "unknown variant" error.
- **D5, not addressed**: the marigold/Rust control-flow split and the fate of the prototype branch are undecided.
- **D6, met**: `DESIGN.md` is on `main` with a table mapping each concept to where it is implemented.

Everything below describes what this code does today.

## Merging has no default

A flow must state its outcome: `outcome` and `outcome.mode` are required and have no default. `merge_when` exists as a mode and is validated, but no shipped example uses it, and `inchworm validate` warns when a flow turns it on. The intended direction is that merging becomes a capability a user grants to a nanna subagent or another agent, with the user defining when and how (nanna-coder issue 1043). Whether merging is possible at all is one switch, `[merge] allow` in `inchworm.toml`, which is `false` unless set. With it `false` a flow in `merge_when` mode is a configuration error, not a warning. `MergeConfig::grant` maps it to `MergeGrantSetting::Denied` or `Granted`, the one place the later policy code reads. Do not enable `merge_when` in a real repository.

## Commands

```
inchworm validate [--config PATH]
inchworm list [--config PATH]
```

`list` prints one line per registered directory, the slug and the path separated by a tab, and exits `0`, or `2` when the config cannot be found, read or parsed.

Exit codes:

- `0`: everything validated
- `1`: at least one error was found, either in a repository or in the global config (for example a missing or non-executable `nanna_bin`, reported with scope `[config]`); all errors are listed on stdout
- `2`: the command could not run: bad usage, no config could be found, or the config could not be read or parsed (the reason is on stderr)

`validate` is offline. It does not talk to GitHub, nanna or a model. It needs `HOME` only when the config uses `~`, and the default config location needs `XDG_CONFIG_HOME` or `HOME`.

For the global config it checks that `nanna_bin` is an executable file or, for a bare name, that it is on `PATH`. For every repository it checks:

- the path is a git repository (a `.git` file or symlink must lead to a gitdir that exists)
- `inchworm.toml` at the repository root passes the rules below; if only the old `.inchworm/flows.toml` exists, that is an error saying it moved
- every `llm` judge rubric exists and is a regular file inside `.inchworm/`
- `.inchworm` is not a symlink
- `.nanna/protected.toml` follows nanna's entry rules and lists both `inchworm.toml` and `.inchworm/**`, so agents can change neither the configuration nor the rubrics. Its only key is `extra`, for example `extra = ["inchworm.toml", ".inchworm/**", "scripts/deploy.sh"]`; any other key (such as `entries`) is rejected as an unknown key
- `.nanna/prompt.md` exists (a warning only)

The repo-controlled files above (`.inchworm`, `.nanna` and everything read from them) are read only if no component of the path under the repository root is a symlink at the time of the check, each is a regular file, and each is read with a size cap: 256 KiB for `inchworm.toml`, 64 KiB for `protected.toml`, 4 KiB for a `.git` file. The symlink check is a walk of the path before the read, and only the final component is opened without following symlinks, so a directory swapped for a symlink between the walk and the read is not prevented. The global config may itself be a symlink (for example from home-manager), but must resolve to a regular file of at most 256 KiB.

What a passing `validate` proves: the files inchworm reads are well formed, `inchworm.toml` and `.inchworm/**` are declared protected, and nothing it reads escapes the repository through a symlink. The `protected.toml` check mirrors `validate_extra` in nanna-coder `harness/src/protected.rs` as of nanna-coder commit `239a1c94`; it does not run nanna's code, so a later change there is not tracked. It does not prove that identities named in flows exist, that labels exist, or that branch protection matches `required_checks`; those online checks are not built yet.

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
- Not implemented: per-directory configuration, offramps and a per-step auditor (D1 to D3). `Verdict` carries an optional `step` and the judge types are `non_exhaustive`, but a per-step verdict (D3) additionally needs a policy change: `policy::decide` matches verdicts by (kind, index) and requires exactly one per configured judge, so a second verdict with the same kind and index and a different `step` is currently rejected as a duplicate.

## Development

The pre-commit hook runs fmt, clippy, tests, doctests, audit, deny, docs, a security review of the staged diff and a coverage no-regression check against the merge base with `origin/main` (or `INCHWORM_COVERAGE_BASE`). The review must start with exactly `VERDICT: SAFE` on its first line and contain no `UNSAFE` verdict, NUL byte or standalone "not safe" phrase (`scripts/review-verdict.sh`). This is a tripwire, not a classifier: it only catches a review that contradicts itself in those exact words. Wording such as "not safely wrapped", "cannot be considered safe", "risky" or "vulnerable" still passes after a `VERDICT: SAFE` first line, so a human still reads the review. `cargo-husky` installs the hook on `cargo test`; in a git worktree it cannot, so copy `.cargo-husky/hooks/pre-commit` into the repository's hooks directory by hand.

The coverage gate compares line coverage with the merge base with `origin/main` (or `INCHWORM_COVERAGE_BASE`, or the pull request's base commit in CI) and fails if it dropped. While `main` is a stub (about 50% coverage) it is trivially satisfied. It is a regression check, not a quality bar.

The toolchain is pinned to the exact version in `rust-toolchain.toml` (1.97.0, with rustfmt and clippy), and CI installs that same version. A test (`tests/toolchain_pin.rs`) fails if the file names a moving channel, if `ci.yml` installs a different version, or if the toolchain running the tests is not the pinned one. Bump the version in both files together. Run the tests with the pinned toolchain: any other (`cargo +nightly test`, or a newer stable) makes `the_pinned_toolchain_is_the_one_running_the_tests` fail on purpose. Pinning exists because the standard library's error wording changed between releases and made a test pass locally and fail in CI.

CI runs fmt, clippy, tests, doctests, audit, deny, the docs build and the coverage comparison with the pull request's base commit; it does not run the security review, which is local only. The coverage gate is a regression check against the merge base with `origin/main`, not a quality bar, and it is trivially satisfied while `main` is a stub.

## License

MIT OR Apache-2.0
