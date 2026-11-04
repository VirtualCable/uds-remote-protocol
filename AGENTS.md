<!-- code-review-graph MCP tools -->
## Language and licensing

**This is (going to be) an open-source project.** Rules:

- **All code and code comments: English only.** Identifiers, doc comments,
  inline comments, commit messages, PR descriptions.
- **All distributable files: English only.** `README.md`, `AGENTS.md`,
  `LICENSE`, in-repo docs, examples.
- **Spanish is fine for the private stuff**: chat communication, private
  plans (`doc/plan/`, which is git-ignored on purpose), internal notes,
  "padawan" documents.
- **License: BSD 3-Clause** (same as uds client). Every crate declares
  `license = "BSD-3-Clause"` via the workspace; keep the `LICENSE` file at
  the repo root in sync.

## Design philosophy

- **State machines: second round, not first.** The owner prefers solving
  the problem directly first, detecting the stateful tangle in practice,
  and ONLY THEN consolidating into an explicit state machine (that is how
  the console FSM was born, §18.55). If the state machine shape is obvious
  up front, fine — but do not force one by default.
- Same pattern for abstractions in general: earn them with evidence, then
  consolidate. Avoid speculative generality.
- **Source file header (mandatory in every source file, present and
  future)**: `.rs` files start with these four lines (adapt the comment
  prefix for other file types, e.g. `#` for shell/PowerShell/Dockerfile):

  ```
  // BSD 3-Clause License
  // Copyright (c) 2026, Virtual Cable S.L.
  // All rights reserved.
  // Authors: Adolfo Gómez, dkmaster at dkmon dot com
  ```

## MCP Tools: code-review-graph

**This project has a knowledge graph. Start with the code-review-graph
MCP tools to narrow scope, then read the source.** The graph is cheaper than scanning files and
gives you structural context (callers, dependents, test coverage) that file search cannot.

### When to use graph tools FIRST

- **Exploring code**: `semantic_search_nodes_tool` or `query_graph_tool` instead of Grep
- **Understanding impact**: `get_impact_radius_tool` instead of manually tracing imports
- **Code review**: `detect_changes_tool` + `get_review_context_tool` instead of reading entire files
- **Finding relationships**: `query_graph_tool` with callers_of/callees_of/imports_of/tests_for
- **Architecture questions**: `get_architecture_overview_tool` + `list_communities_tool`

### Verify in the source

- Narrow scope with the graph, then read the source. Do not change code from graph output alone.
- For any non-trivial change, read the implementation and the relevant tests before concluding.
- Verify the exact source when touching behavior, database logic, migrations, retries, fallbacks,
  recovery, or compatibility code.
- When the graph and the source disagree, the source wins. The graph may be stale or may not
  model that relationship.
- An empty graph result can mean "not indexed" or "not statically visible", not "does not exist".

### Key Tools

| Tool | Use when |
| ------ | ---------- |
| `detect_changes_tool` | Reviewing code changes — gives risk-scored analysis |
| `get_review_context_tool` | Need source snippets for review — token-efficient |
| `get_impact_radius_tool` | Understanding blast radius of a change |
| `get_affected_flows_tool` | Finding which execution paths are impacted |
| `query_graph_tool` | Tracing callers, callees, imports, tests, dependencies |
| `semantic_search_nodes_tool` | Finding functions/classes by name or keyword |
| `get_architecture_overview_tool` | Understanding high-level codebase structure |
| `refactor_tool` | Planning renames, finding dead code |

### Workflow

1. The graph auto-updates on file changes (via hooks).
2. Use `detect_changes_tool` for code review.
3. Use `get_affected_flows_tool` to understand impact.
4. Use `query_graph_tool` pattern="tests_for" to check coverage.
<!-- /code-review-graph MCP tools -->
