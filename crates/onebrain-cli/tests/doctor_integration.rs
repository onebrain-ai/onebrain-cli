//! Layer 2 integration tests for `onebrain doctor` — exercises the wired CLI
//! end-to-end (assert_cmd spawns the real binary) against synthetic vaults
//! built in tempdirs. Verifies exit codes, stdout/stderr content, and
//! warning-vs-error distinction.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
use tempfile::tempdir;

/// Build a minimal vault that should pass every check.
///
/// Includes:
/// - `vault.yml` with all 8 folder keys + `update_channel: stable` (so the
///   `onebrain.yml-keys` check has no soft-required warning). The fixture
///   writes the legacy filename on purpose; the check reads it via fallback.
/// - All 8 vault folders.
/// - `.claude/plugins/onebrain/` with the required files + non-empty
///   `agents/` and `skills/foo/`.
/// - `.claude/settings.json` with the canonical exec-form Stop hook and the
///   `Bash(onebrain *)` permission (so `settings-hooks` reports ok).
fn write_minimal_vault(dir: &Path) {
    std::fs::write(
        dir.join("vault.yml"),
        "update_channel: stable\n\
         folders:\n  \
           inbox: 00-inbox\n  \
           projects: 01-projects\n  \
           areas: 02-areas\n  \
           knowledge: 03-knowledge\n  \
           resources: 04-resources\n  \
           agent: 05-agent\n  \
           archive: 06-archive\n  \
           logs: 07-logs\n",
    )
    .unwrap();
    for f in [
        "00-inbox",
        "01-projects",
        "02-areas",
        "03-knowledge",
        "04-resources",
        "05-agent",
        "06-archive",
        "07-logs",
    ] {
        std::fs::create_dir_all(dir.join(f)).unwrap();
    }
    let plugin = dir.join(".claude/plugins/onebrain");
    std::fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(plugin.join("agents")).unwrap();
    std::fs::create_dir_all(plugin.join("skills/foo")).unwrap();
    std::fs::write(plugin.join("INSTRUCTIONS.md"), "x").unwrap();
    std::fs::write(plugin.join(".claude-plugin/plugin.json"), "{}").unwrap();
    std::fs::write(plugin.join("agents/x.md"), "x").unwrap();
    std::fs::write(plugin.join("skills/foo/SKILL.md"), "x").unwrap();
    let settings_dir = dir.join(".claude");
    std::fs::create_dir_all(&settings_dir).unwrap();
    std::fs::write(
        settings_dir.join("settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"onebrain","args":["checkpoint","stop"]}]}]},"permissions":{"allow":["Bash(onebrain *)"]}}"#,
    )
    .unwrap();
}

#[test]
fn doctor_clean_vault_exits_0() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    // Exit code 0 means no `Error` status emerged from any check.
    // Note: this fixture has never run a reindex, so the native `search` check
    // returns Warn ("no index yet"). That keeps the exit code at 0 (advisory)
    // but means the verdict is the ⚠ glyph (warnings present, 0 fail) rather
    // than ✓. The test asserts the no-error invariant by checking that no fail
    // (`✗`) glyph row is rendered and the footer reports "0 fail" (v3.2.1
    // grouped layout).
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{2717}").not())
        .stdout(predicate::str::contains("0 fail"))
        // v3.4 content swap: the native search row is present; the old qmd
        // embeddings row is gone.
        .stdout(predicate::str::contains("search"))
        .stdout(predicate::str::contains("unembedded").not());
}

