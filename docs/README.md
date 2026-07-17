# OneBrain CLI — Docs

User guides for the tool, plus design & internals docs for people reading the source. This complements the top-level [`README.md`](../README.md) (the quick overview) and [`CONTRIBUTING.md`](../CONTRIBUTING.md) (how to build + submit).

## User guides

| Doc | Read it when you want to… |
|---|---|
| [`install.md`](install.md) | Install or update the binary — Homebrew / npm / direct download, the per-platform binary table, self-update behavior, build from source, and the security & trust model. |
| [`commands.md`](commands.md) | See the full `onebrain <noun> <verb>` command tree, what each verb group does, and the output modes (`--json` / `--yaml` / envelope contract). |
| [`configuration.md`](configuration.md) | Look up every `onebrain.yml` key — what it does, its default, valid range, and whether `doctor --fix` can reset it. |
| [`token-optimization.md`](token-optimization.md) | Understand the token-optimization level ladder, the two cache layers, `onebrain token gain/check/discover`, and the honesty-signal contract. |
| [`platform-support.md`](platform-support.md) | Check which release targets ship full semantic search vs keyword-only. |
| [`serve.md`](serve.md) | Run the embedded local web UI (`onebrain serve`) — file explorer, reading view, search, agent chat — and understand its security posture. |
| [`reference/mcp.md`](reference/mcp.md) | Plug OneBrain into Claude Code / Cursor / any MCP client (`onebrain mcp`) — including standalone vault-search use on any Markdown folder. |
| [`why.md`](why.md) | The case for a deterministic vault binary instead of ad-hoc agent shell improvisation. |

## Design & internals

| Doc | Read it when you want to… |
|---|---|
| [`architecture.md`](architecture.md) | Understand the 4-crate workspace, how a command flows from `main` to the filesystem, and why the crate boundaries fall where they do. |
| [`architecture/search.md`](architecture/search.md) | Follow the whole native-search system end-to-end: what's stored on disk per collection, the reindex/embed pipeline (incl. the auto-hook split), and what every search surface — CLI verbs, MCP tools, WebUI, `status` — actually touches, with degradation behavior. |
| [`reference/`](reference/) | Navigate the source file-by-file: every module's purpose, its key types/functions, and how it connects to the rest. A code/API reference — start here when you want to follow the calls. |
| [`decisions/`](decisions/) | Understand *why* a choice was made (Rust over Bun, direct-GitHub self-update, the canonical `Envelope`, …). One Architecture Decision Record (ADR) per choice. |
| [`rust-patterns.md`](rust-patterns.md) | Learn the idiomatic Rust this codebase uses — trait objects, compile-time target detection, error enums, atomic file swaps — each anchored to a real file. |
| [`output-style.md`](output-style.md) | See the contract every failure/warning message follows (`✗ what: why` + `💡 hint`), what's frozen (envelope shape, error/exit codes) vs. improvable (message wording), and the checklist for adding new output. |

## Who this is for

- **Contributors** orienting before a change — start with `architecture.md`, then the relevant ADR.
- **People studying the codebase** as a worked example of a production Rust CLI.
- **Rust learners** — `rust-patterns.md` is a guided tour of the patterns, with file references you can open alongside.

> These docs describe intent and rationale. The source is the source of truth; if a doc and the code disagree, the code wins — please open a PR fixing the doc.
