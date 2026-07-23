mod support;

#[cfg(unix)]
use assert_cmd::Command;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

#[cfg(unix)]
fn vault() -> tempfile::TempDir {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("onebrain.yml"), "folders: {}\n").unwrap();
    dir
}

#[cfg(unix)]
#[test]
fn managed_install_writes_marker_config_and_expected_argv() {
    let vault = vault();
    let home = tempdir().unwrap();
    let bin = home.path().join("codex");
    fs::write(
        &bin,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CODEX_ARGV_LOG\"\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let log = home.path().join("argv.log");

    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("CODEX_BIN", &bin)
        .env("CODEX_HOME", home.path())
        .env("CODEX_ARGV_LOG", &log)
        .args([
            "--vault",
            vault.path().to_str().unwrap(),
            "plugin",
            "install",
            "--harness",
            "codex",
        ])
        .assert()
        .success();

    let argv = fs::read_to_string(log).unwrap();
    assert!(argv.contains("plugin marketplace add"));
    assert!(argv.contains("plugin add onebrain@onebrain"));
    assert!(vault.path().join(".codex/onebrain-plugin.json").is_file());
    assert_eq!(
        fs::read_dir(home.path().join("onebrain-managed"))
            .unwrap()
            .count(),
        1
    );
    let config = fs::read_to_string(home.path().join("config.toml")).unwrap();
    assert!(config.contains("hooks = true"));
    assert!(config.contains("multi_agent = true"));
}

#[cfg(unix)]
#[test]
fn managed_install_dry_run_touches_no_codex_state() {
    let vault = vault();
    let home = tempdir().unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("CODEX_HOME", home.path())
        .args([
            "--vault",
            vault.path().to_str().unwrap(),
            "plugin",
            "install",
            "--harness",
            "codex",
            "--dry-run",
        ])
        .assert()
        .success();
    assert!(!home.path().join("config.toml").exists());
    assert!(!vault.path().join(".codex/onebrain-plugin.json").exists());
}

#[cfg(unix)]
#[test]
fn managed_uninstall_removes_plugin_and_marker() {
    let vault = vault();
    let home = tempdir().unwrap();
    let bin = home.path().join("codex");
    fs::write(
        &bin,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CODEX_ARGV_LOG\"\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let log = home.path().join("argv.log");
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("CODEX_BIN", &bin)
        .env("CODEX_HOME", home.path())
        .env("CODEX_ARGV_LOG", &log)
        .args([
            "--vault",
            vault.path().to_str().unwrap(),
            "plugin",
            "install",
            "--harness",
            "codex",
        ])
        .assert()
        .success();
    fs::write(&log, "").unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("CODEX_BIN", &bin)
        .env("CODEX_HOME", home.path())
        .env("CODEX_ARGV_LOG", &log)
        .args([
            "--vault",
            vault.path().to_str().unwrap(),
            "plugin",
            "uninstall",
            "--harness",
            "codex",
        ])
        .assert()
        .success();

    assert_eq!(
        fs::read_to_string(log).unwrap(),
        "plugin remove onebrain@onebrain\n"
    );
    assert!(!vault.path().join(".codex/onebrain-plugin.json").exists());
    assert_eq!(
        fs::read_dir(home.path().join("onebrain-managed"))
            .unwrap()
            .count(),
        0
    );
}

#[cfg(unix)]
#[test]
fn managed_uninstall_dry_run_preserves_marker_and_global_state() {
    let vault = vault();
    let home = tempdir().unwrap();
    let bin = home.path().join("codex");
    fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("CODEX_BIN", &bin)
        .env("CODEX_HOME", home.path())
        .args([
            "--vault",
            vault.path().to_str().unwrap(),
            "plugin",
            "install",
            "--harness",
            "codex",
        ])
        .assert()
        .success();
    let marker = vault.path().join(".codex/onebrain-plugin.json");

    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("CODEX_BIN", home.path().join("must-not-run"))
        .env("CODEX_HOME", home.path())
        .args([
            "--vault",
            vault.path().to_str().unwrap(),
            "plugin",
            "uninstall",
            "--harness",
            "codex",
            "--dry-run",
        ])
        .assert()
        .success();

    assert!(marker.exists());
}

#[cfg(unix)]
#[test]
fn failed_plugin_add_rolls_back_config_marker_and_receipt() {
    let vault = vault();
    let home = tempdir().unwrap();
    let bin = home.path().join("codex");
    fs::write(
        &bin,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CODEX_ARGV_LOG\"\ncase \"$*\" in\n  \"plugin add onebrain@onebrain\") exit 17 ;;\nesac\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(home.path().join("config.toml"), "model = \"gpt-5\"\n").unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("CODEX_BIN", &bin)
        .env("CODEX_HOME", home.path())
        .env("CODEX_ARGV_LOG", home.path().join("argv.log"))
        .args([
            "--vault",
            vault.path().to_str().unwrap(),
            "plugin",
            "install",
            "--harness",
            "codex",
        ])
        .assert()
        .code(17);

    assert_eq!(
        fs::read_to_string(home.path().join("config.toml")).unwrap(),
        "model = \"gpt-5\"\n"
    );
    assert!(!vault.path().join(".codex/onebrain-plugin.json").exists());
    assert!(
        !home.path().join("onebrain-managed").exists()
            || fs::read_dir(home.path().join("onebrain-managed"))
                .unwrap()
                .next()
                .is_none()
    );
    assert!(
        !fs::read_to_string(home.path().join("argv.log"))
            .unwrap()
            .contains("plugin remove"),
        "a failed add must not remove a pre-existing manual plugin"
    );
}

#[cfg(unix)]
#[test]
fn unattended_hook_trust_requires_matching_global_receipt() {
    let vault = vault();
    let home = tempdir().unwrap();
    let bin = home.path().join("codex");
    let log = home.path().join("argv.log");
    fs::write(
        &bin,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CODEX_ARGV_LOG\"\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir_all(vault.path().join(".codex")).unwrap();
    fs::write(
        vault.path().join(".codex/onebrain-plugin.json"),
        r#"{"managed":true,"plugin":"onebrain@onebrain"}"#,
    )
    .unwrap();

    let run_skill = || {
        Command::cargo_bin("onebrain")
            .unwrap()
            .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
            .env("CODEX_BIN", &bin)
            .env("CODEX_HOME", home.path())
            .env("CODEX_ARGV_LOG", &log)
            .args([
                "--vault",
                vault.path().to_str().unwrap(),
                "skill",
                "run",
                "daily",
                "--harness",
                "codex",
            ])
            .assert()
            .success();
    };

    run_skill();
    assert!(!fs::read_to_string(&log)
        .unwrap()
        .contains("--dangerously-bypass-hook-trust"));

    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("CODEX_BIN", &bin)
        .env("CODEX_HOME", home.path())
        .env("CODEX_ARGV_LOG", &log)
        .args([
            "--vault",
            vault.path().to_str().unwrap(),
            "plugin",
            "install",
            "--harness",
            "codex",
        ])
        .assert()
        .success();
    fs::write(&log, "").unwrap();

    run_skill();
    assert!(fs::read_to_string(log)
        .unwrap()
        .contains("exec --dangerously-bypass-hook-trust"));
}

#[cfg(unix)]
#[test]
fn install_preserves_private_config_permissions_for_config_and_backup() {
    let vault = vault();
    let home = tempdir().unwrap();
    let bin = home.path().join("codex");
    fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let config = home.path().join("config.toml");
    fs::write(&config, "model = \"gpt-5\"\n").unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("CODEX_BIN", &bin)
        .env("CODEX_HOME", home.path())
        .args([
            "--vault",
            vault.path().to_str().unwrap(),
            "plugin",
            "install",
            "--harness",
            "codex",
        ])
        .assert()
        .success();

    assert_eq!(
        fs::metadata(&config).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(home.path().join("config.toml.onebrain.bak"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn pending_receipt_failure_restores_config_before_plugin_add() {
    let vault = vault();
    let home = tempdir().unwrap();
    let bin = home.path().join("codex");
    let log = home.path().join("argv.log");
    fs::write(
        &bin,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CODEX_ARGV_LOG\"\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(home.path().join("config.toml"), "model = \"gpt-5\"\n").unwrap();
    fs::write(home.path().join("onebrain-managed"), "not a directory").unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("CODEX_BIN", &bin)
        .env("CODEX_HOME", home.path())
        .env("CODEX_ARGV_LOG", &log)
        .args([
            "--vault",
            vault.path().to_str().unwrap(),
            "plugin",
            "install",
            "--harness",
            "codex",
        ])
        .assert()
        .failure();

    assert_eq!(
        fs::read_to_string(home.path().join("config.toml")).unwrap(),
        "model = \"gpt-5\"\n"
    );
    assert!(!fs::read_to_string(log)
        .unwrap()
        .contains("plugin add onebrain@onebrain"));
}

#[cfg(unix)]
#[test]
fn failed_compensating_remove_retains_pending_cleanup_receipt() {
    let vault = vault();
    let home = tempdir().unwrap();
    let bin = home.path().join("codex");
    fs::write(
        &bin,
        "#!/bin/sh\ncase \"$*\" in\n  \"plugin remove onebrain@onebrain\") exit 23 ;;\nesac\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(home.path().join("config.toml"), "model = \"gpt-5\"\n").unwrap();
    fs::write(vault.path().join(".codex"), "blocks marker directory").unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("CODEX_BIN", &bin)
        .env("CODEX_HOME", home.path())
        .args([
            "--vault",
            vault.path().to_str().unwrap(),
            "plugin",
            "install",
            "--harness",
            "codex",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("plugin cleanup also failed"));

    let receipt = fs::read_dir(home.path().join("onebrain-managed"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(receipt).unwrap()).unwrap()
            ["state"],
        "pending"
    );
    assert_eq!(
        fs::read_to_string(home.path().join("config.toml")).unwrap(),
        "model = \"gpt-5\"\n"
    );
}

#[cfg(unix)]
#[test]
fn finalize_failure_never_removes_preexisting_manual_plugin() {
    let vault = vault();
    let home = tempdir().unwrap();
    let bin = home.path().join("codex");
    let log = home.path().join("argv.log");
    fs::write(
        &bin,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CODEX_ARGV_LOG\"\ncase \"$*\" in\n  \"plugin list\") printf '%s\\n' 'onebrain@onebrain installed' ;;\nesac\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(vault.path().join(".codex"), "blocks marker directory").unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("CODEX_BIN", &bin)
        .env("CODEX_HOME", home.path())
        .env("CODEX_ARGV_LOG", &log)
        .args([
            "--vault",
            vault.path().to_str().unwrap(),
            "plugin",
            "install",
            "--harness",
            "codex",
        ])
        .assert()
        .failure();

    assert!(!fs::read_to_string(log).unwrap().contains("plugin remove"));
}

#[cfg(unix)]
#[test]
fn unknown_plugin_prestate_retains_pending_receipt_on_finalize_failure() {
    let vault = vault();
    let home = tempdir().unwrap();
    let bin = home.path().join("codex");
    let log = home.path().join("argv.log");
    fs::write(
        &bin,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CODEX_ARGV_LOG\"\ncase \"$*\" in\n  \"plugin list\") exit 9 ;;\nesac\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(vault.path().join(".codex"), "blocks marker directory").unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("CODEX_BIN", &bin)
        .env("CODEX_HOME", home.path())
        .env("CODEX_ARGV_LOG", &log)
        .args([
            "--vault",
            vault.path().to_str().unwrap(),
            "plugin",
            "install",
            "--harness",
            "codex",
        ])
        .assert()
        .failure();

    let receipt = fs::read_dir(home.path().join("onebrain-managed"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(receipt).unwrap()).unwrap()
            ["state"],
        "pending"
    );
    assert!(!fs::read_to_string(log).unwrap().contains("plugin remove"));
}

#[cfg(unix)]
#[test]
fn failed_managed_reinstall_preserves_installed_marker_and_receipt() {
    let vault = vault();
    let home = tempdir().unwrap();
    let bin = home.path().join("codex");
    fs::write(
        &bin,
        "#!/bin/sh\ncase \"$*\" in\n  \"plugin list\") printf '%s\\n' 'onebrain@onebrain installed' ;;\nesac\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let install = || {
        Command::cargo_bin("onebrain")
            .unwrap()
            .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
            .env("CODEX_BIN", &bin)
            .env("CODEX_HOME", home.path())
            .args([
                "--vault",
                vault.path().to_str().unwrap(),
                "plugin",
                "install",
                "--harness",
                "codex",
            ])
            .assert()
    };
    install().success();
    let marker = vault.path().join(".codex/onebrain-plugin.json");
    let receipt = fs::read_dir(home.path().join("onebrain-managed"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let marker_before = fs::read(&marker).unwrap();
    let receipt_before = fs::read(&receipt).unwrap();

    fs::set_permissions(marker.parent().unwrap(), fs::Permissions::from_mode(0o500)).unwrap();
    install().failure();
    fs::set_permissions(marker.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();

    assert_eq!(fs::read(marker).unwrap(), marker_before);
    assert_eq!(fs::read(receipt).unwrap(), receipt_before);
}
