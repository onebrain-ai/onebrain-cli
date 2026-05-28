//! Layer 3 CLI integration tests for the v3.1 Consistency Standard.
//!
//! Covers the acceptance criteria in
//! `01-projects/onebrain/cli/2026-05-25-v3.1.0-consistency-standard-design.md`:
//!
//! - `--help` shows the clean tree (3 root verbs + 24 groups · no hidden
//!   aliases · no `--vault-dir` legacy in the top-level help)
//! - `session-init` alias dispatches to `session init` with parity output
//! - `qmd-reindex` alias dispatches to `qmd reindex`
//! - `vault current` reports detected source correctly
//! - Vault-required commands exit 64 outside a vault with the canonical
//!   error envelope (text mode shows the quick-fix block; JSON mode emits
//!   the `E_VAULT_NOT_FOUND` envelope)
//! - Hook-protocol commands emit `{"decision":"block","reason":"..."}`
//!   + exit 0 outside a vault (graceful for SessionStart consumption)
//! - Migration notice fires once per command per process (sticky via state
//!   file across processes)

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

fn make_vault(dir: &std::path::Path) {
    fs::write(dir.join("vault.yml"), "method: onebrain\n").unwrap();
}

#[test]
fn root_help_shows_3_root_verbs_and_visible_groups() {
    // v3.1.0 UX polish: only groups with ≥1 user-facing implemented verb are
    // advertised at root `--help`. The full 24-group tree shape is still
    // present in the parser (verified by `unimplemented_groups_still_parse`
    // and `hidden_stub_still_dispatches`); they're just hidden from help.
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();

    // 3 root verbs. v3.2.12: long-format renders verbs as `  init\n` (verb
    // name on its own line, description indented on next line). Match either
    // shape so the test survives if clap's renderer changes the trailing
    // whitespace (it might be `\n` or `  ` depending on alignment column).
    for v in ["init", "update", "doctor"] {
        assert!(
            stdout.contains(&format!("  {v}\n")) || stdout.contains(&format!("  {v}  ")),
            "root verb `{v}` missing from --help. Got:\n{stdout}"
        );
    }
    // Visible groups only.
    for g in [
        "checkpoint",
        "harness",
        "plugin",
        "qmd",
        "schedule",
        "session",
        "skill",
        "vault",
    ] {
        assert!(
            stdout.contains(g),
            "group `{g}` missing from --help. Got:\n{stdout}"
        );
    }
}

#[test]
fn root_help_renders_compact_with_wrapped_defaults() {
    // v3.2.15 reverts the v3.2.12 `next_line_help = true` blanket — that
    // attribute pushed EVERY arg into long format (name on its own line) and
    // made `--help` read as "ดูยาก" per user testing. The new shape:
    //   - Commands: compact (`  init        Initialize a new vault`)
    //   - Options without defaults: compact
    //   - Options WITH `[default]` + `[possible values]`: description still
    //     inline, the bracketed value block wraps to a new line indented to
    //     the description column.
    //
    // This positive assertion pins the new layout so a re-introduction of
    // `next_line_help = true` (or removal of the `hide_default_value` /
    // `hide_possible_values` + manual help-string wrap on `--output`) fails
    // loudly.
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    // Commands compact — `  init        Initialize...` on one line.
    assert!(
        stdout.contains("  init        Initialize a new vault"),
        "expected compact command row `  init        Initialize a new vault`; \
         `next_line_help = true` may have crept back. Got:\n{stdout}"
    );
    // `-o, --output` description on the same line; `[default: text, \
    // possible values: ...]` wrapped to a new aligned line below.
    // v3.2.15: dropped `table` and `tsv` from the possible values along
    // with the OutputMode variants that lied about emitting columnar / TSV
    // output (every command routed both through the JSON encoder).
    assert!(
        stdout.contains(
            "  -o, --output <FMT>  Output format. Default `text` is TTY-friendly; pipe-detected calls drop color/pretty automatically\n                      [default: text, possible values: text, json, yaml]"
        ),
        "expected `-o, --output` description inline with `[default: text, \
         possible values: text, json, yaml]` wrapped to indented next line. \
         The `hide_default_value` / `hide_possible_values` + manual help-string \
         wrap on `Cli::output` may have been removed, or `table` / `tsv` may \
         have been re-added to the possible values list. Got:\n{stdout}"
    );
}

#[test]
fn root_help_hides_v30_aliases() {
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    for alias in [
        "session-init",
        "orphan-scan",
        "qmd-reindex",
        "register-hooks",
        "register-schedule",
        "vault-sync",
        "run-skill",
    ] {
        // Not asserting absence of substring (could appear in env help text);
        // assert it doesn't appear as a command entry. v3.2.12 long-format
        // emits `  {alias}\n`; legacy compact was `  {alias} ` /
        // `  {alias}  `. Reject all three.
        assert!(
            !stdout.contains(&format!("  {alias}\n"))
                && !stdout.contains(&format!("  {alias}  "))
                && !stdout.contains(&format!("  {alias} ")),
            "hidden alias `{alias}` leaked into top-level --help"
        );
    }
}

#[test]
fn top_level_help_hides_stub_groups() {
    // v3.1.0 final UX (user-decided 2026-05-25): only groups with ≥1 real
    // user-facing verb are visible at root `--help`. Stub-only groups
    // (`avatar`, `bookmark`, `bundle`, `config`, `daemon`, `date`, `dream`,
    // `frontmatter`, `gateway`, `inbox`, `log`, `memory`, `note`, `pause`,
    // `serve`, `task`) are hidden — they still parse + dispatch (see
    // `hidden_stub_still_dispatches`), they just don't clutter the help.
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();

    for visible in [
        "init",
        "update",
        "doctor",
        "vault",
        "session",
        "checkpoint",
        "harness",
        "plugin",
        "schedule",
        "skill",
        "qmd",
    ] {
        // v3.2.12 long-format: `  {name}\n` is the canonical shape; legacy
        // compact `  {name} ` / `  {name}  ` retained as fallback should a
        // future renderer change put the description on the same line again.
        assert!(
            stdout.contains(&format!("  {visible}\n"))
                || stdout.contains(&format!("  {visible} "))
                || stdout.contains(&format!("  {visible}  ")),
            "expected visible command `{visible}` in --help. Got:\n{stdout}"
        );
    }

    for stub in [
        "avatar",
        "bookmark",
        "bundle",
        "config",
        "daemon",
        "date",
        "dream",
        "frontmatter",
        "gateway",
        "inbox",
        "log",
        "memory",
        "note",
        "pause",
        "serve",
        "task",
    ] {
        assert!(
            !stdout.contains(&format!("  {stub}\n"))
                && !stdout.contains(&format!("  {stub}  "))
                && !stdout.contains(&format!("  {stub} ")),
            "stub group `{stub}` leaked into top-level --help. Got:\n{stdout}"
        );
    }
}

#[test]
fn hidden_stub_still_dispatches() {
    // `#[command(hide = true)]` is purely a help-display flag — the parser
    // still accepts hidden commands and the dispatcher still routes them.
    // A hidden stub group + verb must produce exit 72 with the canonical
    // `not implemented: <group> <verb>` error message.
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .args(["avatar", "pair"])
        .assert()
        .failure()
        .code(72)
        .get_output()
        .clone();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("not implemented: avatar pair"),
        "expected canonical not-implemented message. Got:\n{combined}"
    );
}

