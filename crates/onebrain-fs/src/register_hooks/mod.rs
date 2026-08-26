//! Idempotent `.claude/settings.json` mutation: registers OneBrain hooks +
//! permission entries. Mirrors Bun `register-hooks.ts` for the claude harness.
//!
//! Public surface:
//! - [`RegisterHooksOptions`] · CLI-supplied flags
//! - [`RegisterHooksResult`] · summary returned to caller
//! - [`HookStatus`] · added | migrated | ok
//! - [`run`] · entry point
//!
//! All JSON mutation uses `serde_json::Value` so unknown keys at any nesting
//! depth survive a read/write round trip.

mod hooks;
mod permissions;
mod qmd;
pub mod settings;

use crate::{harness::detect_harnesses, Result};
use onebrain_core::{find_vault_root, load_vault_config, Harness};
use std::path::PathBuf;

pub use hooks::HookStatus;
pub use settings::{read_settings, settings_path, write_settings};

/// CLI options forwarded from `onebrain register-hooks`.
#[derive(Debug, Clone, Default)]
pub struct RegisterHooksOptions {
    /// Vault root. Defaults to current working directory.
    pub vault_dir: Option<PathBuf>,
    /// Compute changes but do not write `settings.json` to disk.
    pub dry_run: bool,
    /// Strip all OneBrain-managed hooks + permission entries (uninstall).
    pub remove: bool,
}

/// Summary of what `run` did, for CLI printing.
///
/// `#[non_exhaustive]` — fields evolve across CLI versions (alpha.9 added
/// `direct_mode`). Out-of-tree consumers should construct via `..Default::default()`
/// pattern-style updates so a v3.0.x patch can introduce additional fields
/// without breaking downstream builds. The constructor is hidden behind
/// `run(...)` anyway; pattern-match consumers are expected to use `..` arms.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct RegisterHooksResult {
    /// True when the run completed (no parse/IO errors).
    pub ok: bool,
    /// Status for the Stop hook. None when --remove was given.
    pub stop: Option<HookStatus>,
    /// Status for the qmd PostToolUse hook. None when `qmd_collection` is absent
    /// from vault.yml (the hook is then stripped silently) or --remove was given.
    pub qmd: Option<HookStatus>,
    /// Retained for source compatibility. Always `None`: the shared Stop
    /// lifecycle runner now performs checkpoint and pending reindex together.
    pub embed: Option<HookStatus>,
    /// Permission entries appended on this run (empty on idempotent re-run).
    pub permissions_added: Vec<String>,
    /// Permission entries removed by --remove.
    pub permissions_removed: Vec<String>,
    /// True when settings.json was actually written.
    pub wrote: bool,
    /// Resolved vault directory.
    pub vault_dir: PathBuf,
    /// Whether the run targeted the claude harness (false → no-op).
    pub claude_harness: bool,
    /// True when --remove mode was used.
    pub remove_mode: bool,
    /// True when the vault is in direct mode (no harness). In this state
    /// `register-hooks` is a no-op by design: direct invocation calls the
    /// `onebrain` binary from the user's shell and needs no settings.json
    /// hook plumbing. CLI surfaces this so the user gets a clear message
    /// instead of silent success.
    pub direct_mode: bool,
}