/// The native `search` check's engine path, exercised without any model
/// download: reindexing an EMPTY vault (zero `.md` docs) never constructs the
/// lazy embedder, but it does create the on-disk index and stamp
/// `last_indexed`. Doctor then opens the engine read-only and reads its
/// status:
///   1. after the empty reindex → up to date on disk but the model is absent
///      → the "model not downloaded" advisory arm;
///   2. after a note appears → the pending-drift arm ("1 pending").
///
/// `ONEBRAIN_CACHE_DIR` isolates the search cache in a tempdir so nothing
/// touches the real user cache.
#[test]
fn doctor_search_check_reads_engine_status_after_reindex() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write_minimal_vault(vault.path());
    // Pin the collection so the check and the reindex agree on the cache dir.
    let cfg = vault.path().join("vault.yml");
    let existing = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(
        &cfg,
        format!("search:\n  collection: doctor-it-engine\n{existing}"),
    )
    .unwrap();

    // 0. A cache dir that exists but was never reindexed (e.g. wiped index
    //    markers): the engine opens fresh and reports no last_indexed stamp →
    //    the "never reindexed" arm.
    std::fs::create_dir_all(cache.path().join("search/doctor-it-engine")).unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("never reindexed"));

    // Empty-vault reindex: builds the index files, downloads nothing.
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .args(["search", "reindex"])
        .assert()
        .success();

    // 1. Index exists + up to date + model absent → advisory warn.
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .arg("doctor")
        .assert()
        .success() // warn-only — exit-code contract unchanged
        .stdout(predicate::str::contains("0 indexed · model not downloaded"))
        .stdout(predicate::str::contains("onebrain search reindex"));

    // 2. A new note → pending drift reported from Engine::status.
    std::fs::write(vault.path().join("00-inbox/note.md"), "# hello\n").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("0 indexed · 1 pending"));
}

/// The all-green path: index up to date AND the embedding model present (its
/// dir is fabricated — `model_download_status` only checks the `models--*`
/// dir exists, so no download is needed). The `search` row reports `ok`, the
/// footer shows the ✅ verdict with zero warnings, and a `--fix` run finds
/// nothing to do (the `issues.is_empty()` branch).
#[test]
fn doctor_all_green_and_fix_noop_with_fake_model_dir() {
    let vault = tempdir().unwrap();
    let cache = tempdir().unwrap();
    write_minimal_vault(vault.path());
    // All-green needs the CANONICAL config filename (a legacy vault.yml would
    // trip the vault-config-migration warn).
    let legacy = vault.path().join("vault.yml");
    let existing = std::fs::read_to_string(&legacy).unwrap();
    std::fs::remove_file(&legacy).unwrap();
    std::fs::write(
        vault.path().join("onebrain.yml"),
        format!("search:\n  collection: doctor-it-green\n{existing}"),
    )
    .unwrap();

    // Empty-vault reindex (no docs → no model download) + a fabricated
    // downloaded-model dir for the default model.
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .args(["search", "reindex"])
        .assert()
        .success();
    std::fs::create_dir_all(
        cache
            .path()
            .join("search/doctor-it-green/models--intfloat--multilingual-e5-small"),
    )
    .unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("indexed · up to date"))
        .stdout(predicate::str::contains("0 warnings · 0 fail"));

    // All checks pass → --fix has nothing to do.
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("ONEBRAIN_CACHE_DIR", cache.path())
        .args(["doctor", "--fix"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Nothing to fix — all checks pass.",
        ));
}

/// Structured `--fix --json` runs every recipe without prompting and reports
/// the outcomes in the `fix[]` array — here the legacy-qmd-collection
/// migration lands as `fixed`, and the post-fix re-check feeds the final
/// `checks` array (the legacy row flips to ok).
#[test]
fn doctor_fix_json_reports_legacy_qmd_collection_outcome() {
    let vault = tempdir().unwrap();
    write_minimal_vault(vault.path());
    let cfg = vault.path().join("vault.yml");
    let existing = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, format!("qmd_collection: ob-json\n{existing}")).unwrap();

    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    let doc: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("doctor --fix --json emits one JSON document");
    let fixes = doc["fix"].as_array().expect("fix[] present");
    let legacy = fixes
        .iter()
        .find(|f| f["check"] == "legacy-qmd-collection")
        .expect("legacy-qmd-collection outcome present");
    assert_eq!(legacy["outcome"], "fixed", "outcome: {legacy}");
    // Post-fix re-check: the legacy row is now ok.
    let checks = doc["checks"].as_array().expect("checks[] present");
    let row = checks
        .iter()
        .find(|c| c["check"] == "legacy-qmd-collection")
        .expect("legacy row present");
    assert_eq!(row["status"], "ok", "row: {row}");
}