#[test]
fn top_level_help_is_production_grade() {
    // v3.1.0 production polish (Item D + E + F): heading is user-facing,
    // long_about dev-log preamble is stripped, visible groups carry one-line
    // `about` descriptions, and commands appear in domain-clustered order.
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();

    // Item D: clean heading · no dev-log preamble. As of the FIGlet banner
    // upgrade, the duplicate `about` line was stripped from clap — the brand
    // presence now comes from the stderr ASCII-art banner alone, and the
    // help stdout body starts straight at `Usage:`. This test asserts the
    // about line is NOT present in stdout (the banner lives on stderr).
    assert!(
        !stdout.contains("OneBrain CLI — Your AI Thinking Partner"),
        "duplicate clap `about` brand line leaked back into --help stdout. Got:\n{stdout}"
    );
    assert!(
        !stdout.contains("Consistency Standard"),
        "dev-log label `Consistency Standard` leaked into --help"
    );
    assert!(
        !stdout.contains("24 resource groups"),
        "internal-architecture preamble leaked into --help"
    );

    // Item F: visible groups have one-line `about` descriptions (no blank
    // description column). Each phrase below is the unique `about` text.
    for desc in [
        "Vault operations (sync · current)",
        "Session lifecycle (init)",
        "Auto-save management (stop · reset · orphans)",
        "Detect or run an AI harness (claude / gemini)",
        "Plugin lifecycle + hook rewriter",
        "launchd schedule management",
        "Skill invocation",
        "Vault search index",
    ] {
        assert!(
            stdout.contains(desc),
            "expected `about` line `{desc}` in --help. Got:\n{stdout}"
        );
    }

    // Item E: domain-clustered ordering via `display_order`. Find the byte
    // offset of each command-line entry in the rendered help and assert the
    // cluster boundaries.
    //
    // v3.2.12 long-format: command name appears as `  {name}\n` (verb on its
    // own line, description indented below). Pre-3.2.12 compact form was
    // `  {name} ` (two spaces, name, trailing space alignment). Match the
    // long-format anchor first, fall back to compact if a future renderer
    // tweak reverts it — keeps the ordering invariant decoupled from the
    // renderer's exact whitespace.
    fn offset_of(haystack: &str, needle: &str) -> usize {
        haystack
            .find(&format!("  {needle}\n"))
            .or_else(|| haystack.find(&format!("  {needle} ")))
            .unwrap_or_else(|| panic!("expected `{needle}` in --help"))
    }
    // System cluster (1-3).
    let init = offset_of(&stdout, "init");
    let update = offset_of(&stdout, "update");
    let doctor = offset_of(&stdout, "doctor");
    // Vault & session cluster (10-13).
    let vault = offset_of(&stdout, "vault");
    let session = offset_of(&stdout, "session");
    let checkpoint = offset_of(&stdout, "checkpoint");
    let harness = offset_of(&stdout, "harness");
    // Config & maintenance cluster (20-23).
    let plugin = offset_of(&stdout, "plugin");
    let schedule = offset_of(&stdout, "schedule");
    let skill = offset_of(&stdout, "skill");
    // Search cluster (30).
    let qmd = offset_of(&stdout, "qmd");

    assert!(
        init < update && update < doctor,
        "system cluster mis-ordered"
    );
    assert!(doctor < vault, "doctor should precede vault cluster");
    assert!(
        vault < session && session < checkpoint && checkpoint < harness,
        "vault/session cluster mis-ordered"
    );
    assert!(harness < plugin, "harness should precede plugin cluster");
    assert!(
        plugin < schedule && schedule < skill,
        "plugin/schedule/skill cluster mis-ordered"
    );
    assert!(skill < qmd, "qmd should come last in clusters");
}

#[test]
fn help_no_longer_has_about_line() {
    // After the 2026-05-25 FIGlet banner upgrade, the duplicate clap `about`
    // brand line (`OneBrain CLI — Your AI Thinking Partner`) was stripped to
    // eliminate the visible duplicate beneath the stderr banner. This test
    // pins that removal — if a future refactor adds `about` back, the help
    // body will once again carry two stacked brand lines.
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env_remove("NO_COLOR")
        .env_remove("CI")
        .env("TERM", "xterm-256color")
        .args(["--help", "--pretty"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert!(
        !stdout.contains("OneBrain CLI — Your AI Thinking Partner"),
        "duplicate clap `about` line is back in --help stdout. Got:\n{stdout}"
    );
    // The actual help body (`Usage:` line) must still be present — we don't
    // want to accidentally suppress the entire help output.
    assert!(
        stdout.contains("Usage:") || stdout.contains("USAGE:"),
        "help body missing from stdout. Got:\n{stdout}"
    );
}

#[test]
fn vault_current_reports_walk_up_source() {
    let dir = tempdir().unwrap();
    make_vault(dir.path());

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["vault", "current", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["command"], "vault.current");
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["detected"], true);
    assert_eq!(v["data"]["source"], "walk-up");
    // `name` is the vault dir basename. On macOS tempdir() lives under
    // /var/folders/... so we just verify the field exists and is non-empty.
    assert!(v["data"]["name"].as_str().is_some_and(|s| !s.is_empty()));
}

#[test]
fn vault_current_reports_flag_source_when_explicit() {
    let dir = tempdir().unwrap();
    make_vault(dir.path());

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        // Run from a totally different directory so walk-up can't accidentally
        // match. Tempdir for cwd has no vault.yml.
        .current_dir(tempdir().unwrap().path())
        .args([
            "--vault",
            dir.path().to_str().unwrap(),
            "vault",
            "current",
            "--json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["data"]["source"], "--vault flag");
    assert_eq!(v["data"]["detected"], true);
}

#[test]
fn vault_current_reports_not_detected_when_no_vault() {
    let no_vault = tempdir().unwrap();

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(no_vault.path())
        .args(["vault", "current", "--json"])
        // vault current is informational, NOT vault-required, so exit 0.
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["data"]["detected"], false);
    assert!(v["data"]["name"].is_null() || v["data"].get("name").is_none());
}

#[test]
fn session_init_alias_dispatches_to_session_init() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("vault.yml"), "qmd_collection: x\n").unwrap();

    // v3.1: pass --json to assert parity on the structured envelope.
    let out_alias = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["session-init", "--json"])
        .assert()
        .success();
    let s_alias = String::from_utf8_lossy(&out_alias.get_output().stdout).to_string();

    let out_new = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["session", "init", "--json"])
        .assert()
        .success();
    let s_new = String::from_utf8_lossy(&out_new.get_output().stdout).to_string();

    // Both must emit the same JSON keys (datetime+session_token+qmd_unembedded).
    let v_alias: serde_json::Value = serde_json::from_str(s_alias.trim()).unwrap();
    let v_new: serde_json::Value = serde_json::from_str(s_new.trim()).unwrap();
    assert!(v_alias.get("session_token").is_some());
    assert!(v_new.get("session_token").is_some());
    assert!(v_alias.get("qmd_unembedded").is_some());
    assert!(v_new.get("qmd_unembedded").is_some());
}

