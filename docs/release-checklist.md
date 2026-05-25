# Pre-Ship UX Acceptance Checklist

Run before every `vX.Y.Z` tag. `cargo test` proves the unit + integration
contract; this checklist proves the **human experience**. Every entry must
be verifiable by a concrete command or script — no subjective "looks
right" lines.

## How to run

```sh
cargo build --bin onebrain
./scripts/smoke-ux.sh | tee /tmp/onebrain-smoke-$(date +%Y%m%d).log
```

Paste the resulting log into the release PR.

## Default behaviour (no flags)

- [ ] `onebrain` → banner + 3 root verbs + visible groups · matches
      snapshot `crates/onebrain-cli/src/snapshots/`
- [ ] `onebrain --help` → help screen · stub-only groups hidden (no
      `bookmark`, `daemon`, `dream`, etc.)
- [ ] `onebrain session init` (outside vault) → text starting with
      `⚠ No OneBrain vault found at …` · NOT a JSON brace
- [ ] `onebrain session init` (inside vault) → text starting with
      `Session ready · token=…`
- [ ] `onebrain checkpoint orphans . tokABC` → text `0 orphan
      checkpoints found` · NOT JSON

## Format flags · every supported mode

For each of `session init` and `checkpoint orphans .`:

- [ ] `--json` → compact JSON on a single line (verified by
      `output_format_matrix_json_flag_emits_compact_json`)
- [ ] `--json --pretty` → indented JSON with `\n` between fields
- [ ] `--yaml` → valid YAML, no leading `{`
- [ ] `--output json` → identical shape to `--json`
- [ ] `--output yaml` → identical shape to `--yaml`

## Help screens · every level

- [ ] `onebrain --help` (root)
- [ ] `onebrain session --help` (group)
- [ ] `onebrain session init --help` (leaf)
- [ ] `onebrain checkpoint --help`
- [ ] Each renders the banner once and lists the verbs/flags without
      stub commands leaking in

## Error paths

- [ ] No vault → `session init --json` returns
      `{"decision":"block","reason":"onebrain-vault-not-found"}` ·
      verified by `flow_new_user_no_vault`
- [ ] Malformed `onebrain.yml` → `session init --json` returns
      `reason: "onebrain-vault-malformed"` plus a non-empty
      `error_detail` field · verified by
      `flow_error_recovery_malformed_vault_yml`
- [ ] `onebrain notacommand` → suggests a similar verb (clap default)

## Banner & branding

- [ ] Banner renders 3-line block-shaded ASCII (verified by
      `banner::tests::emit_help_banner_writes_to_buffer_when_gated_on`)
- [ ] Tagline `Your AI Thinking Partner` present
- [ ] Banner suppressed on hook-protocol stdout (verified by
      `hook_protocol_session_init_keeps_stderr_clean`)

## Hook integration

- [ ] `onebrain plugin update --dry-run` reports `hooks_rewritten` count
      consistent with `crates/onebrain-cli/src/v31/hook_rewriter.rs`
      report shape · verified by
      `plugin_update_json_envelope_snapshot_dry_run`
- [ ] Rewriter is idempotent (running twice on the same `settings.json`
      writes the same bytes) · verified by
      `v31::hook_rewriter::tests::second_pass_is_a_no_op` +
      `idempotent_when_json_already_present` (3 tests cover every
      idempotency edge case: `--json` already present, `--output json`
      long form, `--output=json` equal form)
- [ ] Fresh-install scaffold (`onebrain init`) writes hook entries
      with `--json` baked in · verified by
      `register_hooks_fresh_install_snapshot`

## Sign-off

| Item              | Result | Notes |
| ----------------- | ------ | ----- |
| Smoke script log  |        |       |
| `cargo test`      |        |       |
| `cargo clippy`    |        |       |
| `cargo fmt`       |        |       |
| Reviewer · date   |        |       |
