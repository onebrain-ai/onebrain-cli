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
| `intent` | string | no | — | **Compatibility only, not used by the native engine yet.** Background context for disambiguation; native intent-aware ranking is relevance-phase work (v3.4.3). |
| `rerank` | boolean | no | — | **Compatibility only, not used by the native engine.** Native rerank (bge-reranker-v2-m3) lands in v3.4.3. |

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
  "cache_dir": "/Users/you/Library/Caches/onebrain/search/my-vault-a1b2c3",
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

## Versioning & compatibility

- **qmd-compat contract**: the tool names (`query`/`get`/`multi_get`/`status`) and most parameter names deliberately match the external `qmd` MCP server this replaces, so agent instructions written against qmd need no rewrite to call this server instead. Params the native engine doesn't yet use are still accepted (see each tool's table above) rather than rejected — a client sending them gets normal behavior, not a schema error.
- **Plugin config staging**: the OneBrain plugin's `.mcp.json` registers this server under the config key `qmd` through **v3.4.1** — this is why the example above uses `"qmd":` as the server key even though the binary command is `onebrain mcp`. The key renames to `search` (with a matching agent-instruction update) in **v3.4.2**. Until then, the `mcp__plugin_onebrain_qmd__*` tool namespace keeps working unchanged.
- **Server version = CLI version**: the MCP `initialize` handshake's `serverInfo` reports `{ "name": "onebrain", "version": "<CLI version>" }` — there is no separate MCP-protocol version to track; it always matches `onebrain --version`.

## Roadmap

`onebrain mcp` is designed to grow: future tool groups (notes, tasks, and other vault resources) will mount on this same server surface as they land, following the [gateway vision](../../README.md#roadmap) of one MCP entry point for the whole vault rather than one server per resource type. This page will gain a new per-tool-group section for each as it ships — per the maintenance rule at the top of this page.
