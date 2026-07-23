mod support;

use assert_cmd::Command;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

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
    fs::create_dir_all(vault.path().join(".codex")).unwrap();
    fs::write(
        vault.path().join(".codex/onebrain-plugin.json"),
        r#"{"managed":true,"plugin":"onebrain@onebrain"}"#,
    )
    .unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("CODEX_BIN", &bin)
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
}

#[cfg(unix)]
#[test]
fn managed_uninstall_dry_run_preserves_marker_and_global_state() {
    let vault = vault();
    let home = tempdir().unwrap();
    fs::create_dir_all(vault.path().join(".codex")).unwrap();
    let marker = vault.path().join(".codex/onebrain-plugin.json");
    fs::write(&marker, r#"{"managed":true,"plugin":"onebrain@onebrain"}"#).unwrap();

    Command::cargo_bin("onebrain")
        .unwrap()
        .env("ONEBRAIN_CACHE_DIR", support::scratch_cache_root())
        .env("CODEX_BIN", home.path().join("must-not-run"))
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
