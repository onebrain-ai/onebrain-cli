# MCP API reference

> **This page is the canonical MCP API reference. Any PR that adds, changes, or removes an MCP tool or parameter MUST update this page in the same PR.**

`onebrain mcp` is OneBrain's native [Model Context Protocol](https://modelcontextprotocol.io) server. It hosts vault search tools today (`query`, `get`, `multi_get`, `status`) and is the single entry point future vault tool groups (notes, tasks, other resources) will mount on as they land — see [ADR 0019](../decisions/0019-native-mcp-server-staged-qmd-cutover.md) for the architecture and staged-cutover rationale.

## Starting the server

```bash
onebrain mcp --vault /path/to/vault
```

- `onebrain mcp` speaks JSON-RPC 2.0 over **stdio** — it's meant to be spawned by an MCP client, not run interactively.
- **Requires a vault**: either `--vault <PATH>` (highest priority), the `ONEBRAIN_VAULT` env var, or walk-up discovery from the current directory (an `onebrain.yml` in the cwd or an ancestor). See `onebrain mcp --help` for the full global-flag list (output-format flags are accepted but irrelevant — the server always speaks JSON-RPC).
- The server never downloads an embedding model just to start: `status` and the connection handshake never construct the embedder (see `Engine::open`'s lazy-embedder contract in [`onebrain-search.md`](onebrain-search.md#srcembedrs)). A model download is only triggered by the first `query` sub-query of type `vec`/`hyde`, or by `onebrain search reindex`.

### Client registration

**Claude Code** (`.mcp.json`, project- or user-scoped):

```json
{
  "mcpServers": {
    "qmd": {
      "command": "onebrain",
      "args": ["mcp", "--vault", "/path/to/vault"]
    }
  }
}
```

> The server key is `qmd` in v3.4.1 — see [Versioning & compatibility](#versioning--compatibility) below for why, and when that changes.

**Generic MCP client** (any client that spawns a stdio server):

```json
{
  "vault-search": {
    "command": "onebrain",
    "args": ["mcp", "--vault", "/path/to/vault"]
  }
}
```

## Use as a standalone vault-search MCP

`onebrain mcp` also works as a generic local Markdown search engine for **any** folder that has an `onebrain.yml`, not just a full OneBrain vault. Hybrid lex + semantic search, multilingual, single binary, no Node/Python.

Minimal `onebrain.yml` (folder defaults + a search collection name are all it needs):

```yaml
folders:
  inbox: 00-inbox
  projects: 01-projects
search:
  collection: my-notes
```

Then index and register it in any MCP client:

```bash
onebrain search reindex --vault /path/to/notes
```

```json
{ "vault-search": { "command": "onebrain", "args": ["mcp", "--vault", "/path/to/notes"] } }
```

A zero-config `--dir`-only mode (no `onebrain.yml` required) is on the backlog for v3.4.3+ — not yet available.

## Tools

### `query`

Search the vault with typed sub-queries (`lex` = BM25 keywords, `vec` = semantic question, `hyde` = hypothetical answer passage), fused client-side via Reciprocal Rank Fusion. The first sub-query in the list gets 2x weight; every other sub-query gets 1x.

**Parameters**

| Name | Type | Required | Default | Notes |
|---|---|---|---|---|
| `searches` | array of `{ type, query }` | yes | — | 1–10 typed sub-queries. `type` is `"lex"`, `"vec"`, or `"hyde"` (lowercase). `lex` never triggers a model download; `vec`/`hyde` embed the query text. |
| `limit` | number | no | `10` | Max results returned. |
| `minScore` | number | no | `0` | Minimum normalized relevance, 0–1. The top fused hit always scores exactly `1.0`, so `minScore` is a fraction of the best hit's fused rank score, not an absolute similarity. |
| `candidateLimit` | number | no | — | **Compatibility only, not used by the native engine.** Accepted for qmd schema compatibility; deserializes without error, has no effect. |
| `collections` | array of string | no | — | **Compatibility only, not used by the native engine.** The native index is single-collection per vault, so there's nothing to select between. |
| `intent` | string | no | — | **Compatibility only, not used by the native engine yet.** Background context for disambiguation; native intent-aware ranking is relevance-phase work (v3.4.5). |
| `rerank` | boolean | no | — | **Compatibility only, not used by the native engine.** Native rerank (bge-reranker-v2-m3) lands in v3.4.5. |

**Result shape**

```json
{
  "results": [
    {
      "docid": "notes/meeting.md#2",
      "file": "notes/meeting.md",
      "title": "meeting",
      "score": 1.0,
      "context": "Decisions > Q3 roadmap",
      "snippet": "…agreed to ship the native search engine before…"
    }
  ]
}
```

`context` is omitted entirely (not `null`) when the hit has no heading path.

When the vault has **no search index yet** (never reindexed, or the cache was purged), `query` does not error — it returns an empty `results` array plus a `note` string telling the agent to run `onebrain search reindex` or fall back to filesystem search (grep/read). `note` is omitted entirely once an index exists:

```json
{ "results": [], "note": "No search index for this vault yet — run `onebrain search reindex`, or fall back to filesystem search (grep/Read)." }
```

**Example call**

```json
{
  "name": "query",
  "arguments": {
    "searches": [
      { "type": "lex", "query": "release checklist" },
      { "type": "vec", "query": "what do we check before tagging a release" }
    ],
    "limit": 5,
    "minScore": 0.3
  }
}
```

### `get`

Read a file's contents by vault-relative path (typically taken straight from a `query` result's `file`/`docid`).

**Parameters**

| Name | Type | Required | Default | Notes |
|---|---|---|---|---|
| `file` | string | yes | — | Vault-relative path, e.g. `"notes/meeting.md"`. A trailing `:N` (e.g. `"notes/meeting.md:100"`) starts reading at line `N` — only a purely-numeric suffix counts as a line number, so Windows drive letters and other non-numeric `:`-suffixes stay part of the path. |
| `fromLine` | number | no | `1` | Start line (1-indexed). Overridden by a `:N` suffix on `file` if both are given. |
| `maxLines` | number | no | — (whole file) | Maximum number of lines to return from `fromLine`. |
| `lineNumbers` | boolean | no | `false` | Prefix each returned line with `"N: "`. |

**Result shape**: a single text content block — the sliced file text, no wrapping JSON.

**Example call**

```json
{ "name": "get", "arguments": { "file": "notes/meeting.md:100", "maxLines": 20, "lineNumbers": true } }
```

### `multi_get`

Read multiple files at once, by glob pattern or an explicit comma-separated path list.

**Parameters**

| Name | Type | Required | Default | Notes |
|---|---|---|---|---|
| `pattern` | string | yes | — | A glob (e.g. `"journals/2026-07*.md"`) matched against every file under the vault root (hidden dirs like `.git`/`.obsidian` are skipped, mirroring the search engine's own indexing walk) — **or**, if the string contains a comma, an explicit comma-separated list of vault-relative paths (trimmed, no globbing). |
| `maxLines` | number | no | — (whole file) | Maximum lines returned per file. |
| `maxBytes` | number | no | `10240` | Files larger than this are skipped, with a one-line note in the output, rather than dumped whole. |
| `lineNumbers` | boolean | no | `false` | Prefix each returned line with `"N: "`. |

**Result shape**: a single text content block, one `--- <path>` section per matched file, separated by blank lines. A file that's skipped (too large, unreadable, escapes the vault) gets a `--- <path>\n(skipped: <reason>)` section instead of its content. No files matched → `"no files matched"`.

**Example call**

```json
{ "name": "multi_get", "arguments": { "pattern": "notes/*.md", "maxBytes": 5000 } }
```

### `status`

Show the status of the search index: collection, embed model, document counts, pending changes, and health information. No parameters.

**Result shape** (identical JSON shape to `onebrain search status --json`'s `data` field — see [`onebrain-search.md`](onebrain-search.md) for field-by-field meaning):

```json
{
  "collection": "my-vault-a1b2c3",
  "embed_model": "multilingual-e5-small",
  "cache_dir": "/Users/you/Library/Application Support/onebrain/search/my-vault-a1b2c3",
  "indexed": true,
  "model_size_bytes": 493921024,
  "model_downloaded_at": 1751500000,
  "last_indexed_at": 1751500400,
  "index_size_bytes": 16777216,
  "doc_count": 412,
  "pending_new": 0,
  "pending_changed": 2,
  "pending_removed": 0,
  "cache_size_bytes": 510000000,
  "semantic_available": true
}
```

`index_size_bytes`, `cache_size_bytes`, and `reindexing` are omitted (not `null`) when there's nothing to report.

> **Note:** the MCP `status` tool's `indexed` reflects `doc_count > 0` (the engine is already open on this path), a deliberate refinement over `onebrain search status`'s cache-dir-existence `indexed` — an empty-but-present cache dir reads as `indexed: true` there but `indexed: false` here.

**Example call**

```json
{ "name": "status", "arguments": {} }
```

## Long-running operations (sync vs async)

A common question for tools like a future `reindex`: can an agent kick off long work and *not* block on the result? Yes — but that is modelled at the application level, not the protocol level.

**MCP tool calls are request/response.** Every `tools/call` carries an `id` and the client waits for the response with the matching `id`; there is no protocol-level "fire and forget with no reply." Long-running work is expressed with one of three patterns:

| Pattern | Mechanism | Fits |
|---|---|---|
| **Return-immediately + poll** | The tool starts the work on a detached thread and returns a short "started" acknowledgement immediately; the agent polls a status tool for progress/completion. | Reindex and other long batch jobs — the agent isn't blocked and isn't blind. |
| **Progress notifications** | `notifications/progress` (requires the client to pass a `progressToken`) streams progress while the request stays open, so the call still resolves at the end but the agent sees intermediate percentages. | Work whose final result the agent needs, but wants visible progress meanwhile. |
| **Concurrent transport + cancellation** | The transport is duplex: the client can issue other requests while one is in flight (each has its own `id`), and `notifications/cancelled` can cancel an in-flight request. | Keeping a slow call from blocking other tool calls. |

**Today, all four shipped tools are fast and effectively synchronous** — none starts background work. There is no `reindex` MCP tool yet; reindexing is a CLI/cron operation. However, the engine already provides the primitive for the *return-immediately + poll* pattern: `onebrain search reindex` writes an on-disk progress marker, and the `status` tool reports `reindexing: { done, total }` live — even when the reindex is running in a **separate** process. So a client can already observe reindex progress through `status` while a CLI/cron reindex runs.

**Forward design (roadmap, not a commitment):** when a `reindex` MCP tool lands (v3.4.4, alongside the auto-reindex work), the natural shape is the return-immediately + poll pattern above: `reindex` spawns the work and returns a "started" acknowledgement, and the agent polls `status` for `reindexing: { done, total }`. Two design constraints apply and are tracked for that milestone:

- The server holds the engine behind a single `Arc<Mutex<Engine>>`, so a `reindex` that held the lock for its whole duration would serialize (block) concurrent `query`/`status` calls. A long-running tool must run its work off the shared lock, not inside a single `with_engine` critical section.
- A long-lived server's in-memory readers (tantivy `IndexReader`, the vector mmap) must be reloaded after an external reindex commits, or the server would keep answering from the pre-reindex index until restart.

## Versioning & compatibility

- **qmd-compat contract**: the tool names (`query`/`get`/`multi_get`/`status`) and most parameter names deliberately match the external `qmd` MCP server this replaces, so agent instructions written against qmd need no rewrite to call this server instead. Params the native engine doesn't yet use are still accepted (see each tool's table above) rather than rejected — a client sending them gets normal behavior, not a schema error.
- **Plugin config staging**: the OneBrain plugin's `.mcp.json` registered this server under the config key `qmd` from the **v3.4.1** native-backend swap onward — this is why the example above uses `"qmd":` as the server key even though the binary command is `onebrain mcp`. The key was renamed to `search` (with a matching agent-instruction update) in the **v3.4.5** epic — plugin PR onebrain-ai/onebrain#208 — after which the tool namespace is `mcp__plugin_onebrain_search__*`.
- **Server version = CLI version**: the MCP `initialize` handshake's `serverInfo` reports `{ "name": "onebrain", "version": "<CLI version>" }` — there is no separate MCP-protocol version to track; it always matches `onebrain --version`.

## Roadmap

`onebrain mcp` is designed to grow: future tool groups (notes, tasks, and other vault resources) will mount on this same server surface as they land, following the [gateway vision](../../README.md#roadmap) of one MCP entry point for the whole vault rather than one server per resource type. This page will gain a new per-tool-group section for each as it ships — per the maintenance rule at the top of this page.
