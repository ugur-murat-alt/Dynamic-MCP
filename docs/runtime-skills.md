# Runtime Skills

Runtime skills are deterministic, control-plane-only sequences of downstream
tool calls. They do not add Host MCP tools and are unrelated to the Markdown
skill installed into OpenCode or Claude Code.

## Files And Reload

Place skills directly in the daemon config directory with the exact suffix
`.skill.toml`. Normal server loading ignores that suffix. Skill files are loaded
in deterministic filename order with server manifests and `policy.toml`; all
three snapshots are validated and published atomically after the existing 500
ms debounce. A bad skill retains the complete prior snapshot.

Every referenced server must exist in the same validated registry snapshot.
Symlinks, nested files, hidden files, and uppercase suffixes are not loaded.

## Format

```toml
id = "issue-notify"
name = "Create and announce issue"
description = "Create an issue, then send its URL to chat"

[[inputs]]
name = "title"
type = "string"

[[inputs]]
name = "priority"
type = "number"
required = false
default = 2

[[steps]]
id = "create"
server = "github"
tool = "create_issue"
timeout_ms = 30000
arguments = { title = "${input.title}", priority = "${input.priority}" }

[[steps]]
id = "notify"
server = "chat"
tool = "post_message"
arguments = { text = "Issue: ${steps.create.output.structuredContent.url}" }
```

A skill has 1 through 16 ordered steps. Step and input IDs use lowercase ASCII
letters followed by lowercase letters, digits, or underscores. Step server IDs
use the normal server-ID rules. Tool names must be non-empty, arguments must be
a JSON object, and `timeout_ms` must be in `1..=300000` when present. Unknown
fields, duplicate IDs, invalid input defaults, and forward references reject the
complete reload. Conditions, loops, dependencies, DAGs, expressions, and JSONPath
are not supported.

## Inputs And Templates

Input types are `string`, `number`, `boolean`, and `json`. Inputs are required by
default. Set `required = false` to permit a missing value, which resolves to JSON
`null` when no default exists. Runtime input must be one JSON object with no
unknown fields.

Two reference namespaces exist:

- `${input.name}` reads a validated runtime input.
- `${steps.step_id.output.path}` reads the raw result of an earlier step.

Paths traverse object keys and numeric array indices. A string that is exactly
one reference preserves the referenced JSON type. A reference embedded in a
larger string inserts strings directly and compact-JSON encodes every other
type. References apply recursively to argument values, not object keys. There
is no escaping syntax for a literal `${` sequence.

Only previous steps may be referenced. A missing runtime output path produces a
secret-safe `SKILL_TEMPLATE_ERROR` and stops execution.

## Execution And Results

```bash
mcp-host skill list
mcp-host skill run issue-notify --input '{"title":"Login fails"}'
mcp-host skill run issue-notify --input-file inputs.json
```

`--input-file -` reads stdin; input defaults to `{}`. Skill execution is
synchronous and stateless. The default control wait is 4,805 seconds, the upper
bound implied by 16 steps each using the maximum timeout plus framing allowance;
global `--timeout` explicitly chooses a shorter control deadline.

The engine clones one immutable skill snapshot, then calls each step through
`RuntimeManager::call_tool`. Every step therefore performs the existing `call`
policy check. The first runtime/policy/template error stops the sequence. A
downstream result with `isError: true` is also a failed step and stops execution.
No later step runs.

Success and step failure both return a `SkillRunResult`. It contains status,
step totals, successful count, ordered raw results, and optional failure metadata
with zero-based `step_index` plus a secret-safe `RuntimeError`. A failed
`isError` result remains in `results`; a transport or policy failure has no raw
result for that step. Accumulated raw results are limited to 7 MiB so the final
control envelope remains below the 8 MiB IPC frame limit.

CLI exit status is `0` on complete success, `5` for downstream `isError`, and
`4` for other skill failures. Tool arguments and interpolated values are not
included in errors or structured logs.

## Policy

Skill-level policy uses `action = "skill_run"` and the dedicated `skill` glob:

```toml
[[rules]]
id = "allow-issue-skills"
action = "skill_run"
effect = "allow"
skill = "issue-*"
```

`skill` is valid only for `skill_run`; `server` is not valid for that action.
Default effect and deny precedence are unchanged. An allowed skill can still
stop on a denied server/tool step because every step independently passes the
normal `action = "call"` policy.

## Stable Surfaces

Control protocol v1 adds `skill_list` and `skill_run` request variants. The Host
MCP server still exposes exactly nine fixed tools and does not publish
`tools/list_changed`. Running skills are not stored, listed, resumed, or retried.
