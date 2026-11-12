# Fixture provenance

These JSON files stand in for nanna's answers in tests. They differ in how much
they can be trusted, and the tests that use them should be read with that in
mind.

## Captured from a real nanna

Checked on 2026-11-11 against the installed `nanna` (built 9 November 2026), run
as `nanna mcp-serve` in a sandbox with a temporary HOME, config dir and state
dir. No model and no container were involved.

- `initialize_result.json`: identical to the real `initialize` result.
- `assign_refused_legacy.json`: the real shape of a synchronous refusal
  (JSON-RPC error `-32602`, message `spawn refused: block: unknown_identity: ...`,
  no `data`). The identity name and the list of known identities differ from the
  day it was captured because the catalog differs.

The installed build predates nanna-coder #1030, so its errors carry no
`data.failure_kind` and its failed results carry no `structuredContent`.

## Reconstructed, not captured

These were written from nanna's source and its own tests. A live task needs a
model and a container, and none has been run end to end, so they have **not**
been compared with a real answer.

- `assign_refused_structured.json`, `assign_invalid_structured.json`: the
  `data.failure_kind` refusals defined by nanna-coder #1030 (draft; not in any
  released nanna).
- `result_failed_structured.json`, `result_cancelled_structured.json`: the
  `structuredContent` shape defined by #1030.
- `result_failed_offline_deps.json`: the `OfflineDependenciesFailed` label from
  nanna-coder #1031 (draft).
- `create_task_result.json`, `task_working.json`, `task_completed.json`,
  `task_failed.json`, `task_cancelled.json`, `result_success.json`,
  `result_failed_legacy.json`, `result_cancelled_legacy.json`: the task and
  result shapes described by nanna's `mcp/handlers.rs` and `task.rs`.

Because of this, `classify` has two live paths on the installed nanna: the
`error_type` label parsed from the text of a failed result, and the text
heuristics. The `failure_kind` path is exercised only against reconstructed
fixtures until a nanna that emits it is installed.