#[test]
fn doctor_missing_folder_exits_1() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    std::fs::remove_dir_all(d.path().join("01-projects")).unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .failure()
        .code(1)
        // v3.2.1 grouped layout: fail glyph `✗` on the folders row, with the
        // check's hint surfaced on the indented `└` line.
        .stdout(predicate::str::contains("\u{2717} folders"))
        .stdout(predicate::str::contains("Missing: 01-projects"));
}

#[test]
fn doctor_missing_vault_yml_errors_out() {
    let d = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not inside a vault"));
}

/// Structured mode (`--json`) outside a vault emits a JSON error envelope
/// on stdout (not stderr anyhow text) so scripts can parse it.
/// Covers the `want_structured=true` not-in-vault early return in `run()`.
#[test]
fn doctor_json_mode_not_in_vault_emits_json_error_envelope() {
    let d = tempdir().unwrap();
    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .args(["doctor", "--json"])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    let doc: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("structured mode must emit JSON on stdout");
    assert_eq!(doc["ok"], false, "ok must be false: {doc}");
    assert_eq!(doc["error"], "not_in_vault", "error field: {doc}");
}

/// v3.4: `doctor --fix` on a vault carrying the deprecated top-level
/// `qmd_collection` key migrates it to `search.collection` and removes the
/// legacy key. End-to-end proof of the `legacy-qmd-collection` check +
/// migration recipe (the old qmd-embeddings `qmd embed` recipe is gone).
/// PATH is scrubbed so no real qmd binary is ever consulted.
#[test]
fn doctor_fix_migrates_legacy_qmd_collection() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    // Add the deprecated key to the (legacy-named) config the fixture wrote.
    let cfg = d.path().join("vault.yml");
    let existing = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, format!("qmd_collection: ob-legacy\n{existing}")).unwrap();

    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    // The fix summary must appear (proves the fix pass ran). Non-TTY runs
    // (like this one) auto-proceed past the confirmation prompt.
    assert!(
        stdout.contains("Fix summary:"),
        "expected the fix summary in stdout · got: {stdout}"
    );
    // No qmd-embed recipe exists anymore — it must never spawn.
    assert!(
        !stdout.contains("running: qmd embed"),
        "qmd embed recipe should be gone · got: {stdout}"
    );

    // The config (migrated to canonical onebrain.yml by the config-migration
    // recipe) has the legacy key removed and search.collection seeded.
    let after =
        std::fs::read_to_string(d.path().join("onebrain.yml")).expect("config present after --fix");
    assert!(
        !after.contains("qmd_collection"),
        "legacy qmd_collection must be removed · got:\n{after}"
    );
    assert!(
        after.contains("collection: ob-legacy"),
        "value must be migrated to search.collection · got:\n{after}"
    );
}

#[test]
fn doctor_invalid_yaml_falls_back_to_defaults() {
    let d = tempdir().unwrap();
    std::fs::write(d.path().join("vault.yml"), "not: : valid").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .failure()
        .stdout(predicate::str::contains("invalid YAML"));
}

#[test]
fn doctor_orphan_checkpoints_warns_without_failing() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    let cp = d.path().join("07-logs/checkpoint");
    std::fs::create_dir_all(&cp).unwrap();
    std::fs::write(cp.join("2026-05-19-XXX-checkpoint-01.md"), "x").unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .success() // warning-only
        .stdout(predicate::str::contains("1 unmerged"));
}

