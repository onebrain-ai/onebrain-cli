# Output style — the ✗/⚠/💡 contract

How OneBrain CLI writes user-facing text output, in text mode. Established
by issue #279 (a full sweep of every command's error/warning strings); keep
new output consistent with it.

## The contract

Every user-facing **failure** message answers three questions, in order:

```
✗ <what happened — plain words>: <why — one clause>
💡 <what to do next — a concrete command or action>
```

- **What, then why, then next.** Lead with the plain-English outcome, not
  the exception type. The cause follows after a colon or em dash.
- **Plain words first.** Internal terms — engine lock, slot, redb, version
  skew, `{e:#}` debug dumps — get a plain-language phrase, with the
  technical term in parens if it's useful for grepping logs or filing a bug.
- **Every failure with a known remedy carries a `💡` hint naming the exact
  command or flag.** No dead-end errors. If there genuinely is no remedy,
  say that ("no action needed", "please report it") — don't omit the hint.
- **Warnings use `⚠`** with the same shape, for things that degrade
  gracefully rather than fail the command.
- **Hint lines never end with a period**, and literal flags, env vars, and
  commands are always backticked (`` `--port <N>` ``, `` `$ONEBRAIN_BIND` ``,
  `` `onebrain vault sync` ``).
- **Success output is unchanged in substance** — the emoji-section banner
  style (`section()` / `item()` helpers, `doctor`'s ✓/◐/✗ check glyphs, the
  `search reindex` progress lines, the `update` orchestrator output) stays
  as-is. A *success* line may still carry a `💡` hint without an ✗/⚠ glyph —
  e.g. `daemon already running (pid N) — this vault is already served` +
  `` 💡 run `onebrain daemon stop --vault .` first if you want to restart it ``.
- **Every `💡` hint must reference a command/flag that actually exists.**
  Verify against the `clap` command tree (`cli.rs`) before writing one in.

## Errors that reach the top-level envelope: `HintedError`

An `eprintln!` site prints its literal ✗/⚠/💡 string directly. A **bail
site** (an `Err` that propagates to `main::render_error`) must NOT bake
glyphs into the anyhow message — that string becomes the `--output json`
envelope's `error.message`. Instead it returns a
[`output::HintedError`](../crates/onebrain-cli/src/output/hinted.rs)
`{ plain, hint }`: text mode renders `✗ {plain}` + `💡 {hint}`; structured
modes see `plain` only (single-line, glyph-free). When wrapping an
underlying error, attach it with `.context(HintedError::new(..))` so the
source stays in the chain and `exit::exit_code_for`'s specific mappings
(e.g. permission-denied → 66) survive.

## Frozen vs. improvable

- **Frozen:** the `Envelope<T>` JSON/YAML *shape*, error *codes* (`E_*`),
  and *exit codes*. `text_render` closures passed to `output::emit()` only
  run in `OutputMode::Text`, so restyling one never touches the payload.
- **Improvable:** `error.message` *wording* may get better — but it stays
  single-line and glyph-free. Glyphs and 💡 hints are text-mode-only
  dressing (see `HintedError` above).
- **Machine-parsed / grep-target substrings** stay verbatim inside reworded
  messages — e.g. `"vault-sync: failed:"`, `"vault-sync: download failed"`,
  `"not inside a vault"`, `"multiple time axes"`, `"Failed to spawn claude"`,
  `"bind HTTP listener"`. Grep the test suite before changing wording.
- **Bun-parity strings** stay stable so users grepping old logs find the
  same text the Bun CLI emitted: the harness-binary env warning
  (`resolve_bin` in `onebrain-fs/src/run_skill.rs` — wrapped, not reworded,
  by `run_skill.rs`/`harness_run.rs`), and `migrate`'s per-file warnings
  (`onebrain-fs/src/migrate.rs` — an aggregate hint is added once after the
  loop instead).

## Before / after

**A raw error dump with no next step** (`search reindex`'s hook path):

```text
# before
onebrain search reindex --lex-only: engine busy: index locked

# after
✗ Search reindex failed — couldn't open the search index: engine busy: index locked
💡 run `onebrain search status` to check the index, or `onebrain search reindex
   --force` to rebuild it (this was an automatic background reindex — nothing
   else was affected)
```

**A status line with no hint** (`daemon start` on an already-running daemon):

```text
# before
daemon already running (pid 4213)

# after
daemon already running (pid 4213) — this vault is already served
💡 run `onebrain daemon stop --vault .` first if you want to restart it
```

**A bind failure surfaced only as an OS error** (`serve`, via `HintedError`):

```text
# before
Error: bind HTTP listener on 127.0.0.1:6789: Address already in use (os error 48)

# after (text mode; --output json gets the plain first line only)
✗ Could not start the server — bind HTTP listener on 127.0.0.1:6789: Address already in use (os error 48)
💡 something else is already using that address — pick a different port with `--port <N>`, or drop `--port`/`--dir` (and `$ONEBRAIN_BIND`) so `onebrain serve` reuses or starts the per-vault daemon instead
```

## Checklist for new output

1. **Which channel?** `eprintln!` that never propagates → literal ✗/⚠/💡
   string. An `Err` that reaches `main::render_error` → `HintedError`
   (glyphs never in the anyhow message). Success output → house banner
   style; a bare `💡` hint is fine, ✗/⚠ are not.
2. **Failure or graceful degradation?** Failure → `✗`. Fail-open → `⚠`,
   and say the operation continues.
3. **Plain words** for the what, the cause after a dash, then a `💡` hint
   naming a real command (check `cli.rs`) — no trailing period, literal
   tokens backticked.
4. **Grep for load-bearing substrings** (`grep -rn "<old text>" crates/`)
   and preserve them; never break a JSON/exit-code assertion.
5. **Fix sibling instances** of the same message shape crate-wide, or note
   why not.
