# Command surface

v3.1 locks a singular-noun, two-level grammar — `onebrain <noun> <verb>` — so every command path is predictable. Five root verbs handle the common flow; eleven resource groups cluster the rest.

```text
onebrain
├── init                       create / re-scaffold a vault (--yes · --force · --no-sync)
├── update                     self-update the binary (--check · --plan)
├── doctor [--fix]             read-only health checks (text folds the 2 migration checks into 1 row) + auto-repair recipes
├── serve                      local web UI + vault JSON API (--port · --open)
├── mcp                        MCP stdio server — vault search tools (--vault)
│
├── vault       sync · current
├── session     init
├── checkpoint  stop · reset · orphans
├── search      query · search · vsearch · get · status · reindex · model
├── note        read · list · find · search · stat · new · append · edit
│               · move · mkdir · archive · delete · orphans · backlinks
├── task        list
├── plugin      install · update · migrate
├── schedule    register · list
├── token       gain · check · discover
├── skill       run
└── harness     detect
```

| Group | Verbs | Purpose |
|---|---|---|
| **Setup** | `init`, `plugin install`, `vault sync` | Scaffold `onebrain.yml` + PARA folders, register the plugin with the harness, overlay the latest plugin tarball. |
| **Runtime** (hook protocol) | `session init`, `checkpoint stop · reset · orphans`, `search reindex · search reindex --lex-only · search reindex --pending-only` | Called by the harness `SessionStart` / `Stop` / `PostToolUse` hooks. `search reindex --lex-only` runs on PostToolUse (incremental keyword-index pass, no model load); `search reindex --pending-only` runs on Stop (embeds pending docs in background). Emit hard-wired JSON; banner suppressed for clean machine stdio. |
| **Search** | `search query · search · vsearch · get · status · reindex · model` | Native hybrid search (tantivy BM25 + fastembed embeddings, RRF-fused, then Tier-2 cross-encoder reranked — [ADR 0025](decisions/0025-tier2-cross-encoder-reranker.md)) over the vault's `*.md` notes, plus embedding-model and reranker-model management with an interactive TUI. The native `search` verbs are the sole search surface as of v3.4.5, completing the v3.4 native-search cutover (the previous external search commands were removed, not deprecated). `search reindex` flags: `--lex-only` (incremental keyword-index pass; never loads/downloads the embedding model; changed docs stay pending for the next embed pass), `--pending-only` (embeds only pending-vector docs; loads the model only when there is pending work; in `--json` mode detaches to background); a full reindex also fetches the reranker model when `search.reranker.enabled` and not yet downloaded (`LoadingReranker` phase) — the `--lex-only`/`--pending-only` hook paths do NOT, so a vault indexed only via the hooks shows the "unreranked (… run `onebrain search reindex`)" hint until one bare `search reindex` fetches the model. |
| **MCP** | `mcp` | Stdio [Model Context Protocol](reference/mcp.md) server exposing `query`/`get`/`multi_get`/`status` over the native search engine — for Claude Code, Cursor, or any MCP client. See [`reference/mcp.md`](reference/mcp.md) for the full tool reference. |
| **Notes** | `note read · list · find · search · stat · new · append · edit · move · mkdir · archive · delete · orphans · backlinks` | Structured vault-note operations — wikilink-aware moves, dated archiving, orphan/backlink graph queries. |
| **Tasks** | `task list` | List dated vault tasks (fence-aware), filterable by due date and folder. |
| **Token optimization** | `token gain · check · discover` | Report/administer the token-optimization ladder + cache (v3.4.10): `gain` reports byte-exact savings (summary, `--by` pivot, `--history`, `--reset`, `--rebuild`); `check` is the read-hook's 0/2 allow/deny verdict over the already-sent ledger; `discover` estimates missed savings from direct `Read`/`Grep` bypass traffic in Claude Code session transcripts. See [`token-optimization.md`](token-optimization.md). |
| **Web UI** | `serve` | Host the binary-embedded web UI + token-gated vault JSON API on `127.0.0.1` (routes to this vault's daemon on its ephemeral port; a standalone `serve --port` defaults to 6789) — file explorer, reading view, search panel, agent chat; `--open` launches the browser. See [`serve.md`](serve.md). |
| **Maintenance** | `doctor [--fix]`, `plugin update · migrate`, `schedule register` | Read-only health checks + `--fix` recipes (incl. per-key config-value validation with comment-preserving reset-to-default), self-update the binary + rewrite hooks + rebind OS scheduler artifacts, compile the `onebrain.yml schedule:` block into OS scheduler artifacts. |
| **Diagnostics** | `vault current`, `harness detect` | Report which mechanism resolved the active vault, and which AI harness is running. |

> The tree shape was **locked for v3.2+** — verbs beyond the working set above were stubbed with a stable `E_NOT_IMPLEMENTED` (exit 72) so the grammar couldn't drift while features landed. **v3.4.24 (#334) reversed that**: the 63 verbs that only ever returned 72 were removed from the parser and now fail as unknown commands, because a shipped binary that accepts verbs it cannot perform is a trap for scripts and docs. See ADR 0006 (superseded). Hidden v3.0 flat aliases (`session-init`, `qmd-reindex`, `register-hooks`, …) still dispatch, printing a one-time migration notice (silence with `ONEBRAIN_QUIET_MIGRATION=1`); they're removed no earlier than v4.

Not every target ships every search capability — see the [platform-support matrix](platform-support.md) for which binaries are semantic-search-enabled vs keyword-only.

## Output modes

Interactive commands default to human-readable `text`; pass a flag for structured output. Every structured payload is wrapped in the canonical `Envelope<T>`:

```bash
onebrain doctor                 # TTY: animated per-check report, colorized
onebrain doctor --json          # { version, command, ok, vault, data, warnings, error }
onebrain vault current --yaml   # same envelope, YAML
onebrain search status --json | jq .data
onebrain token gain --json --by month,surface   # same envelope, Tier-2 pivot data
```

- `--output {text,json,yaml,table,tsv}` — full matrix on every command; `--json` / `--yaml` are shorthands.
- `--pretty` forces indented JSON even when stdout is piped; `--no-color` (or `NO_COLOR`) forces monochrome; `-q` drops info logs (errors still hit stderr).
- Output auto-adapts: piped/CI invocations drop color and the startup banner, so machine consumers get clean bytes with no flags. Closed-pipe writes (`onebrain search reindex | head`) exit `0`, not a panic.
- **`token gain` / `token discover`** follow the same matrix (`--json` is a local shorthand for `--output json`, still rendered through the canonical envelope). **`token check`** is the one exception: it's a hook-facing verdict, not a report — no `--output` flag; it always answers with a bare **exit 0** (allow, stdout empty) or **exit 2** (deny, a reference-envelope JSON object on stdout) — see [`token-optimization.md`](token-optimization.md#token-check--the-hooks-verdict).