#[test]
fn doctor_stale_marketplace_warns() {
    let d = tempdir().unwrap();
    write_minimal_vault(d.path());
    // Override settings.json with stale marketplace block (keep the canonical
    // Stop hook + permission so `settings-hooks` still reports ok).
    std::fs::write(
        d.path().join(".claude/settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"onebrain","args":["checkpoint","stop"]}]}]},"permissions":{"allow":["Bash(onebrain *)"]},"extraKnownMarketplaces":{"onebrain":{"source":{"repo":"kengio/onebrain"}}}}"#,
    )
    .unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(d.path())
        .arg("doctor")
        .assert()
        .success() // warn-only
        .stdout(predicate::str::contains("stale marketplace repo"));
}

/// Regression — `onebrain doctor --vault PATH` from outside the vault must
/// scan the supplied PATH, not the cwd. The original v3.0 / early-v3.1
/// implementation used `find_vault_root(cwd)` only, so passing `--vault`
/// from anywhere except inside the vault produced
/// `{"error":"not_in_vault",...}`. Vault resolution now goes through the
/// canonical chain (flag > env > walk-up) shared with `vault current`.
#[test]
fn doctor_honors_vault_flag() {
    let vault = tempdir().unwrap();
    write_minimal_vault(vault.path());
    // Deliberately run from a DIFFERENT directory that has no vault — if
    // the flag isn't honoured, walk-up fails and the smoke-test envelope
    // (`error: not_in_vault`) is what we'll see.
    let elsewhere = tempdir().unwrap();
    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(elsewhere.path())
        .args(["doctor", "--vault"])
        .arg(vault.path())
        .arg("--json")
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    let doc: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("doctor must emit a JSON envelope on stdout");
    // Smoke-test failure mode emitted `{"ok":false,"error":"not_in_vault",...}`
    // with no `checks` array — guard against that shape here.
    assert!(
        doc.get("error").is_none(),
        "must not be the not_in_vault envelope · got: {doc}"
    );
    assert!(
        doc.get("checks").is_some(),
        "must include the checks array · got: {doc}"
    );
    // The config check (named `onebrain.yml`) should report ok against the
    // minimal fixture — even though the fixture writes the legacy `vault.yml`
    // filename, the check reads it via fallback and reports the canonical name.
    let checks = doc["checks"].as_array().expect("checks is array");
    let vault_yml = checks
        .iter()
        .find(|c| c["check"] == "onebrain.yml")
        .expect("onebrain.yml check must be present");
    assert_eq!(vault_yml["status"], "ok", "onebrain.yml check should be ok");
}

/// Regression — `onebrain doctor --fix --vault PATH` must run the
/// `vault-config-migration` recipe against the supplied PATH (not cwd).
/// The fixture writes legacy `vault.yml`; after `--fix` the canonical
/// `onebrain.yml` should exist with the same content and `vault.yml`
/// should be gone.
#[test]
fn doctor_fix_migrates_vault_yml_with_vault_flag() {
    let vault = tempdir().unwrap();
    write_minimal_vault(vault.path());
    // Sanity: fixture writes legacy filename.
    assert!(vault.path().join("vault.yml").is_file());
    assert!(!vault.path().join("onebrain.yml").exists());
    let original = std::fs::read_to_string(vault.path().join("vault.yml")).unwrap();
    let elsewhere = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(elsewhere.path())
        // Minimal, deterministic PATH so `doctor --fix` runs against a
        // predictable environment (no developer-specific binaries leak in).
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix", "--vault"])
        .arg(vault.path())
        .assert()
        // `--fix` exit code mirrors post-fix check status; the minimal
        // fixture has no Error checks, so it returns 0.
        .success();
    // Migration recipe should have renamed legacy → canonical.
    assert!(
        vault.path().join("onebrain.yml").is_file(),
        "expected onebrain.yml at {}",
        vault.path().display()
    );
    assert!(
        !vault.path().join("vault.yml").exists(),
        "expected vault.yml to be gone after --fix"
    );
    let after = std::fs::read_to_string(vault.path().join("onebrain.yml")).unwrap();
    // The migration preserves the original content as a prefix; `doctor` then
    // stamps the run timestamps (v3.2.3) into a trailing `stats:` block — and
    // because `--fix` ran, both run and fix dates are written.
    assert!(
        after.starts_with(&original),
        "rename must preserve original content as a prefix · got:\n{after}"
    );
    assert!(
        after.contains("stats:")
            && after.contains("last_doctor_run:")
            && after.contains("last_doctor_fix:"),
        "doctor --fix must stamp last_doctor_run + last_doctor_fix · got:\n{after}"
    );
}

/// Regression — when `doctor --fix` runs both the `vault-config-migration`
/// recipe AND the `plugin-files` recipe (because the vault is missing
/// plugin files), vault-sync's "Step 7 update_vault_yml" must not
/// resurrect a legacy `vault.yml` after migration renamed it away.
///
/// Pre-fix bug: the recipes ran in declaration order — migration renamed
/// `vault.yml` → `onebrain.yml`, then plugin-files' vault-sync wrote
/// `update_channel` into a hardcoded `vault.yml` path, leaving BOTH files
/// at vault root.
///
/// Skipped on hosts without network — vault-sync downloads the upstream
/// plugin tarball. The repro runs locally via the worktree's debug binary
/// and CI has full internet.
#[test]
#[ignore = "requires network for vault-sync plugin tarball download"]
fn doctor_fix_does_not_resurrect_vault_yml_after_migration() {
    let vault = tempdir().unwrap();
    // Bare-bones legacy vault: no plugin files (forces plugin-files recipe
    // to spawn vault-sync), legacy vault.yml present (forces migration
    // recipe to run).
    std::fs::write(
        vault.path().join("vault.yml"),
        "update_channel: stable\n\
         folders:\n  \
           inbox: 00-inbox\n  \
           projects: 01-projects\n  \
           areas: 02-areas\n  \
           knowledge: 03-knowledge\n  \
           resources: 04-resources\n  \
           agent: 05-agent\n  \
           archive: 06-archive\n  \
           logs: 07-logs\n",
    )
    .unwrap();
    for f in [
        "00-inbox",
        "01-projects",
        "02-areas",
        "03-knowledge",
        "04-resources",
        "05-agent",
        "06-archive",
        "07-logs",
        ".claude",
    ] {
        std::fs::create_dir_all(vault.path().join(f)).unwrap();
    }

    let _ = Command::cargo_bin("onebrain")
        .unwrap()
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix", "--vault"])
        .arg(vault.path())
        .assert();

    assert!(
        vault.path().join("onebrain.yml").is_file(),
        "expected onebrain.yml at {}",
        vault.path().display()
    );
    assert!(
        !vault.path().join("vault.yml").exists(),
        "REGRESSION — vault-sync resurrected vault.yml after migration recipe \
         renamed it away. Step 7 must write to onebrain.yml when canonical \
         is present."
    );
}

/// Text-mode `--fix` with a vault that has only manual issues (qmd_collection
/// the native `search` "no index yet" warning is the one manual-only warning
/// the minimal canonical vault always has). Must not print "Will apply" (no
/// auto-fixable issues) and should confirm via the "manual step" path.
/// The `issues.is_empty()` branch requires a vault with ZERO Warn/Error
/// results — only possible once the search index has been built, which needs a
/// reindex (model download). We cover the manual-only path here and leave the
/// all-clean branch as residual (requires a real index).
#[test]
fn doctor_fix_text_mode_manual_issues_shows_manual_step_section() {
    let vault = tempdir().unwrap();
    // Write canonical onebrain.yml (no migration warning) with all keys present.
    std::fs::write(
        vault.path().join("onebrain.yml"),
        "update_channel: stable\n\
         folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n",
    )
    .unwrap();
    for f in [
        "00-inbox",
        "01-projects",
        "02-areas",
        "03-knowledge",
        "04-resources",
        "05-agent",
        "06-archive",
        "07-logs",
    ] {
        std::fs::create_dir_all(vault.path().join(f)).unwrap();
    }
    let plugin = vault.path().join(".claude/plugins/onebrain");
    std::fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(plugin.join("agents")).unwrap();
    std::fs::create_dir_all(plugin.join("skills/foo")).unwrap();
    std::fs::write(plugin.join("INSTRUCTIONS.md"), "x").unwrap();
    std::fs::write(plugin.join(".claude-plugin/plugin.json"), "{}").unwrap();
    std::fs::write(plugin.join("agents/x.md"), "x").unwrap();
    std::fs::write(plugin.join("skills/foo/SKILL.md"), "x").unwrap();
    let settings_dir = vault.path().join(".claude");
    std::fs::write(
        settings_dir.join("settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"onebrain","args":["checkpoint","stop"]}]}]},"permissions":{"allow":["Bash(onebrain *)"]}}"#,
    )
    .unwrap();
    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    // The native `search` warning is manual-only: "Nothing to auto-fix" appears.
    assert!(
        stdout.contains("Nothing to auto-fix") || stdout.contains("manual step"),
        "expected manual-only message · got: {stdout}"
    );
    // No auto-fix recipes were applied.
    assert!(
        !stdout.contains("Will apply"),
        "must not show auto-fix plan when only manual issues: {stdout}"
    );
}

/// Text-mode `--fix` with vault that has mixed auto+manual issues. The vault
/// uses legacy vault.yml (auto-fixable via migration) PLUS an orphan
/// checkpoint (manual-only). Both the auto plan AND manual step sections
/// must appear in the output, and the auto fix must apply.
#[test]
fn doctor_fix_text_mode_mixed_auto_and_manual_issues() {
    let vault = tempdir().unwrap();
    // Legacy vault.yml → auto-fixable (vault-config-migration recipe).
    std::fs::write(
        vault.path().join("vault.yml"),
        "update_channel: stable\n\
         folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n",
    )
    .unwrap();
    for f in [
        "00-inbox",
        "01-projects",
        "02-areas",
        "03-knowledge",
        "04-resources",
        "05-agent",
        "06-archive",
        "07-logs",
    ] {
        std::fs::create_dir_all(vault.path().join(f)).unwrap();
    }
    let plugin = vault.path().join(".claude/plugins/onebrain");
    std::fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(plugin.join("agents")).unwrap();
    std::fs::create_dir_all(plugin.join("skills/foo")).unwrap();
    std::fs::write(plugin.join("INSTRUCTIONS.md"), "x").unwrap();
    std::fs::write(plugin.join(".claude-plugin/plugin.json"), "{}").unwrap();
    std::fs::write(plugin.join("agents/x.md"), "x").unwrap();
    std::fs::write(plugin.join("skills/foo/SKILL.md"), "x").unwrap();
    let settings_dir = vault.path().join(".claude");
    std::fs::write(
        settings_dir.join("settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"onebrain","args":["checkpoint","stop"]}]}]},"permissions":{"allow":["Bash(onebrain *)"]}}"#,
    )
    .unwrap();
    // Add an orphan checkpoint — manual-only issue.
    let cp = vault.path().join("07-logs/checkpoint");
    std::fs::create_dir_all(&cp).unwrap();
    std::fs::write(cp.join("2026-05-19-XXX-checkpoint-01.md"), "x").unwrap();

    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    // Auto plan must be previewed.
    assert!(
        stdout.contains("Will apply") || stdout.contains("automated fix"),
        "expected auto-fix plan · got: {stdout}"
    );
    // Manual section must also appear.
    assert!(
        stdout.contains("manual step") || stdout.contains("wrapup"),
        "expected manual section · got: {stdout}"
    );
    // The fix summary must appear (proves auto recipes ran).
    assert!(
        stdout.contains("Fix summary:"),
        "expected Fix summary · got: {stdout}"
    );
}

/// Structured (`--fix --json`) mode with a vault that has fixable issues.
/// Must emit a single JSON document with `fix[]` array containing outcomes,
/// not text output.
#[test]
fn doctor_fix_json_mode_emits_fix_array_with_outcomes() {
    let vault = tempdir().unwrap();
    // Write legacy vault.yml (triggers vault-config-migration which is auto-fixable).
    std::fs::write(
        vault.path().join("vault.yml"),
        "update_channel: stable\n\
         folders:\n  inbox: 00-inbox\n  projects: 01-projects\n  areas: 02-areas\n  knowledge: 03-knowledge\n  resources: 04-resources\n  agent: 05-agent\n  archive: 06-archive\n  logs: 07-logs\n",
    )
    .unwrap();
    for f in [
        "00-inbox",
        "01-projects",
        "02-areas",
        "03-knowledge",
        "04-resources",
        "05-agent",
        "06-archive",
        "07-logs",
    ] {
        std::fs::create_dir_all(vault.path().join(f)).unwrap();
    }
    let plugin = vault.path().join(".claude/plugins/onebrain");
    std::fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(plugin.join("agents")).unwrap();
    std::fs::create_dir_all(plugin.join("skills/foo")).unwrap();
    std::fs::write(plugin.join("INSTRUCTIONS.md"), "x").unwrap();
    std::fs::write(plugin.join(".claude-plugin/plugin.json"), "{}").unwrap();
    std::fs::write(plugin.join("agents/x.md"), "x").unwrap();
    std::fs::write(plugin.join("skills/foo/SKILL.md"), "x").unwrap();
    let settings_dir = vault.path().join(".claude");
    std::fs::write(
        settings_dir.join("settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"onebrain","args":["checkpoint","stop"]}]}]},"permissions":{"allow":["Bash(onebrain *)"]}}"#,
    )
    .unwrap();

    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix", "--json"])
        .assert();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    let doc: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("expected JSON output · error: {e} · stdout: {stdout}"));
    // `fix[]` must be present (--fix was requested).
    assert!(doc.get("fix").is_some(), "fix key must be present: {doc}");
    let fix_arr = doc["fix"].as_array().expect("fix is array");
    // The migration recipe ran → at least one entry.
    assert!(!fix_arr.is_empty(), "fix array must have entries: {doc}");
    // Each entry has check + outcome + message.
    for entry in fix_arr {
        assert!(entry.get("check").is_some(), "entry missing check: {entry}");
        assert!(
            entry.get("outcome").is_some(),
            "entry missing outcome: {entry}"
        );
    }
}

/// CRITICAL data-safety regression: `doctor --fix` must NEVER lose config
/// keys. The vault carries a legacy `vault.yml` holding `qmd_collection` and a
/// custom key but MISSING `update_channel`, so the `vault-config-migration`
/// rename, the `legacy-qmd-collection` migration, AND the `onebrain.yml-keys`
/// backfill all fire. After --fix the config lives at canonical
/// `onebrain.yml`; the deprecated `qmd_collection` is migrated to
/// `search.collection` and its old key removed (v3.4); the unknown custom key
/// survives the re-serialization; the missing `update_channel` is backfilled;
/// and a timestamped backup was written before any destructive write.
#[test]
fn doctor_fix_preserves_custom_keys_and_backs_up() {
    let vault = tempdir().unwrap();
    write_minimal_vault(vault.path()); // folders + plugin files + (complete) vault.yml
                                       // Replace the config: keep all folders, add qmd_collection + a custom key,
                                       // drop update_channel so the keys-backfill recipe has work to do.
    std::fs::write(
        vault.path().join("vault.yml"),
        "qmd_collection: ob-1-441565\n\
         custom_key: keepme\n\
         folders:\n  \
           inbox: 00-inbox\n  \
           projects: 01-projects\n  \
           areas: 02-areas\n  \
           knowledge: 03-knowledge\n  \
           resources: 04-resources\n  \
           agent: 05-agent\n  \
           archive: 06-archive\n  \
           logs: 07-logs\n",
    )
    .unwrap();
    let elsewhere = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(elsewhere.path())
        .env("PATH", "/usr/bin:/bin") // scrub qmd so the probe degrades
        .args(["doctor", "--fix", "--vault"])
        .arg(vault.path())
        .assert()
        .success();

    let after = std::fs::read_to_string(vault.path().join("onebrain.yml"))
        .expect("config migrated to canonical onebrain.yml");
    // v3.4: the deprecated qmd_collection is migrated away, not preserved.
    assert!(
        !after.contains("qmd_collection"),
        "legacy qmd_collection must be removed by --fix · got:\n{after}"
    );
    assert!(
        after.contains("collection: ob-1-441565"),
        "qmd_collection value must be migrated to search.collection · got:\n{after}"
    );
    assert!(
        after.contains("custom_key: keepme"),
        "unknown user keys must survive --fix · got:\n{after}"
    );
    assert!(
        after.contains("update_channel"),
        "missing required key should be backfilled (onebrain.yml-keys recipe ran) · got:\n{after}"
    );
    // A backup was taken before the destructive (rename + re-serialize) writes.
    let backups = vault.path().join(".onebrain-backups");
    assert!(backups.is_dir(), "expected .onebrain-backups/ to exist");
    let count = std::fs::read_dir(&backups).unwrap().count();
    assert!(count >= 1, "expected at least one timestamped backup");
}

/// Safety + coverage for the `plugin-cache` `--fix` recipe (`fix_plugin_cache`).
/// `$HOME` is pinned to a tempdir so the destructive cache sweep operates on a
/// synthetic home and can NEVER touch the real developer cache — the inline
/// unit test this replaces called `fix_plugin_cache` directly against the live
/// `$HOME` and could delete real `~/.claude/plugins/cache` entries on every
/// `cargo test`. A stale orphan version dir is planted under the fake cache;
/// after `--fix` the recipe must report `fixed` and the orphan must be gone.
///
/// `#[cfg(unix)]`: the isolation hinges on `$HOME` steering
/// `dirs::home_dir()`, which only holds on unix. On Windows `home_dir()` reads
/// the profile known-folder (not the env), so the fake-home redirect would not
/// take and the recipe would resolve the real home — gate to the platforms
/// where the sweep provably can't escape the tempdir.
#[cfg(unix)]
#[test]
fn doctor_fix_prunes_stale_plugin_cache_under_fake_home() {
    let home = tempdir().unwrap();
    // Any version dir under `<cache>/<marketplace>/onebrain/` is a prunable
    // orphan (the active plugin is the vault-local pin, never a cache copy).
    let stale = home
        .path()
        .join(".claude/plugins/cache/test-marketplace/onebrain/2.2.4");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("plugin.json"), "{}").unwrap();
    // Registry present but empty — the unconditional `<cache>/*/onebrain/` glob
    // still discovers the orphan under the unregistered marketplace.
    std::fs::write(
        home.path().join(".claude/plugins/installed_plugins.json"),
        r#"{"plugins":{}}"#,
    )
    .unwrap();

    let vault = tempdir().unwrap();
    write_minimal_vault(vault.path());

    let assert = Command::cargo_bin("onebrain")
        .unwrap()
        .current_dir(vault.path())
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["doctor", "--fix", "--json"])
        .assert();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap_or_default();
    let doc: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("expected JSON · error: {e} · stdout: {stdout}"));
    let fix_arr = doc["fix"].as_array().expect("fix is array");
    let pc = fix_arr
        .iter()
        .find(|e| e["check"] == "plugin-cache")
        .unwrap_or_else(|| panic!("no plugin-cache fix entry · fix: {fix_arr:?}"));
    assert_eq!(
        pc["outcome"], "fixed",
        "plugin-cache recipe must report fixed · entry: {pc}"
    );
    // The sweep actually removed the orphan — proves the destructive path ran
    // against the fake home, not just that a finding was reported.
    assert!(
        !stale.exists(),
        "stale cache dir must be pruned by --fix: {}",
        stale.display()
    );
}
