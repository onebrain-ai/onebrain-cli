# 0021 — Native-search state moves to the persistent data dir

- **Status:** accepted
- **Date:** 2026-07-03

## Context

The native-search engine's on-disk state — the downloaded embedding model (hundreds of MB to >1 GB), the tantivy lexical index, the flat vector store, and the `engine.redb` metadata database — lived under the OS **cache** directory: `~/Library/Caches/onebrain/search/<collection>/` on macOS, `$XDG_CACHE_HOME/onebrain/search/` on Linux, `%LOCALAPPDATA%\onebrain\search\` on Windows. This was resolved by `migration::default_state_dir()` (`dirs::cache_dir()` + `/onebrain`), which `search_common::search_cache_root()` extends with `/search`.

On a real machine (2026-07-03, issue #114) the entire collection cache dir — ~536 MB (an e5-small model at 487 MB + the tantivy/vector index for ~715 docs + `engine.redb`) — vanished. Directory birth timestamps showed it recreated fresh when the next `onebrain search` command ran. No onebrain code path deletes the whole dir (`model remove` / the TUI delete single model dirs behind an active-model guard; `reindex --force` wipes index files but explicitly preserves models), and `brew upgrade` was ruled out. The most likely cause is macOS storage cleanup (or a third-party cleaner): **`~/Library/Caches` is purgeable by OS contract** — the system may evict it under disk pressure without asking.

The user silently lost an expensive model download plus a fully-embedded index and had to re-download and re-embed. Expensive-to-recreate data must not live in a location the OS is contractually free to delete.

## Decision

- **Relocate the native-search state to the persistent data dir**, via `dirs::data_dir()` instead of `dirs::cache_dir()` in `migration::default_state_dir()`:
  - macOS: `~/Library/Application Support/onebrain/search/`
  - Linux: `$XDG_DATA_HOME/onebrain/search/` (default `~/.local/share/onebrain/search/`)
  - Windows: `%APPDATA%\onebrain\search\` (Roaming)
  These are the OS-sanctioned homes for application data the user expects to persist — none is purgeable by OS cleanup.
- **Move existing state once, automatically.** `migration::migrate_search_cache()` renames `<cache_dir>/onebrain/search` → `<data_dir>/onebrain/search` exactly once — only when the old path exists and the new one does not — and is called at the top of `dispatch()` so it runs before any command touches the index. It is best-effort: a move failure warns on stderr but never aborts the command; the old data is left intact for manual recovery and the engine recreates an empty index at the new location.
- **Move only the `search/` subtree, not the whole `onebrain/` state dir.** The update-check cache (`latest-release.json`) is resolved independently through `dirs::cache_dir()` in `onebrain_fs::update` and is genuinely disposable — it stays in the cache dir on purpose. Sweeping the whole `onebrain/` dir into the data dir would strand `latest-release.json` where the update code no longer looks for it.
- **`doctor` distinguishes "purged" from "never indexed."** The native-search check reads whether a collection was already configured (`search.collection`, or the legacy `qmd_collection`) before it resolves one. An already-configured collection with a missing index reports `index missing (<collection>) — search cache may have been removed by OS storage cleanup` (hint: `onebrain search reindex`) rather than the neutral "no index yet" a genuinely fresh vault gets.
- **The `ONEBRAIN_CACHE_DIR` test override keeps its name.** It still short-circuits `default_state_dir()` (now to a data-dir stand-in), which also makes `migrate_search_cache()` a no-op under the override. Renaming it would churn every search/mcp integration test for no behavioural gain; the name is an internal test knob, documented as such.

## Consequences

- The model + index survive OS cache eviction; the incident in #114 cannot silently recur.
- Existing users migrate transparently on their next command — no manual step, no re-download, no re-embed. In the common case the old and new dirs share a volume (`~/Library/Caches`↔`~/Library/Application Support`, `~/.cache`↔`~/.local/share`, `%LOCALAPPDATA%`↔`%APPDATA%`), so the move is an atomic, instant rename regardless of index size. On unusual setups where the two straddle volumes — a redirected `$HOME`/`$XDG_*`, a symlinked cache, networked storage — `fs::rename` fails with `EXDEV`; because the migration is best-effort it then warns on stderr and leaves the old data intact (no copy fallback), and the engine recreates an empty index at the new location until the user runs `onebrain search reindex`.
- The data dir grows with the model + index and is **not** reclaimed by OS cleanup — this is the intended tradeoff (persistence over auto-reclaim). Users reclaim space explicitly via `onebrain model remove` or by deleting the collection dir; `search status` reports the cache dir and its size.
- The update-check cache stays purgeable, so a version-check probe file can still be evicted harmlessly — only the expensive, hard-to-recreate state moved.
- Doc comments across `migration.rs`, `search_common.rs`, and `onebrain-search/src/lib.rs` now name the data-dir paths; the CLI remains the single owner of the concrete path (`search_cache_root`).
