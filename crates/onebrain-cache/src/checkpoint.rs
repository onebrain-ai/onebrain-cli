//! Checkpoint hook logic · handle_stop / handle_reset · state-file driven.

use std::fs;
use std::path::Path;

/// Scan `vault_root/logs_folder/checkpoint/` for files matching `{date}-{token}-checkpoint-NN.md`
/// · return the highest NN. Returns 0 if dir missing or no matching files.
pub(crate) fn max_checkpoint_nn(
    vault_root: &Path,
    logs_folder: &str,
    date: &str,
    token: &str,
) -> u32 {
    let dir = vault_root.join(logs_folder).join("checkpoint");
    let prefix = format!("{date}-{token}-checkpoint-");
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let mut max = 0u32;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        if !name.starts_with(&prefix) || !name.ends_with(".md") {
            continue;
        }
        // Extract NN: chars between "-checkpoint-" and ".md"
        let after_prefix = &name[prefix.len()..];
        let nn_str = after_prefix.trim_end_matches(".md");
        if nn_str.len() != 2 || !nn_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Ok(nn) = nn_str.parse::<u32>() {
            if nn > max {
                max = nn;
            }
        }
    }
    max
}

use crate::state::{read_state, write_state, CheckpointState};
use std::io::Write;

const SKIP_WINDOW: u64 = 60;
const MIN_ACTIVITY: u32 = 2;

