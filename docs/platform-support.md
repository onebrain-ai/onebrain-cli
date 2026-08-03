# Platform support

Two capabilities vary by platform: **search tier** (semantic vs keyword-only) and the **OS scheduler backend** behind `onebrain schedule register`.

## Search — semantic vs keyword

Every release target ships a binary with the **full CLI** and **keyword (lexical/BM25) search**. **Semantic** search (vector + hybrid `query`, plus `vsearch` and `model set`) additionally needs an ONNX Runtime prebuilt, which isn't available on every platform — so some targets ship **keyword-only**. The tiering is driven by the `ort-sys` prebuilt list ([ADR 0017](decisions/0017-platform-tiered-semantic-search.md)); this table, the release-workflow matrix, and the ADR all agree. Windows-arm64 is cross-compiled from x64 and native-dep changes are matrix-tested before tagging ([ADR 0018](decisions/0018-release-build-strategy-lessons.md)).

| Target | Binary | Keyword search (lex) | Semantic search (vector/hybrid) | Notes |
|---|---|---|---|---|
| macOS arm64 (Apple Silicon) — `aarch64-apple-darwin` | ✅ | ✅ | ✅ | |
| macOS x64 (Intel) — `x86_64-apple-darwin` | ✅ | ✅ | ❌ lex-only | no ONNX Runtime prebuilt for darwin-x64 |
| Linux x64 glibc — `x86_64-unknown-linux-gnu` | ✅ | ✅ | ✅ | |
| Linux ARM64 glibc — `aarch64-unknown-linux-gnu` (Pi 3/4/5 64-bit) | ✅ | ✅ | ✅ | |
| Linux x64 musl / Alpine — `x86_64-unknown-linux-musl` | ✅ | ✅ | ❌ lex-only | ONNX Runtime prebuilt is glibc-only, not musl |
| Linux ARMv7 32-bit — `armv7-unknown-linux-gnueabihf` (Pi 2/3/4/5 32-bit OS) | ✅ | ✅ | ❌ lex-only | onnxruntime has no 32-bit ARM support |
| Linux ARMv6 32-bit — `arm-unknown-linux-gnueabihf` (Pi 1 · Zero) | ✅ | ✅ | ❌ lex-only | onnxruntime has no 32-bit ARM support |
| Windows x64 — `x86_64-pc-windows-msvc` | ✅ | ✅ | ✅ | |
| Windows ARM64 — `aarch64-pc-windows-msvc` | ✅ | ✅ | ✅ | |

On a keyword-only (lex-only) binary: `search search`, `get`, `status`, and `reindex` work fully; hybrid `query` falls back to keyword ranking with a one-line notice; `vsearch` and `model set` report that semantic search is unavailable in that build.

Download links for each target: the [pre-built binaries table](install.md#pre-built-binaries). How the `semantic` cargo feature implements the seam: [`reference/onebrain-search.md`](reference/onebrain-search.md).

## OS scheduler backends (v3.4.20+)

`onebrain schedule register` compiles the `onebrain.yml schedule:` block into the platform's own scheduler — there is no bundled daemon:

| Platform | Backend | Artifact | Output capture |
|---|---|---|---|
| macOS | launchd (gui domain) | `~/Library/LaunchAgents/com.onebrain.<label>.plist` | **skill mode:** the CLI opens `~/Library/Logs/onebrain/onebrain-<label>.log` itself, after exec, and writes a run record into the vault. **command mode:** plist file redirect → `~/Library/Logs/onebrain/` (launchd opens it *before* exec, so a missing directory is still fatal there) |
| Windows | Task Scheduler | task `\OneBrain\<label>` (no file on disk — Task Scheduler owns the definition) | none from OneBrain — use Task Scheduler history |
| Linux | systemd user timers | `~/.config/systemd/user/onebrain-<label>.{service,timer}` | journald — `journalctl --user -u onebrain-<label>.service` |

Semantics shared by design, worth knowing:

- **Logged-in only.** All three backends register into the *user* session scheduler (launchd gui domain · an `InteractiveToken` task · a systemd user manager). Entries fire only while that user has a session; nothing runs from a cold machine. OneBrain deliberately does not escalate to system daemons/lingering.
- **Missed runs are skipped, mostly.** Windows tasks (no `StartWhenAvailable`) and Linux timers (`Persistent=false`, a deliberate divergence) simply skip occurrences that pass while there is no session. macOS differs: launchd coalesces occurrences missed while asleep and fires the job once on wake.
- **One-shot entries (`at:`) delete themselves after firing** on every platform — Task Scheduler via `DeleteExpiredTaskAfter`, systemd via an `ExecStopPost` cleanup, launchd via the emitted self-removal block.
- `--dry-run` prints the exact artifact(s) for the current platform; `schedule list` asks the OS scheduler for truth rather than trusting files on disk.
