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
  Never surface a bare internal term with no gloss.
- **Every failure with a known remedy carries a `💡` hint naming the exact
  command or flag.** No dead-end errors. If there genuinely is no remedy
  beyond "try again" or "this is a bug, please report it," say that —
  don't omit the hint line.
- **Warnings use `⚠`** with the same what/why/hint shape, for things that
  degrade gracefully rather than fail the command (a browser that wouldn't
  open, a reranker that skipped, a bookkeeping write that didn't land).
- **Success output is unchanged in substance** — the existing
  emoji-section banner style (`section()` / `item()` helpers, `✓`/`◐`/`✗`
  check-result glyphs in `doctor`, etc.) stays as-is. This contract governs
  *failure and warning* text, not the happy path.
- **Every `💡` hint must reference a command/flag that actually exists.**
  Verify against the `clap` command tree (`cli.rs`) before writing one in —
  a plausible-sounding but wrong command is worse than no hint.

## Frozen surfaces — never touch these for wording

- **`--output json` / `--yaml` payloads.** The `Envelope<T>` JSON/YAML shape
  is a versioned contract for scripts and the WebUI. `text_render` callbacks
  passed to `output::emit()` only run in `OutputMode::Text` — restyling one
  never touches the structured payload (see `output/dispatcher.rs::emit`).
- **Exit codes.** A wording change must never change which `exit::exit_code_for`
  branch an error lands in.
- **Machine-parsed lines** — hook-protocol JSON (`session init`,
  `checkpoint orphans`), the `decision:"block"` shape, anything a script
  `grep`s for by a *specific* substring. When a line has an existing test
  asserting a literal substring (e.g. `"vault-sync: failed:"`,
  `"multiple time axes"`, `"Failed to spawn claude"`), preserve that
  substring inside the reworded message rather than changing the test.

If you're not sure whether a string is load-bearing, `grep` the test suite
for it before changing the wording.

## Before / after

**A raw error dump with no next step** (`search reindex`'s hook path,
`search_reindex.rs`):

```text
# before
onebrain search reindex --lex-only: engine busy: index locked

# after
✗ Search reindex failed — engine busy: index locked
💡 run `onebrain search status` to check the index, or `onebrain search
   reindex --force` to rebuild it (this was an automatic background
   reindex — nothing else was affected)
```

**A status line with no hint** (`daemon start` on an already-running
daemon, `daemon.rs`):

```text
# before
daemon already running (pid 4213)

# after
daemon already running (pid 4213) — this vault is already served
💡 run `onebrain daemon stop --vault .` first if you want to restart it
```

**A bind failure surfaced only as an OS error** (`serve`, `server/mod.rs`'s
`bind HTTP listener on {addr}` context):

```text
# before
Error: bind HTTP listener on 127.0.0.1:6789: Address already in use (os error 48)

# after
Error: ✗ Could not start the server — bind HTTP listener on 127.0.0.1:6789:
Address already in use (os error 48)
💡 something else is already using that address — pick a different port
   with `--port <N>`, or drop --port/--dir so `onebrain serve` reuses or
   starts the per-vault daemon instead
```

## Checklist for new output

Before adding an `eprintln!` / `anyhow::bail!` / `anyhow!()` in a command
handler:

1. **Is this text or structured output?** If it flows through `output::emit()`,
   only the `text_render` closure needs this treatment — the `Envelope<T>`
   fields are untouched either way.
2. **Does it fail the command, or degrade gracefully?** Failure → `✗`.
   Non-fatal / fail-open → `⚠`. Say so explicitly if the operation continues
   (a hook path that still exits 0, a warning that doesn't block).
3. **State what happened in plain words**, technical term in parens if useful.
4. **State why** — the underlying cause, usually the wrapped error.
5. **Add a `💡` hint** naming a real command or flag. Check it against
   `cli.rs` (or the relevant subcommand's `--help`) — don't invent one.
6. **Check for an existing test** asserting a substring of the old message
   (`grep -rn "<old text>" crates/*/tests crates/*/src`). Preserve the
   substring, or update the test alongside — never break a JSON/exit-code
   assertion.
7. **Check for sibling instances** of the same message shape elsewhere in
   the crate (the same raw `{e:#}` dump, the same missing-vault check under
   a different command) — fix them together so the contract doesn't drift
   between near-identical call sites.
