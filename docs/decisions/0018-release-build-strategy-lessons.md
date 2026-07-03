# 0018 — Release build strategy: cross-compile windows-arm64, exercise the matrix before tagging

- **Status:** accepted
- **Date:** 2026-07-03

## Context

The first v3.4.0 tag failed release builds on 5 of 9 targets, and it took three further attempts to get a green matrix. Every failure was invisible to regular CI because CI tests only native builds on the three stock runners, while the release matrix cross-compiles — and the native-search work had just introduced the workspace's first heavyweight native build-script dependencies (fastembed→ort, simsimd, plus a TLS stack choice). The specific traps, in the order they surfaced:

1. **openssl via dependency defaults** — fastembed's default features pulled `native-tls`; cross sysroot has no OpenSSL (ADR 0017 fixed this with rustls features).
2. **ONNX Runtime prebuilt coverage** — `ort` has no binaries for mac-x64/musl/ARM32 (ADR 0017: platform tiers).
3. **Missing cross C++ runtime** — onnxruntime needs `-lstdc++`; the aarch64-linux job installed only `gcc-…`, not `g++-…`.
4. **windows-arm64 on the native `windows-11-arm` runner** — without an MSVC dev env, simsimd's `cc` build produced no lib (`LNK1181`); adding `msvc-dev-cmd arch: arm64` made it *worse* by corrupting PATH so rustc invoked Git-Bash's GNU `link.exe` — even host build scripts failed.
5. **macOS `clang_rt.osx` cache poisoning** (regular CI, same week) — simsimd's cached build-script output bakes the active Xcode's clang runtime dir; runner-image Xcode rotation left restored caches pointing at a missing path.

## Decision

- **windows-arm64 is cross-compiled from the x64 `windows-latest` runner** with the mature `amd64_arm64` MSVC toolset — host tools stay canonical x64; only `cc`-compiled target C goes through the cross environment. Native arm64 runners are revisited when the image + action ecosystem stabilizes.
- **simsimd is excluded on windows-arm64 (release attempt 9); its dot product is replaced by a pure-Rust scalar fallback.**
  - **Technical challenges encountered (8 attempts):** simsimd's C proved unbuildable for `aarch64-pc-windows-msvc`.
    - MSVC `cl` rejects its GCC-flavored dialect.
    - clang-cl half-detects ARM, so NEON paths miss `arm_neon.h`.
    - Disabling all SIMD then trips the Windows SDK's own arch detection in `winnt.h` (`PSLIST_HEADER`/`PCONTEXT` undefined).
  - **Implementation solution:** `simsimd` is now a `[target.'cfg(not(all(target_os = "windows", target_arch = "aarch64")))'.dependencies]` entry, and `crates/onebrain-search/src/vector.rs` dispatches to a `dot_scalar` (LLVM auto-vectorized, `f64` accumulation to match simsimd) on that target.
  - **Test coverage:** a parity unit test asserts the two agree within `1e-5` wherever simsimd is present.
  - **Pipeline cleanup:** the dead clang-cl compile/prebuild steps were removed from `release.yml`.
- **macOS caches are keyed by Xcode version** (plus a `LIBRARY_PATH` fallback to the current clang runtime dir) — in **both** `ci.yml` and `release.yml` (attempt 9 ported the fix into the release matrix after the same poisoning hit `aarch64-apple-darwin`).
- **Process rule:** any change that adds or reconfigures a native build-script dependency (`-sys` crates, `cc`-built C, prebuilt-binary downloaders like ort) must be exercised against the release matrix — a tag-less `workflow_dispatch` run or equivalent — *before* the version tag is pushed. Regular CI green is not evidence the matrix builds.

## Consequences

- The windows-arm64 artifact is produced by a well-trodden path at the cost of not exercising a native arm64 host; runtime behavior is unchanged (same target triple).
- Release failures of this class become a pre-tag checklist item instead of a tag-day surprise; the checklist lives in `docs/release-checklist.md`.
- Tag hygiene: a tag whose run failed before creating a Release object can be safely deleted and re-pushed; once a Release/npm/brew publish exists, never re-point the tag (bump a patch instead).
