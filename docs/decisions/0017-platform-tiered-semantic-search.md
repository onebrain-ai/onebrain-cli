# 0017 — Platform-tiered semantic search (`semantic` feature; lex-only fallback)

- **Status:** accepted
- **Date:** 2026-07-03

## Context

The v3.4.0 native search engine (ADR [0012](0012-native-search-replace-qmd.md)) embeds notes via `fastembed`, which links the ONNX Runtime through `ort` / `ort-sys`. Two of `fastembed`'s default features (`hf-hub-native-tls`, `ort-download-binaries-native-tls`) pull `openssl-sys`, which does not cross-compile to musl or ARM — while the rest of the codebase already standardizes on rustls (`ureq`). Worse, `ort-sys` only ships prebuilt ONNX Runtime binaries for a fixed target set; its build script errors `ort does not provide prebuilt binaries for the target` on anything else, and there is **no** ONNX Runtime for 32-bit ARM at all.

That set — read authoritatively from `ort-sys-2.0.0-rc.12`'s `build/download/dist.txt` for our feature set (`none`: no CUDA/webgpu) — covers only 5 of our 9 release targets. The other 4 (`x86_64-apple-darwin`, `x86_64-unknown-linux-musl`, and both 32-bit ARM targets from ADR [0009](0009-raspberry-pi-arm-matrix.md)) have no prebuilt, and ARM32 can never have one. The v3.4.0 release build failed on exactly these targets. Dropping semantic search everywhere to make the build pass would punish every platform for the limitations of a few; refusing to ship on Pi would regress ADR 0009's "every Pi has a binary" guarantee.

## Decision

Gate the fastembed/ONNX path behind a cargo feature **`semantic`** (default **ON**) in `onebrain-search`, forwarded by an `onebrain-cli` feature of the same name. `fastembed` also switches to `default-features = false` + the `-rustls-tls` variants (dropping `openssl-sys` **and** the unused `image-models` codec tree — we only use `TextEmbedding`). Everything portable stays unconditional: chunking, the lex/BM25 (tantivy) index, the vector store (simsimd/memmap2/redb are pure Rust), the `Embed` trait + fake embedders, and the model registry metadata.

Release targets with an ONNX prebuilt keep the default (semantic) build; the 4 without build **lex-only** via `--no-default-features`. In a lex-only binary, `search search`/`get`/`status`/`reindex` work fully (reindex lex-indexes + hashes, skipping embedding; `status` reports "semantic: unavailable in this build"), hybrid `query` degrades to keyword ranking with a one-line stderr notice, and `vsearch` / `model set` error clearly. A CI job keeps the `--no-default-features` build compiling + clippy-clean.

The **`ort-sys` prebuilt list is the single source of truth** for the tiering. It drives all three of: this ADR's tier table, the per-target `cargo_flags` in `.github/workflows/release.yml`, and the platform-support matrix in the repo `README.md` — they must agree exactly. The list below is `ort-sys-2.0.0-rc.12`'s `build/download/dist.txt` for our `none` feature set (no CUDA/webgpu):

| Release target | ONNX prebuilt (ort `none`) | Tier | Build flag |
|---|---|---|---|
| `aarch64-apple-darwin` | ✅ | semantic | (default) |
| `x86_64-apple-darwin` | ❌ | lex-only | `--no-default-features` |
| `aarch64-unknown-linux-gnu` | ✅ | semantic | (default) |
| `armv7-unknown-linux-gnueabihf` | ❌ (ARM32, impossible) | lex-only | `--no-default-features` |
| `arm-unknown-linux-gnueabihf` | ❌ (ARM32, impossible) | lex-only | `--no-default-features` |
| `x86_64-unknown-linux-gnu` | ✅ | semantic | (default) |
| `x86_64-unknown-linux-musl` | ❌ (gnu only, not musl) | lex-only | `--no-default-features` |
| `x86_64-pc-windows-msvc` | ✅ | semantic | (default) |
| `aarch64-pc-windows-msvc` | ✅ | semantic | (default) |

## Consequences

- Pi and other no-ONNX users get keyword search plus the full CLI out of the box — no semantic ranking until an ONNX path exists for their platform.
- The release build is unblocked: openssl-sys is gone from the graph (rustls throughout), and no target tries to link an ONNX Runtime it can't get.
- The `semantic` feature is the seam a future pure-Rust embedding backend (e.g. `ort-tract` or a `candle` embedder) can fill to promote a lex-only target to full semantic — without touching the lex/vector/registry code, which is already portable.
- One more CI job and a per-target `cargo_flags` matrix column; the tiering is documented inline in `.github/workflows/release.yml`.
