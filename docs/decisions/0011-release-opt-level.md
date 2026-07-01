# 0011 — Release build optimizes for size (`opt-level = "z"`), not speed

- **Status:** accepted
- **Date:** 2026-07-01

## Context

The `[profile.release]` in `Cargo.toml` has used `opt-level = "z"` (size) + `lto = "fat"` + `strip = "symbols"` since v3.2.15. The question resurfaced: onebrain is a CLI — wouldn't `opt-level = 2`/`3` (optimize for speed) be better, since binary size "doesn't matter much"? We benchmarked it instead of guessing.

onebrain's real workload is **short-lived and I/O-bound**: each command spawns a fresh process, walks/reads/writes markdown, parses small YAML, maybe shells out (qmd/claude), then exits. The `serve`/daemon path is I/O + network. There is little CPU-bound compute on the hot path — the exception is content/task **scanning** (`note search`, `task list`).

## Decision

**Keep `opt-level = "z"`.** Benchmark (2026-07-01, warm cache, synthetic 800-note / 3.1 MB vault, only `opt-level` varied; `lto = "fat"` + `strip` held constant):

| profile | binary size | `task list` (scan 800) | `note search` | `doctor` | `session init` (qmd wait) |
|---|--:|--:|--:|--:|--:|
| `z` (current) | **3.77 MB** | baseline | baseline | baseline | baseline |
| `2` | 5.52 MB (**+46%**) | −16% | −5…−10% | −7% | ~0% |
| `3` | 5.85 MB (**+55%**) | −21% | −1…−9% | −10% | ~0% |

Key facts from the data:

- `opt-level ≥ 2` **does** speed up the CPU-touching commands (`task list` ~21%, `note search`/`doctor` ~5–10%), so "it's pure I/O, opt-level is irrelevant" is only true for the truly wait-bound path (`session init`'s qmd probe showed exactly 0.0%).
- But the **absolute** savings are **1–3 ms** — imperceptible for a human-invoked CLI, and these run once per invocation, not in a hot loop.
- The ~+2 MB size jump comes from `opt-level ≥ 2` (aggressive inlining) + `lto = "fat"`, **not** from `3` specifically. `2` and `3` are near-identical in size, and their speed edge over `z` is small and **inconsistent between them** — noise at the 12–18 ms scale (`2` even beats `3` on content search, −10% vs −1%; `3` wins `task list`). Neither gives a predictable advantage, and both cost ~+2 MB — so neither justifies the upgrade over `z`. There is no "small like `z`, fast like `3`" option.

Paying +46–55% binary for an invisible speedup means a bigger npm/brew download, a slower cold start (the warm benchmark understates this), and slower CI compiles — a bad trade for interactive use.

## Consequences

- Smallest install and fastest cold-start; the up-to-~21% CPU speedup on scan/search commands is left on the table (worth 1–3 ms/call).
- **Revisit `opt-level = 3` only** if a workload runs scans thousands of times in a batch/script (at 10 000 `note search` calls, `3` saves ~9% / ~16 s) or if `serve`/daemon throughput becomes CPU-bound — and benchmark that specific path first. Prefer `3` over `2` there (its `task list` / topic-search wins are the larger ones); `2` offers no reliable edge over `3` at essentially the same size.
- Full data + methodology: reproduce with `CARGO_PROFILE_RELEASE_OPT_LEVEL=<z|2|3> cargo build --release` and time real commands against a large vault.