/// Stop hook: increment message count, check thresholds, emit block JSON if needed.
/// Sync · always succeeds (errors are folded into safe defaults).
///
/// `stdout` is injected so tests can capture the emitted block JSON.
pub fn handle_stop(
    token: &str,
    vault_root: &Path,
    now: u64,
    tmp_dir: &Path,
    mut stdout: impl Write,
) {
    let mut state = read_state(token, tmp_dir);

    // SKIP_WINDOW: count=0 + last_ts>0 + within 60s → silent (post-reset window)
    // `now >= state.last_ts` guard: if clock went backward (NTP jump), elapsed would be
    // negative in signed arithmetic → not < 60 → must NOT skip. saturating_sub would silently
    // return 0 (< 60) and trigger skip incorrectly. Match Bun's signed-subtraction behavior.
    if state.count == 0
        && state.last_ts > 0
        && now >= state.last_ts
        && (now - state.last_ts) < SKIP_WINDOW
    {
        return;
    }

    state.count += 1;

    // Anchor the elapsed clock on the first observed stop of a session. Until
    // now last_ts stayed 0 until the first count-based block fired, so `elapsed`
    // was forced to 0 and the minutes threshold could never produce a session's
    // FIRST checkpoint — a long but low-message session (e.g. 8 turns over an
    // hour) would never snapshot. Stamping last_ts here starts the clock so the
    // time threshold can fire the first checkpoint too.
    if state.last_ts == 0 {
        state.last_ts = now;
    }

    // Load thresholds from vault.yml · fall back to defaults
    let (messages_threshold, minutes_threshold, logs_folder) =
        match onebrain_core::load_vault_config_at(vault_root) {
            Ok(cfg) => (
                cfg.checkpoint.messages,
                cfg.checkpoint.minutes,
                cfg.folders.logs.clone(),
            ),
            Err(_) => (15u32, 30u32, "07-logs".to_string()),
        };
    let time_threshold = (minutes_threshold as u64) * 60;
    // last_ts is non-zero now (anchored above). saturating_sub yields 0 on the
    // first stop (now == last_ts) and on backward clock skew (now < last_ts) —
    // both correctly suppress a time-based block this round.
    let elapsed = now.saturating_sub(state.last_ts);

    let threshold_met = state.count >= messages_threshold || elapsed >= time_threshold;

    if !threshold_met {
        write_state(token, &state, tmp_dir);
        return;
    }

    if state.count < MIN_ACTIVITY {
        // Threshold fired but not enough activity · preserve last_ts
        write_state(token, &state, tmp_dir);
        return;
    }

    // Derive NN from disk · format date from `now` in local timezone.
    // Bun uses `new Date(epochSeconds * 1000).getDate()` etc which returns Local TZ values;
    // converting to Local here keeps filename date parity for users in non-UTC timezones.
    let date = chrono::DateTime::from_timestamp(now as i64, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_else(|| "1970-01-01".to_string());
    let max_nn = max_checkpoint_nn(vault_root, &logs_folder, &date, token);
    let next_nn = format!("{:02}", max_nn + 1);
    let since = if max_nn == 0 {
        " since start".to_string()
    } else {
        format!(" since checkpoint-{:02}", max_nn)
    };
    let reason = format!("{next_nn}{since}");
    let _ = writeln!(stdout, r#"{{"decision":"block","reason":"{reason}"}}"#);

    write_state(
        token,
        &CheckpointState {
            count: 0,
            last_ts: now,
            last_stop_nn: next_nn,
        },
        tmp_dir,
    );
}

/// Reset checkpoint state · writes `{count: 0, last_ts: now, last_stop_nn: "00"}` ·
/// 3-field on-disk format `0:<now>:00`. Called by the agent after `/wrapup`.
pub fn handle_reset(token: &str, now: u64, tmp_dir: &Path) {
    write_state(
        token,
        &CheckpointState {
            count: 0,
            last_ts: now,
            last_stop_nn: "00".to_string(),
        },
        tmp_dir,
    );
}

#[cfg(test)]
mod stop_tests {
    use super::*;
    use tempfile::tempdir;

    fn make_vault(
        checkpoint_minutes: u32,
        checkpoint_messages: u32,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::write(
            root.join("vault.yml"),
            format!(
                "checkpoint:\n  minutes: {checkpoint_minutes}\n  messages: {checkpoint_messages}\n"
            ),
        )
        .unwrap();
        (dir, root)
    }

    fn capture_stdout(closure: impl FnOnce(&mut Vec<u8>)) -> String {
        let mut buf = Vec::new();
        closure(&mut buf);
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn skip_window_returns_silently_within_60s_post_reset() {
        let (_vd, vault) = make_vault(30, 15);
        let dir = tempdir().unwrap();
        write_state(
            "tok",
            &CheckpointState {
                count: 0,
                last_ts: 1_700_000_000,
                last_stop_nn: "00".to_string(),
            },
            dir.path(),
        );
        let now = 1_700_000_030; // 30s after last_ts
        let stdout = capture_stdout(|buf| handle_stop("tok", &vault, now, dir.path(), buf));
        assert_eq!(stdout, "");
        let s = read_state("tok", dir.path());
        assert_eq!(s.count, 0); // unchanged
    }

    #[test]
    fn increments_count_below_threshold() {
        let (_vd, vault) = make_vault(30, 15);
        let dir = tempdir().unwrap();
        write_state(
            "tok",
            &CheckpointState {
                count: 3,
                last_ts: 1_700_000_000,
                last_stop_nn: "00".to_string(),
            },
            dir.path(),
        );
        let stdout =
            capture_stdout(|buf| handle_stop("tok", &vault, 1_700_000_060, dir.path(), buf));
        assert_eq!(stdout, "");
        let s = read_state("tok", dir.path());
        assert_eq!(s.count, 4);
        assert_eq!(s.last_ts, 1_700_000_000); // preserved
    }

    #[test]
    fn emits_block_when_messages_threshold_met() {
        let (_vd, vault) = make_vault(30, 15);
        let dir = tempdir().unwrap();
        write_state(
            "tok",
            &CheckpointState {
                count: 14,
                last_ts: 1_700_000_000,
                last_stop_nn: "00".to_string(),
            },
            dir.path(),
        );
        let now = 1_700_001_000;
        let stdout = capture_stdout(|buf| handle_stop("tok", &vault, now, dir.path(), buf));
        assert!(stdout.contains("\"decision\":\"block\""));
        assert!(stdout.contains("\"reason\":\"01 since start\""));
        let s = read_state("tok", dir.path());
        assert_eq!(s.count, 0);
        assert_eq!(s.last_stop_nn, "01");
        assert_eq!(s.last_ts, now);
    }

    #[test]
    fn emits_block_when_minutes_threshold_met() {
        let (_vd, vault) = make_vault(30, 15);
        let dir = tempdir().unwrap();
        let last_ts = 1_700_000_000;
        let now = last_ts + 31 * 60; // > 30 min
        write_state(
            "tok",
            &CheckpointState {
                count: 5,
                last_ts,
                last_stop_nn: "00".to_string(),
            },
            dir.path(),
        );
        let stdout = capture_stdout(|buf| handle_stop("tok", &vault, now, dir.path(), buf));
        assert!(stdout.contains("\"decision\":\"block\""));
        assert!(stdout.contains("01 since start"));
    }

    #[test]
    fn min_activity_floor_prevents_block_at_count_1() {
        let (_vd, vault) = make_vault(30, 15);
        let dir = tempdir().unwrap();
        let last_ts = 1_700_000_000;
        let now = last_ts + 31 * 60;
        write_state(
            "tok",
            &CheckpointState {
                count: 0,
                last_ts,
                last_stop_nn: "00".to_string(),
            },
            dir.path(),
        );
        let stdout = capture_stdout(|buf| handle_stop("tok", &vault, now, dir.path(), buf));
        assert_eq!(stdout, "");
        let s = read_state("tok", dir.path());
        assert_eq!(s.count, 1);
        assert_eq!(s.last_ts, last_ts); // preserved
    }

    #[test]
    fn nn_derived_from_existing_files() {
        let (_vd, vault) = make_vault(30, 15);
        let dir = tempdir().unwrap();
        let now = 1_700_001_000;
        // Use Local TZ to match handle_stop's date formatting (matches Bun's Local TZ behavior).
        let date = chrono::DateTime::from_timestamp(now as i64, 0)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d")
            .to_string();
        let cp_dir = vault.join("07-logs").join("checkpoint");
        fs::create_dir_all(&cp_dir).unwrap();
        fs::write(cp_dir.join(format!("{date}-tok-checkpoint-01.md")), "x").unwrap();
        fs::write(cp_dir.join(format!("{date}-tok-checkpoint-02.md")), "x").unwrap();

        write_state(
            "tok",
            &CheckpointState {
                count: 14,
                last_ts: now - 1,
                last_stop_nn: "02".to_string(),
            },
            dir.path(),
        );
        let stdout = capture_stdout(|buf| handle_stop("tok", &vault, now, dir.path(), buf));
        assert!(stdout.contains("\"reason\":\"03 since checkpoint-02\""));
    }

    #[test]
    fn skip_window_only_applies_when_count_zero() {
        // count > 0 + within 60s = NOT skip-window · normal increment
        let (_vd, vault) = make_vault(30, 15);
        let dir = tempdir().unwrap();
        write_state(
            "tok",
            &CheckpointState {
                count: 3,
                last_ts: 1_700_000_000,
                last_stop_nn: "00".to_string(),
            },
            dir.path(),
        );
        let stdout =
            capture_stdout(|buf| handle_stop("tok", &vault, 1_700_000_010, dir.path(), buf));
        assert_eq!(stdout, "");
        let s = read_state("tok", dir.path());
        assert_eq!(s.count, 4);
    }

    #[test]
    fn skip_window_at_exact_60s_boundary_does_not_fire() {
        // Bun semantics: `now - last_ts < SKIP_WINDOW` strict less-than ·
        // age == 60 is NOT skipped · falls through to normal increment.
        let (_vd, vault) = make_vault(30, 15);
        let dir = tempdir().unwrap();
        write_state(
            "tok",
            &CheckpointState {
                count: 0,
                last_ts: 1_700_000_000,
                last_stop_nn: "00".to_string(),
            },
            dir.path(),
        );
        let now = 1_700_000_060; // exactly 60s after last_ts
        let stdout = capture_stdout(|buf| handle_stop("tok", &vault, now, dir.path(), buf));
        assert_eq!(stdout, "");
        // Below messages threshold · time elapsed exactly 60s < 30min · no block
        // But count should be incremented (count=1) since SKIP_WINDOW did NOT fire
        let s = read_state("tok", dir.path());
        assert_eq!(
            s.count, 1,
            "SKIP_WINDOW must NOT fire at exact 60s boundary"
        );
    }

    #[test]
    fn skip_window_does_not_fire_on_backward_clock_skew() {
        // NTP backward jump: now < last_ts · SKIP_WINDOW must NOT fire silently ·
        // matches Bun's signed-subtraction behavior (negative is not < 60).
        let (_vd, vault) = make_vault(30, 15);
        let dir = tempdir().unwrap();
        write_state(
            "tok",
            &CheckpointState {
                count: 0,
                last_ts: 1_700_000_100,
                last_stop_nn: "00".to_string(),
            },
            dir.path(),
        );
        let now = 1_700_000_050; // 50s BEFORE last_ts (clock went backward)
        let stdout = capture_stdout(|buf| handle_stop("tok", &vault, now, dir.path(), buf));
        assert_eq!(stdout, "");
        let s = read_state("tok", dir.path());
        assert_eq!(
            s.count, 1,
            "backward clock skew must NOT trigger SKIP_WINDOW"
        );
    }

    #[test]
    fn fresh_state_anchors_last_ts_on_first_stop() {
        // First stop of a session: elapsed is still 0 (nothing has elapsed yet),
        // but last_ts is anchored to `now` so the minutes threshold can start
        // ticking. Previously last_ts stayed 0 until the first count-based block,
        // which left the time threshold permanently dead for the first checkpoint.
        let (_vd, vault) = make_vault(30, 15);
        let dir = tempdir().unwrap();
        let now = 1_700_000_000;
        // No state file written · fresh state
        let stdout = capture_stdout(|buf| handle_stop("tok", &vault, now, dir.path(), buf));
        // count=1 after increment · below 15 · elapsed=0 below 30min · no block
        assert_eq!(stdout, "");
        let s = read_state("tok", dir.path());
        assert_eq!(s.count, 1);
        assert_eq!(s.last_ts, now); // anchored, not left at 0
    }

    #[test]
    fn time_threshold_fires_from_fresh_start_without_15_messages() {
        // Regression guard for the dead-time-threshold bug: a long but
        // low-message session must still checkpoint. First stop anchors the
        // clock; a later stop past the 30-min window fires the block even though
        // the message count never approached 15.
        let (_vd, vault) = make_vault(30, 15);
        let dir = tempdir().unwrap();
        let t0 = 1_700_000_000;
        // Stop 1 — anchors last_ts=t0, count=1, no block.
        let out1 = capture_stdout(|buf| handle_stop("tok", &vault, t0, dir.path(), buf));
        assert_eq!(out1, "");
        // Stop 2, 31 minutes later — count=2 (≥ MIN_ACTIVITY), elapsed > 30min → block.
        let out2 = capture_stdout(|buf| handle_stop("tok", &vault, t0 + 31 * 60, dir.path(), buf));
        assert!(
            out2.contains("\"decision\":\"block\""),
            "expected a time-based block on the second stop, got: {out2:?}"
        );
        assert!(out2.contains("01 since start"));
    }
}

#[cfg(test)]
mod reset_tests {
    use super::*;
    use crate::state::read_state;
    use tempfile::tempdir;

    #[test]
    fn reset_writes_count_zero_with_now_ts() {
        let dir = tempdir().unwrap();
        handle_reset("tok", 1_700_000_500, dir.path());
        let s = read_state("tok", dir.path());
        assert_eq!(s.count, 0);
        assert_eq!(s.last_ts, 1_700_000_500);
        assert_eq!(s.last_stop_nn, "00");
    }

    #[test]
    fn reset_overwrites_existing_state() {
        let dir = tempdir().unwrap();
        write_state(
            "tok",
            &CheckpointState {
                count: 7,
                last_ts: 1_000,
                last_stop_nn: "05".to_string(),
            },
            dir.path(),
        );
        handle_reset("tok", 1_700_000_500, dir.path());
        let s = read_state("tok", dir.path());
        assert_eq!(s.count, 0);
        assert_eq!(s.last_ts, 1_700_000_500);
        assert_eq!(s.last_stop_nn, "00");
    }
}

#[cfg(test)]
mod max_nn_tests {
    use super::*;
    use tempfile::tempdir;

    fn touch(dir: &Path, rel: &str) {
        let p = dir.join(rel);
        if let Some(par) = p.parent() {
            fs::create_dir_all(par).unwrap();
        }
        fs::write(p, "x").unwrap();
    }

    #[test]
    fn returns_zero_when_dir_missing() {
        let dir = tempdir().unwrap();
        assert_eq!(
            max_checkpoint_nn(dir.path(), "07-logs", "2026-05-19", "tok"),
            0
        );
    }

    #[test]
    fn returns_zero_when_no_matching_files() {
        let dir = tempdir().unwrap();
        touch(
            dir.path(),
            "07-logs/checkpoint/2026-05-19-OTHERTOK-checkpoint-01.md",
        );
        assert_eq!(
            max_checkpoint_nn(dir.path(), "07-logs", "2026-05-19", "tok"),
            0
        );
    }

    #[test]
    fn returns_max_nn_for_matching_token_and_date() {
        let dir = tempdir().unwrap();
        touch(
            dir.path(),
            "07-logs/checkpoint/2026-05-19-tok-checkpoint-01.md",
        );
        touch(
            dir.path(),
            "07-logs/checkpoint/2026-05-19-tok-checkpoint-03.md",
        );
        touch(
            dir.path(),
            "07-logs/checkpoint/2026-05-19-tok-checkpoint-02.md",
        );
        assert_eq!(
            max_checkpoint_nn(dir.path(), "07-logs", "2026-05-19", "tok"),
            3
        );
    }

    #[test]
    fn ignores_files_with_different_date() {
        let dir = tempdir().unwrap();
        touch(
            dir.path(),
            "07-logs/checkpoint/2026-05-18-tok-checkpoint-05.md",
        );
        touch(
            dir.path(),
            "07-logs/checkpoint/2026-05-19-tok-checkpoint-02.md",
        );
        assert_eq!(
            max_checkpoint_nn(dir.path(), "07-logs", "2026-05-19", "tok"),
            2
        );
    }

    #[test]
    fn respects_custom_logs_folder() {
        let dir = tempdir().unwrap();
        touch(
            dir.path(),
            "custom-logs/checkpoint/2026-05-19-tok-checkpoint-04.md",
        );
        assert_eq!(
            max_checkpoint_nn(dir.path(), "custom-logs", "2026-05-19", "tok"),
            4
        );
    }
}