/// Run the register-hooks workflow. Harness-aware:
/// - `Claude` → write Stop + PostToolUse hooks and 14 permissions to
///   `.claude/settings.json` (the Bun-parity behavior).
/// - `Gemini` → no-op (Bun parity — Gemini harness doesn't ship a hooks
///   integration yet).
/// - `Direct` → no-op with `direct_mode = true` on the result so the CLI
///   can emit a clear "nothing to register in direct mode" message. Direct
///   invocations call `onebrain` from the user's shell; there is no
///   harness-side settings file to mutate.
pub fn run(opts: RegisterHooksOptions) -> Result<RegisterHooksResult> {
    let vault_dir = opts
        .vault_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().expect("cwd readable"));
    let mut result = RegisterHooksResult {
        vault_dir: vault_dir.clone(),
        remove_mode: opts.remove,
        ..Default::default()
    };

    let harnesses = detect_harnesses(&vault_dir);
    // `detect_harnesses` only returns `[Direct]` as a fallback when no
    // harness directory was found AND no env override was set — Direct is
    // never combined with Claude/Gemini. So this exact-match check is
    // sufficient and the previous `Direct && !Claude` form was redundant.
    if harnesses == [Harness::Direct] {
        result.direct_mode = true;
        result.ok = true;
        return Ok(result);
    }
    if !harnesses.contains(&Harness::Claude) {
        // Gemini-only — Bun's claude branch never runs; nothing to do.
        result.ok = true;
        return Ok(result);
    }
    result.claude_harness = true;

    // Best-effort qmd_collection — missing vault.yml or unreadable config → None.
    let qmd_collection = find_vault_root(&vault_dir)
        .and_then(|root| load_vault_config(&root).ok())
        .and_then(|cfg| cfg.qmd_collection);

    let path = settings::settings_path(&vault_dir);
    let mut settings_json = settings::read_settings(&path)?;

    if opts.remove {
        let _ = qmd::strip_qmd_hook(&mut settings_json);
        let _ = qmd::strip_embed_hook(&mut settings_json);
        hooks::strip_onebrain_hooks(&mut settings_json);
        result.permissions_removed = permissions::strip_permissions(&mut settings_json);
        if !opts.dry_run {
            settings::write_settings(&path, &settings_json)?;
            result.wrote = true;
        }
        result.ok = true;
        return Ok(result);
    }

    // Stop hook + stale-event cleanup.
    result.stop = Some(hooks::apply_lifecycle_hook(&mut settings_json));

    // PostToolUse is useful only when search is configured. Stop always uses
    // the shared runner, which owns both checkpoint and pending reindex.
    if qmd_collection.is_some() {
        result.qmd = Some(qmd::apply_lifecycle_hook(&mut settings_json));
    } else {
        let _stripped = qmd::strip_lifecycle_hook(&mut settings_json);
    }

    result.permissions_added = permissions::apply_permissions(&mut settings_json);

    if !opts.dry_run {
        settings::write_settings(&path, &settings_json)?;
        result.wrote = true;
    }
    result.ok = true;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::{tempdir, TempDir};

    fn fresh_vault(claude_dir: bool, qmd_collection: Option<&str>) -> TempDir {
        let d = tempdir().unwrap();
        if claude_dir {
            fs::create_dir_all(d.path().join(".claude")).unwrap();
        }
        let yml = match qmd_collection {
            Some(c) => format!("qmd_collection: {c}\n"),
            None => "method: onebrain\n".to_string(),
        };
        fs::write(d.path().join("vault.yml"), yml).unwrap();
        d
    }

    fn read_back(vault: &std::path::Path) -> serde_json::Value {
        let text = fs::read_to_string(vault.join(".claude").join("settings.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn managed_entries<'a>(
        settings: &'a serde_json::Value,
        event: &str,
    ) -> Vec<&'a serde_json::Value> {
        settings["hooks"][event]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|group| group["hooks"].as_array())
            .flatten()
            .filter(|entry| {
                entry["command"] == "onebrain"
                    || entry["command"]
                        .as_str()
                        .is_some_and(|command| command.contains("onebrain"))
            })
            .collect()
    }

    #[test]
    fn run_fresh_uses_one_generic_runner_per_managed_event() {
        let v = fresh_vault(true, Some("ob-1-test"));

        let result = run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        let after = read_back(v.path());
        for event in ["Stop", "PostToolUse"] {
            let entries = managed_entries(&after, event);
            assert_eq!(entries.len(), 1, "{event} entries: {entries:?}");
            assert_eq!(entries[0]["command"], "onebrain");
            assert_eq!(entries[0]["args"], json!(["hook"]));
        }
        assert!(result.embed.is_none());
    }

    #[test]
    fn run_collapses_legacy_stop_entries_and_preserves_foreign_hook() {
        let v = fresh_vault(true, Some("ob-1-test"));
        let path = v.path().join(".claude/settings.json");
        fs::write(
            &path,
            serde_json::to_string(&json!({
                "hooks": {
                    "Stop": [{
                        "matcher": "",
                        "hooks": [
                            {"type": "command", "command": "onebrain", "args": ["checkpoint", "stop", "--json"], "comment": "migrate me"},
                            {"type": "command", "command": "onebrain", "args": ["search", "reindex", "--pending-only", "--json"]},
                            {"type": "command", "command": "notify-send", "args": ["done"]}
                        ]
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        let first = read_back(v.path());
        run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        let second = read_back(v.path());

        let stop = managed_entries(&second, "Stop");
        assert_eq!(stop.len(), 1, "Stop entries: {stop:?}");
        assert_eq!(stop[0]["args"], json!(["hook"]));
        assert_eq!(stop[0]["comment"], "migrate me");
        assert!(second["hooks"]["Stop"][0]["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["command"] == "notify-send"));
        assert_eq!(first, second, "second registration pass must be a no-op");
    }

    #[test]
    fn run_fresh_empty_vault_adds_stop_and_14_perms() {
        let v = fresh_vault(true, None);
        let r = run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert!(r.ok);
        assert!(r.claude_harness);
        assert_eq!(r.stop, Some(HookStatus::Added));
        assert!(r.qmd.is_none());
        assert_eq!(r.permissions_added.len(), 14);
        assert!(r.wrote);
        let after = read_back(v.path());
        assert!(after["hooks"]["Stop"].is_array());
        assert_eq!(after["permissions"]["allow"].as_array().unwrap().len(), 14);
    }

    #[test]
    fn run_idempotent_no_changes_on_second_call() {
        let v = fresh_vault(true, None);
        run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        let r2 = run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(r2.stop, Some(HookStatus::Ok));
        assert!(r2.permissions_added.is_empty());
    }

    #[test]
    fn run_dry_run_does_not_write_file() {
        let v = fresh_vault(true, None);
        let r = run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            dry_run: true,
            ..Default::default()
        })
        .unwrap();
        assert!(r.ok);
        assert!(!r.wrote);
        assert!(!v.path().join(".claude").join("settings.json").exists());
    }

    #[test]
    fn run_remove_strips_onebrain_state() {
        let v = fresh_vault(true, None);
        run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        let r = run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            remove: true,
            ..Default::default()
        })
        .unwrap();
        assert!(r.ok);
        assert!(r.remove_mode);
        assert_eq!(r.permissions_removed.len(), 14);
        let after = read_back(v.path());
        // Stop event + permissions.allow entries are gone
        assert!(after.get("hooks").is_none() || after["hooks"].get("Stop").is_none());
        let allow = after["permissions"]["allow"].as_array().unwrap();
        assert!(!allow.iter().any(|v| v.as_str() == Some("Bash(onebrain *)")));
    }

    #[test]
    fn run_qmd_collection_set_adds_post_tool_use() {
        let v = fresh_vault(true, Some("ob-1-test"));
        let r = run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(r.qmd, Some(HookStatus::Added));
        let after = read_back(v.path());
        assert!(after["hooks"]["PostToolUse"].is_array());
    }

    #[test]
    fn run_qmd_collection_absent_does_not_add_post_tool_use() {
        let v = fresh_vault(true, None);
        let r = run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert!(r.qmd.is_none());
        let after = read_back(v.path());
        assert!(after["hooks"].get("PostToolUse").is_none());
    }

    // ── v3.4.5 Track 4: Stop embed hook (search reindex --pending-only) ──────

    /// Brief test 1: fresh vault with collection configured → PostToolUse is
    /// the `--lex-only` entry, Stop has BOTH checkpoint AND embed entries.
    #[test]
    fn run_qmd_collection_set_adds_lex_only_post_tool_use_and_stop_embed() {
        let v = fresh_vault(true, Some("ob-1-test"));
        let r = run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(r.qmd, Some(HookStatus::Added));
        assert!(r.embed.is_none());
        let after = read_back(v.path());

        let post_tool_use: Vec<_> = after["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(post_tool_use.len(), 1);
        assert_eq!(post_tool_use[0]["args"], json!(["hook"]));

        let stop: Vec<_> = after["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(stop.len(), 1, "stop entries: {stop:?}");
        assert_eq!(stop[0]["args"], json!(["hook"]));
    }

    /// Brief test 2: apply twice → no duplicates for either hook.
    #[test]
    fn run_qmd_collection_set_apply_twice_no_duplicates() {
        let v = fresh_vault(true, Some("ob-1-test"));
        run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        let r2 = run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(r2.qmd, Some(HookStatus::Ok));
        assert!(r2.embed.is_none());
        let after = read_back(v.path());
        let post_tool_use = after["hooks"]["PostToolUse"][0]["hooks"]
            .as_array()
            .unwrap();
        assert_eq!(post_tool_use.len(), 1);
        let stop: Vec<_> = after["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(stop.len(), 1, "stop entries: {stop:?}");
        assert_eq!(stop[0]["args"], json!(["hook"]));
    }

    /// Brief test 3: existing settings with Track-2 PostToolUse
    /// `["search","reindex","--json"]` → migrated in place to `--lex-only`,
    /// matcher preserved, no duplicate created.
    #[test]
    fn run_track2_post_tool_use_migrates_to_lex_only_in_place() {
        let v = fresh_vault(true, Some("ob-1-test"));
        let settings_path = v.path().join(".claude").join("settings.json");
        fs::write(
            &settings_path,
            serde_json::to_string(&json!({
                "hooks": {
                    "PostToolUse": [{
                        "matcher": "Write|Edit",
                        "hooks": [{
                            "type": "command", "command": "onebrain",
                            "args": ["search", "reindex", "--json"]
                        }],
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let r = run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(r.qmd, Some(HookStatus::Migrated));
        let after = read_back(v.path());
        assert_eq!(after["hooks"]["PostToolUse"][0]["matcher"], "Write|Edit");
        let entries: Vec<_> = after["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["args"], json!(["hook"]));
    }

    /// Brief test 4: legacy `["qmd","reindex","--json"]` exec and
    /// `"onebrain qmd-reindex"` shell forms both land on the new `--lex-only`
    /// canonical.
    #[test]
    fn run_legacy_qmd_forms_land_on_lex_only_canonical() {
        let v = fresh_vault(true, Some("ob-1-test"));
        let settings_path = v.path().join(".claude").join("settings.json");
        fs::write(
            &settings_path,
            serde_json::to_string(&json!({
                "hooks": {
                    "PostToolUse": [{
                        "matcher": "Write|Edit",
                        "hooks": [{
                            "type": "command", "command": "onebrain",
                            "args": ["qmd", "reindex", "--json"]
                        }],
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        let after = read_back(v.path());
        let entries: Vec<_> = after["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1, "entries: {entries:?}");
        assert_eq!(entries[0]["args"], json!(["hook"]));

        // Shell-form legacy alias on a fresh vault.
        let v2 = fresh_vault(true, Some("ob-1-test"));
        let settings_path2 = v2.path().join(".claude").join("settings.json");
        fs::write(
            &settings_path2,
            serde_json::to_string(&json!({
                "hooks": {
                    "PostToolUse": [{
                        "matcher": "Write|Edit",
                        "hooks": [{"type": "command", "command": "onebrain qmd-reindex"}],
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        run(RegisterHooksOptions {
            vault_dir: Some(v2.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        let after2 = read_back(v2.path());
        let entries2: Vec<_> = after2["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries2.len(), 1, "entries: {entries2:?}");
        assert_eq!(entries2[0]["args"], json!(["hook"]));
    }

    /// Brief test 5: Stop embed entry is not clobbered by (PostToolUse)
    /// migration passes; the checkpoint entry is untouched byte-for-byte
    /// across a run that ALSO migrates a legacy PostToolUse form.
    #[test]
    fn run_migration_does_not_touch_checkpoint_or_embed_entries() {
        let v = fresh_vault(true, Some("ob-1-test"));
        let settings_path = v.path().join(".claude").join("settings.json");
        let checkpoint_entry = json!({
            "command": "onebrain",
            "args": ["checkpoint", "stop", "--json"],
            "type": "command",
            "comment": "do not touch me",
        });
        fs::write(
            &settings_path,
            serde_json::to_string(&json!({
                "hooks": {
                    "Stop": [{"matcher": "", "hooks": [checkpoint_entry.clone()]}],
                    "PostToolUse": [{
                        "matcher": "Write|Edit",
                        "hooks": [{"type": "command", "command": "onebrain qmd-reindex"}],
                    }],
                }
            }))
            .unwrap(),
        )
        .unwrap();
        run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        // Run again to exercise the idempotent path too.
        run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        let after = read_back(v.path());
        let stop: Vec<_> = after["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(stop.len(), 1, "stop entries: {stop:?}");
        assert_eq!(stop[0]["args"], json!(["hook"]));
        assert_eq!(stop[0]["comment"], checkpoint_entry["comment"]);
    }

    /// Brief test 6: `--remove` strips PostToolUse reindex entry AND Stop
    /// embed entry, alongside the existing checkpoint-stripping behavior.
    #[test]
    fn run_remove_strips_post_tool_use_and_stop_embed() {
        let v = fresh_vault(true, Some("ob-1-test"));
        run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        let r = run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            remove: true,
            ..Default::default()
        })
        .unwrap();
        assert!(r.ok);
        let after = read_back(v.path());
        // Existing behavior: --remove strips the whole onebrain-managed
        // hooks tree, including the checkpoint Stop entry (see
        // `run_remove_strips_onebrain_state`).
        assert!(after.get("hooks").is_none() || after["hooks"].get("Stop").is_none());
        assert!(after.get("hooks").is_none() || after["hooks"].get("PostToolUse").is_none());
    }

    /// Brief test 7: collection NOT configured → neither PostToolUse nor
    /// Stop embed entry added; pre-existing embed entry is stripped while
    /// the checkpoint entry survives.
    #[test]
    fn run_qmd_collection_absent_strips_pre_existing_embed_but_keeps_checkpoint() {
        let v = fresh_vault(true, None);
        let settings_path = v.path().join(".claude").join("settings.json");
        fs::write(
            &settings_path,
            serde_json::to_string(&json!({
                "hooks": {
                    "Stop": [
                        {"matcher": "", "hooks": [
                            {"command": "onebrain", "args": ["checkpoint", "stop", "--json"]}
                        ]},
                        {"matcher": "", "hooks": [
                            {"command": "onebrain", "args": ["search", "reindex", "--pending-only", "--json"]}
                        ]},
                    ],
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let r = run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert!(r.qmd.is_none());
        assert!(r.embed.is_none());
        let after = read_back(v.path());
        assert!(after["hooks"].get("PostToolUse").is_none());
        let stop: Vec<_> = after["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(stop.len(), 1, "stop entries: {stop:?}");
        assert_eq!(stop[0]["args"], json!(["hook"]));
    }

    #[test]
    fn run_no_claude_dir_no_op_returns_ok() {
        let v = fresh_vault(false, None);
        let r = run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert!(r.ok);
        assert!(!r.claude_harness);
        assert!(!r.wrote);
        assert!(r.stop.is_none());
    }

    #[test]
    fn run_preserves_unknown_top_level_keys() {
        let v = fresh_vault(true, None);
        let settings_path = v.path().join(".claude").join("settings.json");
        fs::write(
            &settings_path,
            serde_json::to_string(&json!({"theme": "dark", "model": "claude-sonnet"})).unwrap(),
        )
        .unwrap();
        run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        let after = read_back(v.path());
        assert_eq!(after["theme"], "dark");
        assert_eq!(after["model"], "claude-sonnet");
    }

    #[test]
    fn run_preserves_unknown_keys_inside_hook_entry() {
        let v = fresh_vault(true, None);
        let settings_path = v.path().join(".claude").join("settings.json");
        fs::write(
            &settings_path,
            serde_json::to_string(&json!({
                "hooks": {
                    "Stop": [{
                        "matcher": "",
                        "comment": "preserve me",
                        "hooks": [{
                            "command": "onebrain checkpoint stop",
                            "userMetadata": {"k": 1},
                        }],
                    }],
                }
            }))
            .unwrap(),
        )
        .unwrap();
        run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        let after = read_back(v.path());
        assert_eq!(after["hooks"]["Stop"][0]["comment"], "preserve me");
        assert_eq!(
            after["hooks"]["Stop"][0]["hooks"][0]["userMetadata"]["k"],
            1
        );
        // And the migration happened (v3.1: includes --json so machine
        // consumers keep getting JSON envelope now that text is default).
        assert_eq!(
            after["hooks"]["Stop"][0]["hooks"][0]["args"],
            json!(["hook"])
        );
    }

    #[test]
    fn run_legacy_stop_migrate_then_idempotent_ok_on_rerun() {
        let v = fresh_vault(true, None);
        let settings_path = v.path().join(".claude").join("settings.json");
        fs::write(
            &settings_path,
            serde_json::to_string(&json!({
                "hooks": {"Stop": [{"matcher": "", "hooks": [{"command": "onebrain checkpoint stop"}]}]}
            }))
            .unwrap(),
        )
        .unwrap();
        let r1 = run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(r1.stop, Some(HookStatus::Migrated));
        let r2 = run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(r2.stop, Some(HookStatus::Ok));
    }

    #[test]
    fn run_qmd_disabled_with_canonical_only_strips_post_tool_use() {
        let v = fresh_vault(true, None);
        let settings_path = v.path().join(".claude").join("settings.json");
        fs::write(
            &settings_path,
            serde_json::to_string(&json!({
                "hooks": {
                    "PostToolUse": [{
                        "matcher": "Write|Edit",
                        "hooks": [{"type": "command", "command": "onebrain qmd reindex"}],
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        let after = read_back(v.path());
        assert!(after["hooks"].get("PostToolUse").is_none());
    }

    #[test]
    fn run_qmd_set_migrates_legacy_alias_and_dedupes_to_single_new_form() {
        // Real-world bug: vault ended up with the qmd hook duplicated, one in
        // the legacy `qmd-reindex` alias form. `--fix` (= register_hooks::run)
        // must collapse to a single canonical lifecycle runner entry.
        let v = fresh_vault(true, Some("ob-1-test"));
        let settings_path = v.path().join(".claude").join("settings.json");
        fs::write(
            &settings_path,
            serde_json::to_string(&json!({
                "hooks": {
                    "PostToolUse": [
                        {"matcher": "Write|Edit", "hooks": [{
                            "type": "command", "command": "onebrain", "args": ["qmd-reindex", "--json"]
                        }]},
                        {"matcher": "Write|Edit", "hooks": [{
                            "type": "command", "command": "onebrain", "args": ["qmd", "reindex", "--json"]
                        }]},
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        let after = read_back(v.path());
        let entries: Vec<_> = after["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .collect();
        assert_eq!(entries.len(), 1, "entries: {entries:?}");
        assert_eq!(entries[0]["command"], "onebrain");
        assert_eq!(entries[0]["args"], json!(["hook"]));
    }

    #[test]
    fn task3a_fresh_install_uses_one_generic_runner_and_no_embed_result() {
        let v = fresh_vault(true, Some("ob-1-test"));
        let r = run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(r.embed, None);
        let after = read_back(v.path());
        for event in ["Stop", "PostToolUse"] {
            let entries: Vec<_> = after["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|g| g["hooks"].as_array().unwrap().iter())
                .collect();
            assert_eq!(entries.len(), 1, "{event}: {entries:?}");
            assert_eq!(
                entries[0],
                &json!({
                    "type": "command", "command": "onebrain", "args": ["hook"]
                })
            );
        }
    }

    #[test]
    fn task3a_migration_collapses_managed_entries_but_preserves_foreign_hooks() {
        let v = fresh_vault(true, Some("ob-1-test"));
        let settings_path = v.path().join(".claude").join("settings.json");
        fs::write(
            &settings_path,
            serde_json::to_string(&json!({
                "hooks": {
                    "Stop": [{"matcher": "", "hooks": [
                        {"command": "onebrain checkpoint stop", "user": "keep"},
                        {"command": "onebrain", "args": ["hook"]},
                        {"command": "onebrain", "args": ["search", "reindex", "--pending-only", "--json"]},
                        {"command": "foreign-stop"}
                    ]}],
                    "PostToolUse": [{"matcher": "Old", "hooks": [
                        {"command": "onebrain qmd-reindex"},
                        {"command": "onebrain", "args": ["search", "reindex", "--json"]},
                        {"command": "foreign-post"}
                    ]}],
                    "UserPromptSubmit": [{"matcher": "", "custom": true, "hooks": [
                        {"command": "foreign-user-custom"}
                    ]}]
                },
                "customTopLevel": {"keep": true}
            }))
            .unwrap(),
        )
        .unwrap();
        let r = run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(r.stop, Some(HookStatus::Migrated));
        assert_eq!(r.qmd, Some(HookStatus::Migrated));
        let after = read_back(v.path());
        assert_eq!(after["customTopLevel"]["keep"], true);
        assert_eq!(after["hooks"]["UserPromptSubmit"][0]["custom"], true);
        assert_eq!(
            after["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            "foreign-user-custom"
        );
        assert_eq!(
            after["hooks"]["Stop"][0]["hooks"].as_array().unwrap().len(),
            2
        );
        assert_eq!(after["hooks"]["PostToolUse"][0]["matcher"], "Write|Edit");
        assert_eq!(
            after["hooks"]["PostToolUse"][0]["hooks"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn task3a_missing_collection_removes_managed_post_tool_use_only() {
        let v = fresh_vault(true, None);
        let settings_path = v.path().join(".claude").join("settings.json");
        fs::write(
            &settings_path,
            serde_json::to_string(&json!({
                "hooks": {"PostToolUse": [{"matcher": "Write|Edit", "hooks": [
                    {"command": "onebrain", "args": ["hook"]},
                    {"command": "foreign-post"}
                ]}]}
            }))
            .unwrap(),
        )
        .unwrap();
        run(RegisterHooksOptions {
            vault_dir: Some(v.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        let after = read_back(v.path());
        assert_eq!(
            after["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "foreign-post"
        );
        assert_eq!(
            after["hooks"]["Stop"][0]["hooks"][0]["args"],
            json!(["hook"])
        );
    }
}