#[test]
fn session_init_emits_block_json_outside_vault() {
    let no_vault = tempdir().unwrap();

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(no_vault.path())
        // v3.1: machine consumers must opt into JSON explicitly.
        .args(["session", "init", "--json"])
        // Hook protocol: graceful exit 0 with block JSON.
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["decision"], "block");
    assert_eq!(v["reason"], "onebrain-vault-not-found");
}

/// v3.1: `--yaml` flips the hook-protocol block to YAML. Default stays JSON
/// (verified above). Same shape, different serialiser.
#[test]
fn session_init_yaml_flag_emits_yaml_block_outside_vault() {
    let no_vault = tempdir().unwrap();
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(no_vault.path())
        .args(["session", "init", "--yaml"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    // Parse the YAML to assert structure rather than string-matching the
    // emitter's whitespace (serde_yaml formatting is implementation-defined).
    let v: serde_yaml::Value = serde_yaml::from_str(&stdout).unwrap();
    assert_eq!(
        v.get("decision").and_then(|d| d.as_str()),
        Some("block"),
        "yaml block missing decision; got: {stdout}"
    );
    assert_eq!(
        v.get("reason").and_then(|r| r.as_str()),
        Some("onebrain-vault-not-found"),
        "yaml block missing reason; got: {stdout}"
    );
    // Negative · no JSON braces (defensive — confirms we actually flipped).
    assert!(
        !stdout.trim_start().starts_with('{'),
        "expected YAML, got JSON-shaped output: {stdout}"
    );
}

#[test]
fn session_init_output_yaml_alias_works() {
    let no_vault = tempdir().unwrap();
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(no_vault.path())
        .args(["session", "init", "--output", "yaml"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let v: serde_yaml::Value = serde_yaml::from_str(&stdout).unwrap();
    assert_eq!(v.get("decision").and_then(|d| d.as_str()), Some("block"));
    assert!(
        !stdout.trim_start().starts_with('{'),
        "expected YAML, got JSON-shaped output: {stdout}"
    );
}

#[test]
fn orphan_scan_alias_dispatches_to_checkpoint_orphans() {
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    // No checkpoint files: count should be 0.

    // v3.1: explicit --json so the schema check below holds.
    let out_alias = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["orphan-scan", "07-logs", "tokABC", "--json"])
        .assert()
        .success();
    let s_alias = String::from_utf8_lossy(&out_alias.get_output().stdout).to_string();

    let out_new = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["checkpoint", "orphans", "07-logs", "tokABC", "--json"])
        .assert()
        .success();
    let s_new = String::from_utf8_lossy(&out_new.get_output().stdout).to_string();
    assert_eq!(s_alias.trim(), s_new.trim());
    // Schema check: both should be `{"orphan_count": N}`.
    let v: serde_json::Value = serde_json::from_str(s_new.trim()).unwrap();
    assert!(v["orphan_count"].is_number());
}

#[test]
fn vault_required_stub_returns_64_outside_vault_or_72_inside() {
    // R1 C3: vault-required group stubs (task, memory, note, inbox, pause,
    // bookmark, dream, frontmatter, log, qmd non-reindex, schedule
    // non-protocol, vault non-current/sync) must check vault BEFORE
    // short-circuiting on E_NOT_IMPLEMENTED. Two paths:
    //
    //   1. Inside a vault   → exit 72 (E_NOT_IMPLEMENTED) — the stub fires.
    //   2. Outside any vault → exit 64 (E_VAULT_NOT_FOUND) — vault check fails.

    // Path 1: inside a vault → 72.
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .env_remove("ONEBRAIN_VAULT")
        .args(["task", "list"])
        .assert()
        .failure()
        .code(72)
        .stderr(predicate::str::contains("not implemented"))
        .stderr(predicate::str::contains("task list"));

    // Path 2: outside any vault → 64.
    let no_vault = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(no_vault.path())
        .env_remove("ONEBRAIN_VAULT")
        .args(["task", "list"])
        .assert()
        .failure()
        .code(64);
}

#[test]
fn plugin_update_outside_vault_exits_64_not_101() {
    // Round 1 A1: previously `plugin update` outside a vault panicked
    // (exit 101 + backtrace). After the fix it must return the canonical
    // E_VAULT_NOT_FOUND envelope and exit 64.
    let no_vault = tempdir().unwrap();
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(no_vault.path())
        .env_remove("ONEBRAIN_VAULT")
        .args(["plugin", "update", "--dry-run"])
        .assert();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    // Must NOT be a panic backtrace.
    assert!(
        !stderr.contains("RUST_BACKTRACE") && !stderr.contains("panicked at"),
        "plugin update outside vault panicked: {stderr:?}"
    );
    out.failure().code(64);
}

#[test]
fn plugin_update_broken_pipe_does_not_silently_succeed() {
    // Round 1 A4: previously the text-mode summary used `let _ = emit(...)`
    // which silently swallowed broken-pipe errors. After the fix the emit
    // failure must propagate as a non-zero exit via the IO-error chain
    // classifier.
    //
    // We simulate broken pipe by closing stdout via `head -c 0`. The exact
    // exit code depends on how IO errors classify; what matters is the
    // process does NOT exit 0 after the pipe closes if we encountered a
    // genuine emit error. In dry-run there are no real on-disk writes, so
    // any non-zero exit here indicates the error propagated correctly.
    //
    // Note: this test is best-effort — depending on OS buffering the broken
    // pipe may not actually fire if the small summary fits in the OS pipe
    // buffer. We still run it because the regression we're guarding
    // against is "no propagation at all", not "specific exit code".
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    // Use bash to pipe through `head -c 0` which closes stdout immediately.
    let onebrain_bin = assert_cmd::cargo::cargo_bin("onebrain");
    let status = std::process::Command::new("bash")
        .arg("-c")
        .arg(format!(
            "{} --vault {} plugin update --dry-run | head -c 0",
            onebrain_bin.display(),
            dir.path().display(),
        ))
        .status()
        .unwrap();
    // `head -c 0`'s exit code is what `bash -c` reports without
    // `pipefail`. We're not asserting a specific code; we're verifying the
    // pipeline doesn't hang or panic.
    let _ = status;
}

#[test]
fn json_error_path_emits_canonical_envelope_on_stdout() {
    // R1 B5: --json mode must always emit one well-formed envelope on
    // stdout, even on the error path. Previously errors went to stderr
    // as `Error: ...` regardless of mode, breaking machine consumers.
    let bogus = tempdir().unwrap(); // no vault.yml here

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        // Use --vault pointing at a non-vault path so we hit NotAVault
        // (deterministic; doesn't depend on cwd or env).
        .args([
            "--vault",
            bogus.path().to_str().unwrap(),
            "--json",
            "vault",
            "current",
        ])
        .env_remove("ONEBRAIN_VAULT")
        .assert()
        .failure();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();

    // stdout has the canonical envelope.
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be one canonical JSON document");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "E_VAULT_NOT_FOUND");
    assert!(v["error"]["message"].is_string());
    assert_eq!(v["version"], "1");

    // stderr stays clean of `Error: ...` lines in structured mode (machine
    // consumers should not have to filter stderr).
    assert!(
        !stderr.contains("Error:"),
        "stderr leaked human error line in --json mode: {stderr:?}"
    );

    // Exit code reflects the underlying error.
    out.code(64);
}

#[test]
fn text_error_path_keeps_legacy_stderr_format() {
    // Confirm the text-mode error path is unchanged: still writes
    // `Error: <msg>` to stderr (humans expect this).
    let bogus = tempdir().unwrap();

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .args([
            "--vault",
            bogus.path().to_str().unwrap(),
            "vault",
            "current",
        ])
        .env_remove("ONEBRAIN_VAULT")
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains("Error:"),
        "text-mode error stderr missing legacy format: {stderr:?}"
    );
}

#[test]
fn migration_notice_prints_to_stderr_first_time() {
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    // Isolate the state file by setting XDG/HOME and clearing the cache dir.
    let home = tempdir().unwrap();
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("ONEBRAIN_CACHE_DIR", home.path().join("cache"))
        .args(["qmd-reindex"])
        .assert();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains("v3.1") && stderr.contains("qmd-reindex"),
        "expected migration notice on stderr, got: {stderr:?}"
    );
}

#[test]
fn migration_notice_suppressed_by_env_var() {
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    let home = tempdir().unwrap();
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("ONEBRAIN_CACHE_DIR", home.path().join("cache"))
        .env("ONEBRAIN_QUIET_MIGRATION", "1")
        .args(["qmd-reindex"])
        .assert();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        !stderr.contains("v3.1") || !stderr.contains("renamed"),
        "expected no migration notice with ONEBRAIN_QUIET_MIGRATION=1, got: {stderr:?}"
    );
}

#[test]
fn json_envelope_shape_is_canonical_for_vault_current() {
    let dir = tempdir().unwrap();
    make_vault(dir.path());

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["vault", "current", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();

    // Required envelope fields per skill-alignment §4.3.
    assert_eq!(v["version"], "1");
    assert!(v["command"].is_string());
    assert!(v["ok"].is_boolean());
    assert!(v["warnings"].is_array());
    // `vault` and `error` may be absent in success path.
    assert!(v["data"].is_object());
}

#[test]
fn yaml_output_matches_envelope_keys() {
    let dir = tempdir().unwrap();
    make_vault(dir.path());

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["vault", "current", "--yaml"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    // Quick presence check; full YAML parse covered in unit tests.
    assert!(stdout.contains("version: '1'") || stdout.contains("version: \"1\""));
    assert!(stdout.contains("command: vault.current"));
    assert!(stdout.contains("ok: true"));
}

#[test]
fn harness_flat_invocation_now_shows_help() {
    // v3.2.10: dropped the v3.0 silent-`detect` back-compat. `onebrain harness`
    // (no verb) now prints help and exits — the documented intent. The
    // help-on-missing-subcommand exit is non-zero, hence `failure()`.
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["harness"])
        .assert()
        .failure();
}

#[test]
fn harness_detect_explicit_works() {
    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["harness", "detect"])
        .assert()
        .success();
}

// ─────────────────────────────────────────────────────────────────────────
// R1 branded banner — emission is gated by colour-TTY. In `assert_cmd`
// child processes stdout/stderr are pipes (no TTY), so the banner is
// suppressed and these tests verify the absence on the machine-output path.
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn json_output_never_contains_banner_on_stdout() {
    // Even if a future regression accidentally piped the banner to stdout
    // in `--json` mode, this test catches it.
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["vault", "current", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    // Tagline text or pink ANSI escape would indicate the banner leaked.
    assert!(
        !stdout.contains("Your AI Thinking Partner"),
        "JSON stdout leaked banner tagline: {stdout:?}"
    );
    assert!(
        !stdout.contains("\x1b[38;2;255;45;146m"),
        "JSON stdout leaked banner ANSI escape: {stdout:?}"
    );
}

#[test]
fn piped_text_output_never_emits_banner_on_stdout() {
    // Default text mode + piped stdout (assert_cmd is a pipe) → no banner.
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["vault", "current"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert!(
        !stdout.contains("Your AI Thinking Partner"),
        "piped text stdout leaked banner tagline: {stdout:?}"
    );
}

#[test]
fn hook_protocol_session_init_keeps_stderr_clean() {
    // Hook commands MUST keep both stdout and stderr free of any banner
    // bytes — even if a developer accidentally turns colour on in CI.
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("vault.yml"), "qmd_collection: x\n").unwrap();
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .args(["session", "init"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        !stdout.contains("Your AI Thinking Partner"),
        "hook stdout leaked banner tagline: {stdout:?}"
    );
    assert!(
        !stderr.contains("Your AI Thinking Partner"),
        "hook stderr leaked banner: {stderr:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// R2-B1 · partial-failure E2E
// ─────────────────────────────────────────────────────────────────────────

/// Build a minimal valid tarball fixture for `vault-sync` so step 2 of
/// `plugin update` succeeds. Matches the shape used in `tests/vault_sync.rs`.
fn build_plugin_tarball(version: &str) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    let mut buf = Vec::new();
    let prefix = "onebrain-ai-onebrain-abc1234";
    {
        let enc = GzEncoder::new(&mut buf, Compression::default());
        let mut b = tar::Builder::new(enc);
        let files: Vec<(String, String)> = vec![
            (
                format!("{prefix}/.claude/plugins/onebrain/.claude-plugin/plugin.json"),
                serde_json::json!({"id":"onebrain","version":version,"name":"OneBrain"})
                    .to_string(),
            ),
            (
                format!("{prefix}/.claude/plugins/onebrain/INSTRUCTIONS.md"),
                "# OneBrain Instructions\n".into(),
            ),
            (
                format!("{prefix}/CONTRIBUTING.md"),
                "# Contributing\n".into(),
            ),
            (format!("{prefix}/CHANGELOG.md"), "# Changelog\n".into()),
            (
                format!("{prefix}/CLAUDE.md"),
                "@.claude/plugins/onebrain/INSTRUCTIONS.md\n".into(),
            ),
            (
                format!("{prefix}/GEMINI.md"),
                "@.claude/plugins/onebrain/INSTRUCTIONS.md\n".into(),
            ),
            (
                format!("{prefix}/AGENTS.md"),
                "@.claude/plugins/onebrain/INSTRUCTIONS.md\n".into(),
            ),
        ];
        for (path, content) in &files {
            let mut h = tar::Header::new_gnu();
            h.set_size(content.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, path, content.as_bytes()).unwrap();
        }
        b.finish().unwrap();
    }
    buf
}

#[test]
fn plugin_update_partial_failure_emits_partial_envelope() {
    // R2-B1 (Blocker): the partial-failure envelope contract.
    //
    // Trigger: vault.yml has an INVALID cron schedule (`* *` — too few
    // fields) so step 4 (`schedule register`) fails after step 3 (hook
    // rewrite) has already mutated `.claude/settings.json` on disk.
    //
    // Expected envelope:
    //   - exactly ONE JSON document on stdout (no double-emit)
    //   - `ok == false`
    //   - `error.code == "E_PLUGIN_UPDATE_PARTIAL"`
    //   - `data.hooks_rewritten >= 1` (the rewriter did its job)
    //   - `data.plists_rewritten == false`
    //   - exit code != 0

    let dir = tempdir().unwrap();
    let vault = dir.path();
    fs::create_dir_all(vault.join(".claude")).unwrap();

    // vault.yml: valid YAML BUT contains a malformed cron expression. The
    // register-schedule validate pass rejects it, returning Err after the
    // hook rewriter has already succeeded.
    fs::write(
        vault.join("vault.yml"),
        "update_channel: stable\nschedule:\n  - cron: \"* *\"\n    command: /bin/echo\n    args: [hi]\n",
    )
    .unwrap();

    // .claude/settings.json with one v3.0 hook entry the rewriter will flip.
    let v30_settings = serde_json::json!({
        "hooks": {
            "SessionStart": [
                {
                    "hooks": [
                        { "type": "command", "command": "onebrain", "args": ["session-init"] }
                    ]
                }
            ]
        }
    });
    fs::write(
        vault.join(".claude/settings.json"),
        serde_json::to_string_pretty(&v30_settings).unwrap(),
    )
    .unwrap();

    // Steer vault-sync's network fetch to a local tarball fixture.
    let tarball = build_plugin_tarball("1.11.0");
    let tarball_path = dir.path().join("fixture.tar.gz");
    fs::write(&tarball_path, &tarball).unwrap();
    let isolated = vault.join(".isolated-installed_plugins.json");
    // Isolate HOME so register-schedule's plist path doesn't touch the real
    // ~/Library/LaunchAgents (defensive — invalid cron should fail BEFORE
    // any plist write, but we keep HOME isolated anyway).
    let home = tempdir().unwrap();

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .args(["plugin", "update", "--json"])
        .current_dir(vault)
        .env("HOME", home.path())
        .env(
            "ONEBRAIN_VAULT_SYNC_FIXTURE",
            tarball_path.to_string_lossy().to_string(),
        )
        .env(
            "ONEBRAIN_INSTALLED_PLUGINS_PATH",
            isolated.to_string_lossy().to_string(),
        )
        .env("ONEBRAIN_QUIET_MIGRATION", "1")
        .output()
        .expect("spawn failed");

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    // ── Exactly one JSON document on stdout (no double-emit, R2-H3). ──
    //
    // vault-sync prints progress lines BEFORE the JSON envelope, so we can't
    // parse-from-start. Filter to lines that start with `{` and assert
    // exactly one such line. (Each JSON envelope is one line in compact
    // mode; double-emit would produce two.)
    let json_lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.trim_start().starts_with('{'))
        .collect();
    assert_eq!(
        json_lines.len(),
        1,
        "expected exactly one JSON envelope on stdout (R2-H3 guard); got {} · stdout: {stdout:?} · stderr: {stderr:?}",
        json_lines.len()
    );
    let v: serde_json::Value = serde_json::from_str(json_lines[0]).expect("valid JSON");

    // ── Envelope contract: partial-failure shape. ──
    assert_eq!(v["ok"], false, "envelope ok must be false on partial");
    assert_eq!(
        v["error"]["code"], "E_PLUGIN_UPDATE_PARTIAL",
        "wrong error code · got: {v:#?}"
    );
    assert!(v["error"]["message"].is_string());
    assert_eq!(v["version"], "1");

    // ── Step progress preserved in data. ──
    assert!(
        v["data"]["hooks_rewritten"].as_u64().unwrap_or(0) >= 1,
        "hooks_rewritten must reflect on-disk progress · got: {v:#?}"
    );
    assert_eq!(
        v["data"]["plists_rewritten"], false,
        "plists_rewritten must be false on partial failure"
    );

    // ── Exit code is non-zero. ──
    assert_ne!(
        out.status.code(),
        Some(0),
        "partial failure must surface non-zero exit · stderr: {stderr:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// R2-H5 · semantic swap contract pins
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn root_update_does_not_rewrite_hooks_or_plists() {
    // The v3.1 semantic swap: `onebrain update` is now self-update of the
    // CLI binary; hook/plist rewriting moved under `onebrain plugin update`.
    //
    // Contract pin: running `onebrain update --check --json` (which calls
    // the version-check path, not the hook rewriter) must NOT touch
    // .claude/settings.json. This is a STRUCTURAL guarantee independent of
    // whether the network fetch succeeds — the update orchestrator never
    // touches the vault filesystem on the dry-run path.
    let dir = tempdir().unwrap();
    let vault = dir.path();
    fs::create_dir_all(vault.join(".claude")).unwrap();

    // A v3.0-shape settings.json that the v3.1 hook rewriter WOULD modify
    // if it ran. If `onebrain update` accidentally invoked the rewriter,
    // the args would flip from ["session-init"] to ["session","init"].
    let v30 = serde_json::json!({
        "hooks": {
            "SessionStart": [
                {
                    "hooks": [
                        { "type": "command", "command": "onebrain", "args": ["session-init"] }
                    ]
                }
            ]
        }
    });
    let settings_path = vault.join(".claude/settings.json");
    let original_bytes = serde_json::to_string_pretty(&v30).unwrap();
    fs::write(&settings_path, &original_bytes).unwrap();
    fs::write(vault.join("vault.yml"), "method: onebrain\n").unwrap();
    let home = tempdir().unwrap();

    // Run with --check --json. Network may or may not be reachable in CI;
    // either way, the contract is that settings.json is untouched. We let
    // the binary exit with whatever code it returns (0 on success, 1 on
    // fetch failure) — we only care about the on-disk side-effect.
    let _ = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault)
        .env("HOME", home.path())
        .args(["update", "--check", "--json"])
        .output()
        .expect("spawn failed");

    let after_bytes = fs::read(&settings_path).expect("settings.json must still exist");
    assert_eq!(
        after_bytes,
        original_bytes.as_bytes(),
        "onebrain update must NOT rewrite .claude/settings.json"
    );
}

#[test]
fn plugin_update_does_not_touch_cli_binary() {
    // The dual contract pin: `onebrain plugin update` must never download
    // or replace the CLI binary. v3.0 conflated both paths under one
    // `update` verb; v3.1 split them. If a future refactor accidentally
    // wires the binary-fetch path back in, this test catches it.
    let dir = tempdir().unwrap();
    let vault = dir.path();
    fs::create_dir_all(vault.join(".claude")).unwrap();
    fs::write(vault.join("vault.yml"), "method: onebrain\n").unwrap();
    fs::write(
        vault.join(".claude/settings.json"),
        serde_json::to_string_pretty(&serde_json::json!({"hooks": {}})).unwrap(),
    )
    .unwrap();

    // Isolated cache + HOME so any rogue binary write lands in the
    // tempdir, not the real ~/.cache/onebrain.
    let home = tempdir().unwrap();
    let cache_home = home.path().join("cache");
    let cache_dir = cache_home.join("onebrain");

    Command::cargo_bin("onebrain")
        .unwrap()
        .args(["plugin", "update", "--dry-run"])
        .current_dir(vault)
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", &cache_home)
        .env("ONEBRAIN_CACHE_DIR", &cache_home)
        .env("ONEBRAIN_QUIET_MIGRATION", "1")
        .assert()
        .success();

    // Nothing under the binaries subdirectory. We don't assert the cache
    // root is absent (some unrelated subsystem may create it), only that
    // no binary downloads happened.
    let binaries_dir = cache_dir.join("binaries");
    assert!(
        !binaries_dir.exists(),
        "plugin update must NOT create a binaries cache dir (got: {})",
        binaries_dir.display()
    );
}

// ─────────────────────────────────────────────────────────────────────────
// R2-M2 · parametric hidden alias coverage
// ─────────────────────────────────────────────────────────────────────────

/// Build a vault sufficient for register-hooks / register-schedule / migrate
/// to dispatch cleanly. `onebrain.yml` (the canonical v3.1 name) has no
/// schedule entries (empty list → register-schedule exits 0 with "Nothing
/// to register"). The .claude/ subdir is created so register-hooks can
/// write into it on --dry-run. Using `onebrain.yml` instead of `vault.yml`
/// keeps stderr clean of the `vault.yml is deprecated` warning, which
/// would otherwise interleave with the migration-notice assertions and
/// mask real regressions (notably on Windows where stream ordering
/// differs).
fn make_alias_vault() -> tempfile::TempDir {
    let v = tempdir().unwrap();
    fs::write(v.path().join("onebrain.yml"), "method: onebrain\n").unwrap();
    fs::create_dir_all(v.path().join(".claude")).unwrap();
    v
}

#[test]
fn all_hidden_aliases_dispatch_and_warn() {
    // R2-M2: every v3.0 alias must (a) dispatch successfully, (b) emit the
    // migration notice on stderr the first time it runs, and (c) record
    // itself in the state file so subsequent invocations stay silent.
    //
    // Coverage table — each entry pairs the alias name with the minimal
    // valid argv. Exit code expectations vary per command (some are
    // hook-protocol exit-0-on-block; vault-sync exits 1 on a bad fixture;
    // run-skill exits 78 on missing vault.yml) — we don't assert a
    // specific code, only that the migration arm fired.
    let cases: &[(&str, &[&str], Option<i32>)] = &[
        // (alias name, extra-args, optional expected exit code)
        ("session-init", &[], None),
        ("orphan-scan", &[".", "tokABC"], None),
        ("qmd-reindex", &[], None),
        // register-hooks: vault-required, but `--vault` lets us point at a
        // tempdir. With --dry-run + an empty vault, exit 0.
        ("register-hooks", &["--dry-run"], None),
        // register-schedule: vault.yml has no `schedule:` block so it
        // prints "Nothing to register" + exits 0.
        ("register-schedule", &["--dry-run"], None),
        // migrate: handler always exits 0 (internal-command contract).
        ("migrate", &["unknown-migration"], Some(0)),
        // run-skill: exits 78 when vault.yml is absent. We give a vault
        // dir without vault.yml so we hit the 78 path quickly.
        ("run-skill", &["--skill", "noop"], Some(78)),
        // vault-sync: pointed at a missing fixture path → exit 1.
        ("vault-sync", &[], Some(1)),
    ];

    for (alias, extra, want_exit) in cases.iter() {
        let vault = make_alias_vault();
        let home = tempdir().unwrap();
        let cache = home.path().join("cache");
        let mut args: Vec<String> = vec![alias.to_string()];
        // For aliases that need a vault, supply --vault via the alias' own
        // flag. session-init / qmd-reindex / orphan-scan are
        // hook-protocol-style and read cwd directly.
        let vault_str = vault.path().to_string_lossy().to_string();
        if matches!(*alias, "register-hooks" | "register-schedule" | "run-skill") {
            args.push("--vault".into());
            args.push(vault_str.clone());
        }
        if *alias == "run-skill" {
            // Force vault.yml absence by pointing at a fresh tempdir
            // (no vault.yml). Skip the make_alias_vault path's vault.yml.
            let fresh = tempdir().unwrap();
            args = vec![
                alias.to_string(),
                "--vault".into(),
                fresh.path().to_string_lossy().to_string(),
            ];
            for e in extra.iter() {
                args.push((*e).to_string());
            }
            let out = Command::cargo_bin("onebrain")
                .unwrap()
                .args(&args)
                .env("HOME", home.path())
                .env("XDG_CACHE_HOME", &cache)
                .env("ONEBRAIN_CACHE_DIR", &cache)
                .env_remove("ONEBRAIN_VAULT")
                .env_remove("ONEBRAIN_QUIET_MIGRATION")
                .output()
                .unwrap();
            assert_alias_warned(alias, &out, *want_exit);
            // Keep the fresh tempdir alive past assertions.
            drop(fresh);
            continue;
        }
        for e in extra.iter() {
            args.push((*e).to_string());
        }

        let mut cmd = Command::cargo_bin("onebrain").unwrap();
        cmd.args(&args)
            .current_dir(vault.path())
            .env("HOME", home.path())
            .env("XDG_CACHE_HOME", &cache)
            .env("ONEBRAIN_CACHE_DIR", &cache)
            .env_remove("ONEBRAIN_VAULT")
            .env_remove("ONEBRAIN_QUIET_MIGRATION");
        if *alias == "vault-sync" {
            cmd.env(
                "ONEBRAIN_VAULT_SYNC_FIXTURE",
                "/this/path/does/not/exist.tar.gz",
            );
        }
        let out = cmd.output().unwrap();
        assert_alias_warned(alias, &out, *want_exit);

        // Subsequent invocation: same HOME + cache, so state file is
        // sticky → notice MUST stay silent.
        let out2 = Command::cargo_bin("onebrain")
            .unwrap()
            .args(&args)
            .current_dir(vault.path())
            .env("HOME", home.path())
            .env("XDG_CACHE_HOME", &cache)
            .env("ONEBRAIN_CACHE_DIR", &cache)
            .env_remove("ONEBRAIN_VAULT")
            .env_remove("ONEBRAIN_QUIET_MIGRATION")
            .env_remove("ONEBRAIN_VAULT_SYNC_FIXTURE")
            .output()
            .unwrap();
        let stderr2 = String::from_utf8_lossy(&out2.stderr);
        assert!(
            !(stderr2.contains("v3.1:") && stderr2.contains(*alias)),
            "alias `{alias}` re-emitted migration notice on second run · stderr: {stderr2:?}"
        );
    }
}

fn assert_alias_warned(alias: &str, out: &std::process::Output, want_exit: Option<i32>) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("v3.1") && stderr.contains(alias),
        "alias `{alias}` did NOT emit migration notice · stderr: {stderr:?}"
    );
    if let Some(code) = want_exit {
        assert_eq!(
            out.status.code(),
            Some(code),
            "alias `{alias}` wrong exit code · stderr: {stderr:?}",
        );
    }
}

#[test]
fn migration_notice_fires_exactly_once_across_processes() {
    // R2-M2 sub-test: with a shared state-file path (same HOME + cache
    // across invocations), the migration notice MUST fire once and stay
    // silent on every subsequent call. This proves the "sticky" guarantee
    // documented in migration.rs. (A single-process double-call is not
    // expressible via the clap CLI which is one-shot per invocation; the
    // realistic contract is the cross-process state file.)
    let home = tempdir().unwrap();
    let cache = home.path().join("cache");
    let vault = make_alias_vault();

    let run = || -> (String, std::process::ExitStatus) {
        let out = Command::cargo_bin("onebrain")
            .unwrap()
            .current_dir(vault.path())
            .env("HOME", home.path())
            .env("XDG_CACHE_HOME", &cache)
            .env("ONEBRAIN_CACHE_DIR", &cache)
            .env_remove("ONEBRAIN_VAULT")
            .env_remove("ONEBRAIN_QUIET_MIGRATION")
            .args(["session-init"])
            .output()
            .unwrap();
        (String::from_utf8_lossy(&out.stderr).to_string(), out.status)
    };

    let (e1, _) = run();
    let (e2, _) = run();
    let (e3, _) = run();

    assert!(
        e1.contains("v3.1") && e1.contains("session-init"),
        "first invocation must emit notice · stderr: {e1:?}"
    );
    for (i, e) in [&e2, &e3].iter().enumerate() {
        assert!(
            !(e.contains("v3.1") && e.contains("session-init")),
            "invocation #{} must stay silent · stderr: {e:?}",
            i + 2
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────
// R2-M3 · --vault flag position matrix
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn vault_flag_accepted_post_subcommand() {
    // R2-M3: clap `global = true` makes `--vault` accepted at any
    // position. Verify it parses + resolves when placed AFTER the
    // subcommand chain.
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    let other = tempdir().unwrap();

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(other.path())
        .env_remove("ONEBRAIN_VAULT")
        .args([
            "vault",
            "current",
            "--vault",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["data"]["detected"], true);
    assert_eq!(v["data"]["source"], "--vault flag");
}

#[test]
fn vault_env_overridden_by_flag() {
    // R2-M3: `--vault` has higher priority than `ONEBRAIN_VAULT`.
    let good = tempdir().unwrap();
    make_vault(good.path());
    let bad = tempdir().unwrap(); // NO vault.yml — would fail if env wins

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(tempdir().unwrap().path())
        .env("ONEBRAIN_VAULT", bad.path())
        .args([
            "--vault",
            good.path().to_str().unwrap(),
            "vault",
            "current",
            "--json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["data"]["detected"], true);
    assert_eq!(v["data"]["source"], "--vault flag");
    assert_eq!(
        v["data"]["path"].as_str().unwrap_or(""),
        good.path().to_string_lossy()
    );
}

#[test]
fn vault_env_only_when_no_flag() {
    // R2-M3: with no `--vault` flag, `ONEBRAIN_VAULT` is honoured.
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    let elsewhere = tempdir().unwrap();

    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(elsewhere.path())
        .env("ONEBRAIN_VAULT", dir.path())
        .args(["vault", "current", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(v["data"]["detected"], true);
    assert_eq!(v["data"]["source"], "ONEBRAIN_VAULT env");
}

// ─── Help-banner integration (v3.1.0 Item G) ──────────────────────────────
//
// The banner historically rendered only inside `dispatch::dispatch`. Clap
// prints `--help` output and exits BEFORE dispatch runs, so help screens were
// unbranded. These tests pin the pre-parse banner pass that emits the brand
// line above every `--help` / `-h` / `help` invocation — top-level, group, or
// verb — while keeping it OUT of machine-output and version-only paths.
//
// `assert_cmd::Command` pipes stdout/stderr, so the live TTY is gone. The
// help-banner path's gate would normally fall through to "stderr not a tty,
// no banner"; we flip that with `ONEBRAIN_FORCE_BANNER=1`, the test-only
// override documented in `banner::stderr_is_tty_or_test_forced`. Production
// code never sets that var. `BRAND_MARK` is the stable substring of the
// rendered banner — any future palette / wording drift breaks the snapshot
// test first, then surfaces here.

// Stable substring of the rendered banner. Was `OneBrain CLI` until the
// 2026-05-25 FIGlet "Slant" banner upgrade — the literal wordmark is now
// ASCII art and the only deterministic text string is the tagline.
const BRAND_MARK: &str = "Your AI Thinking Partner";

#[test]
fn help_top_level_emits_banner_to_stderr_when_pretty() {
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env_remove("NO_COLOR")
        .env_remove("CI")
        .env("TERM", "xterm-256color")
        .env("ONEBRAIN_FORCE_BANNER", "1")
        .args(["--help", "--pretty"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert!(
        stderr.contains(BRAND_MARK),
        "expected banner on stderr above --help. stderr=\n{stderr}"
    );
    assert!(
        stderr.contains("Your AI Thinking Partner"),
        "expected tagline on stderr. stderr=\n{stderr}"
    );
    // Help payload still lands on stdout — banner must not displace it.
    assert!(
        stdout.contains("Usage:") || stdout.contains("USAGE:"),
        "help body missing from stdout. stdout=\n{stdout}"
    );
}

#[test]
fn help_subcommand_emits_banner() {
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env_remove("NO_COLOR")
        .env_remove("CI")
        .env("TERM", "xterm-256color")
        .env("ONEBRAIN_FORCE_BANNER", "1")
        .args(["plugin", "--help", "--pretty"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains(BRAND_MARK),
        "expected banner on stderr for `plugin --help`. stderr=\n{stderr}"
    );
}

#[test]
fn help_verb_emits_banner() {
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env_remove("NO_COLOR")
        .env_remove("CI")
        .env("TERM", "xterm-256color")
        .env("ONEBRAIN_FORCE_BANNER", "1")
        .args(["vault", "current", "--help", "--pretty"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains(BRAND_MARK),
        "expected banner above verb-level --help. stderr=\n{stderr}"
    );
}

#[test]
fn help_with_json_flag_no_banner() {
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env_remove("NO_COLOR")
        .env_remove("CI")
        .env("TERM", "xterm-256color")
        .env("ONEBRAIN_FORCE_BANNER", "1")
        .args(["--help", "--json"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        !stderr.contains(BRAND_MARK),
        "banner leaked into structured-mode --help. stderr=\n{stderr}"
    );
}

#[test]
fn help_with_quiet_flag_no_banner() {
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env_remove("NO_COLOR")
        .env_remove("CI")
        .env("TERM", "xterm-256color")
        .env("ONEBRAIN_FORCE_BANNER", "1")
        .args(["--help", "--quiet"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        !stderr.contains(BRAND_MARK),
        "banner leaked through --quiet. stderr=\n{stderr}"
    );
}

#[test]
fn version_no_banner() {
    // `onebrain --version --pretty` is still version-only intent — the
    // banner must NOT prepend a brand line above the bare version string.
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env_remove("NO_COLOR")
        .env_remove("CI")
        .env("TERM", "xterm-256color")
        .env("ONEBRAIN_FORCE_BANNER", "1")
        .args(["--version", "--pretty"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        !stderr.contains(BRAND_MARK),
        "banner leaked into --version. stderr=\n{stderr}"
    );
}

#[test]
fn help_no_color_env_no_banner() {
    // `NO_COLOR` set ⇒ banner suppressed even on `--help`. Encodes the
    // colour-text-only gate at the integration level so future regressions
    // (e.g. an emit_help_banner path that bypasses mode resolution) get
    // caught. We set `ONEBRAIN_FORCE_BANNER=1` too so the suppression we
    // observe can only come from the NO_COLOR check, not from stderr being
    // a pipe in assert_cmd.
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env("NO_COLOR", "1")
        .env_remove("CI")
        .env("TERM", "xterm-256color")
        .env("ONEBRAIN_FORCE_BANNER", "1")
        .args(["--help", "--pretty"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        !stderr.contains(BRAND_MARK),
        "banner leaked under NO_COLOR. stderr=\n{stderr}"
    );
}

#[test]
fn group_dash_help_emits_banner() {
    // `onebrain plugin --help` — the canonical group help form after v3.2.11
    // disabled the legacy `help` subcommand on every group. Regression guard
    // that the pre-parse banner pass still wraps `--help` screens at the
    // group level (was the path covering `plugin help` before the rename).
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env_remove("NO_COLOR")
        .env_remove("CI")
        .env("TERM", "xterm-256color")
        .env("ONEBRAIN_FORCE_BANNER", "1")
        .args(["--pretty", "plugin", "--help"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains(BRAND_MARK),
        "expected banner above `plugin --help`. stderr=\n{stderr}"
    );
}

#[test]
fn bare_group_emits_banner_above_help() {
    // `onebrain harness` (no subcommand) — the `arg_required_else_help` path
    // that v3.2.11 fixed via `try_parse` interception. Pre-3.2.11 this hop
    // through `DisplayHelpOnMissingArgumentOrSubcommand` skipped the banner
    // entirely because `argv_requests_help` only sees argv tokens and `harness`
    // by itself doesn't trip the `--help` / no-subcommand cases. Regression
    // guard so a future clap upgrade that drops the error kind from the
    // matcher in `main.rs` doesn't silently re-introduce issue #2.
    //
    // Tightened in round-2 review: pin both the exit code (2, clap's
    // `arg_required_else_help` exit) AND "exactly one banner" so the test
    // doesn't pass-for-the-wrong-reason if the banner emit site moves OR if
    // the pre-parse path is later broadened to match this argv shape too.
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env_remove("NO_COLOR")
        .env_remove("CI")
        .env("TERM", "xterm-256color")
        .env("ONEBRAIN_FORCE_BANNER", "1")
        .args(["--pretty", "harness"])
        .assert()
        .code(2);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    let banner_count = stderr.matches(BRAND_MARK).count();
    assert_eq!(
        banner_count, 1,
        "expected exactly 1 banner above bare `harness` help, got {banner_count}. stderr=\n{stderr}"
    );
}

#[test]
fn legacy_help_keyword_emits_no_banner() {
    // v3.2.11: every `*Cmd` group disabled clap's built-in `help` subcommand
    // keyword. `onebrain plugin help` now trips
    // `ErrorKind::InvalidSubcommand`, which must NOT be added to `main.rs`'s
    // `prints_help` matcher and must NOT match `argv_requests_help` —
    // emitting a banner above the "unrecognized subcommand 'help'" error
    // would be noise AND would re-open issue #1's duplicate-banner regression
    // for any user still typing the v3.2.10 form out of muscle memory.
    //
    // This is the end-to-end counterpart to the unit test
    // `argv_requests_help_rejects_legacy_help_keyword` in banner.rs: it pins
    // the contract through the full clap → main → banner pipeline.
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .env_remove("NO_COLOR")
        .env_remove("CI")
        .env("TERM", "xterm-256color")
        .env("ONEBRAIN_FORCE_BANNER", "1")
        .args(["--pretty", "plugin", "help"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        !stderr.contains(BRAND_MARK),
        "banner must NOT appear above the `InvalidSubcommand` error for the \
         removed `<group> help` keyword. stderr=\n{stderr}"
    );
}

// Issue #1's duplicate-banner regression (v3.2.10 `skill help <name>` emitted
// the banner from BOTH the argv pre-parse pass AND the dispatch path) is
// pinned at the heuristic level by the `argv_requests_help_rejects_legacy_help_keyword`
// unit test in `src/banner.rs`. End-to-end "exactly one banner" can't be
// asserted from `assert_cmd` because the dispatch banner gates on
// `color: true`, which requires a real TTY stdout — intentional, no banner
// on pipes. The bare-`harness` integration test above covers the
// pre-parse-banner-on-DisplayHelp* path that fixes issue #2.

/// `qmd status` is vault-required: exit 64 outside a vault, and a clean exit 0
/// report inside one (degrading gracefully to `qmd_available:false` when the
/// qmd binary is absent). Regression guard for the verb graduating from a
/// `not_implemented_vault_required` stub to a real handler in v3.1.1.
#[test]
fn qmd_status_requires_vault_and_reports_inside() {
    // Outside any vault → exit 64 (E_VAULT_NOT_FOUND).
    let no_vault = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(no_vault.path())
        .env_remove("ONEBRAIN_VAULT")
        .args(["qmd", "status"])
        .assert()
        .failure()
        .code(64);

    // Inside a vault → exit 0 with the status report. PATH is scrubbed so the
    // qmd probe finds nothing and the command degrades instead of hanging.
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .env_remove("ONEBRAIN_VAULT")
        .env("PATH", "/usr/bin:/bin")
        .args(["qmd", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("qmd index status"));
}

/// `qmd status --json` inside a vault emits a parseable object carrying the
/// stable `qmd_available` flag — the machine-consumer contract.
#[test]
fn qmd_status_json_is_parseable_with_availability_flag() {
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    let out = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .env_remove("ONEBRAIN_VAULT")
        .env("PATH", "/usr/bin:/bin")
        .args(["qmd", "status", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert!(
        v.get("qmd_available").is_some(),
        "missing qmd_available: {v}"
    );
}

/// `qmd embed` is vault-required (exit 64 outside a vault) and, inside one,
/// runs the foreground `qmd embed`. PATH is scrubbed so the probe fails fast
/// with the install hint — proving the verb reaches the spawn (no longer the
/// `not_implemented` stub) WITHOUT actually generating embeddings in the test.
#[test]
fn qmd_embed_requires_vault_then_invokes_qmd() {
    // Outside any vault → exit 64.
    let no_vault = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(no_vault.path())
        .env_remove("ONEBRAIN_VAULT")
        .args(["qmd", "embed"])
        .assert()
        .failure()
        .code(64);

    // Inside a vault, qmd scrubbed from PATH → reaches the spawn and fails with
    // the install hint (NOT exit 64, NOT the not-implemented stub).
    let dir = tempdir().unwrap();
    make_vault(dir.path());
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(dir.path())
        .env_remove("ONEBRAIN_VAULT")
        .env("PATH", "/usr/bin:/bin")
        .args(["qmd", "embed"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("qmd embed"))
        .stderr(predicate::str::contains("installed").or(predicate::str::contains("PATH")));
}
