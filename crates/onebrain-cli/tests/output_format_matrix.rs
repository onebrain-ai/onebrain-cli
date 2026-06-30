//! v3.1 output-format matrix · one parametric run over every structured-
//! output command × every supported mode.
//!
//! Why: v3.1 default flipped from JSON to text for hook-protocol commands.
//! That bug class ("the rewriter forgot one command, default regressed to
//! JSON") would NOT have been caught by command-specific tests that only
//! assert one mode. This matrix is the floor: every entry that emits JSON
//! must also handle `--json` (compact), `--json --pretty` (indented),
//! `--yaml` (YAML), and default-text. If a new structured-output command
//! lands without an entry here, the matrix won't fail — that's by design;
//! the matrix is a known-good list, not an introspection oracle. Owners of
//! new commands MUST add an entry.

use assert_cmd::Command;
use serde_json::Value;
use std::path::PathBuf;
use tempfile::tempdir;

/// One command under test. `name` is for panic messages; `args` is the
/// positional / sub-command argv (no format flags). `cwd_setup` writes any
/// fixture files needed before the binary runs.
struct Cmd<'a> {
    name: &'a str,
    args: &'a [&'a str],
    cwd_setup: fn(&std::path::Path),
    /// If true, the command's structured shape is a hook-protocol block
    /// (`{"orphan_count":N}` or `{"decision":"block",...}`). False for
    /// commands that emit the canonical v3.1 envelope.
    hook_protocol_shape: bool,
    /// True when the matrix entry deliberately exercises an error path
    /// (e.g. `doctor` outside a vault) where the binary exits non-zero but
    /// the stdout payload is still the structured failure envelope we care
    /// about. Without this, `run()` would panic on `assert!(success)`.
    allow_nonzero_exit: bool,
}

fn no_setup(_: &std::path::Path) {}

fn write_vault_yml(p: &std::path::Path) {
    std::fs::write(p.join("vault.yml"), "qmd_collection: x\n").unwrap();
}

fn write_checkpoint_dir(p: &std::path::Path) {
    std::fs::write(p.join("vault.yml"), "method: onebrain\n").unwrap();
    std::fs::create_dir_all(p.join("checkpoint")).unwrap();
}

/// Hook-protocol commands · v3.1 default = text, structured = JSON / YAML.
const COMMANDS: &[Cmd<'static>] = &[
    Cmd {
        name: "session init · outside vault",
        args: &["session", "init"],
        cwd_setup: no_setup,
        hook_protocol_shape: true,
        allow_nonzero_exit: false,
    },
    Cmd {
        name: "session init · inside vault",
        args: &["session", "init"],
        cwd_setup: write_vault_yml,
        hook_protocol_shape: true,
        allow_nonzero_exit: false,
    },
    Cmd {
        name: "checkpoint orphans · empty",
        args: &["checkpoint", "orphans", ".", "tokABC123"],
        cwd_setup: write_checkpoint_dir,
        hook_protocol_shape: true,
        allow_nonzero_exit: false,
    },
    Cmd {
        name: "harness · empty dir",
        args: &["harness", "detect"],
        // No setup — empty cwd → `direct` harness detected.
        cwd_setup: no_setup,
        hook_protocol_shape: true,
        allow_nonzero_exit: false,
    },
    Cmd {
        name: "doctor · no vault (error path)",
        args: &["doctor"],
        // No vault → exits 1 with structured error envelope; matrix's
        // `run` tolerates non-success when output is the structured
        // failure shape (see allow_nonzero_exit below).
        cwd_setup: no_setup,
        hook_protocol_shape: false,
        allow_nonzero_exit: true,
    },
    Cmd {
        // `task list` inside a vault with no task notes → empty list, but still
        // the canonical envelope; exercises text/json/yaml format parity for
        // the v3.3.14-surfaced verb.
        name: "task list · inside vault",
        args: &["task", "list"],
        cwd_setup: write_vault_yml,
        hook_protocol_shape: false,
        allow_nonzero_exit: false,
    },
];

#[test]
fn output_format_matrix_default_is_text_not_json() {
    for c in COMMANDS {
        let stdout = run(c, &[]);
        assert!(
            !stdout.trim_start().starts_with('{'),
            "[{}/default] expected text, got JSON-shaped output: {stdout}",
            c.name
        );
        assert!(
            !stdout.trim_start().starts_with('['),
            "[{}/default] expected text, got JSON-array output: {stdout}",
            c.name
        );
    }
}

#[test]
fn output_format_matrix_json_flag_emits_compact_json() {
    for c in COMMANDS {
        let stdout = run(c, &["--json"]);
        let parsed: Value = serde_json::from_str(stdout.trim())
            .unwrap_or_else(|e| panic!("[{}/--json] invalid JSON: {e}\n{stdout}", c.name));
        assert!(
            parsed.is_object(),
            "[{}/--json] expected object root, got {parsed:?}",
            c.name
        );
        // Compact JSON has at most one trailing newline.
        let body = stdout.trim();
        assert!(
            !body.contains("\n"),
            "[{}/--json] compact mode should be single-line; got: {stdout}",
            c.name
        );
    }
}

#[test]
fn output_format_matrix_json_pretty_emits_indented_multiline() {
    for c in COMMANDS {
        let stdout = run(c, &["--json", "--pretty"]);
        assert!(
            stdout.contains('\n'),
            "[{}/--json --pretty] expected multi-line; got: {stdout}",
            c.name
        );
        assert!(
            stdout.contains("  "),
            "[{}/--json --pretty] expected 2-space indent somewhere; got: {stdout}",
            c.name
        );
        // Still parseable.
        let _v: Value = serde_json::from_str(stdout.trim())
            .unwrap_or_else(|e| panic!("[{}/--json --pretty] invalid JSON: {e}\n{stdout}", c.name));
    }
}

#[test]
fn output_format_matrix_yaml_flag_emits_yaml() {
    for c in COMMANDS {
        let stdout = run(c, &["--yaml"]);
        assert!(
            !stdout.trim_start().starts_with('{'),
            "[{}/--yaml] expected YAML, got JSON-shaped: {stdout}",
            c.name
        );
        let v: serde_yaml::Value = serde_yaml::from_str(&stdout)
            .unwrap_or_else(|e| panic!("[{}/--yaml] invalid YAML: {e}\n{stdout}", c.name));
        assert!(
            v.is_mapping() || v.is_sequence(),
            "[{}/--yaml] expected mapping/sequence root, got {v:?}",
            c.name
        );
    }
}

#[test]
fn output_format_matrix_output_json_alias_works() {
    for c in COMMANDS {
        let stdout = run(c, &["--output", "json"]);
        let _v: Value = serde_json::from_str(stdout.trim())
            .unwrap_or_else(|e| panic!("[{}/--output json] invalid JSON: {e}\n{stdout}", c.name));
    }
}

#[test]
fn output_format_matrix_hook_protocol_block_shape_consistent_across_modes() {
    // For hook-protocol commands the structured envelope is the raw block
    // (not the v3.1 envelope). Verify the shape stays consistent across
    // JSON / YAML so the rewriter's flag injection doesn't break Claude
    // Code's parser.
    for c in COMMANDS.iter().filter(|c| c.hook_protocol_shape) {
        let json_stdout = run(c, &["--json"]);
        let yaml_stdout = run(c, &["--yaml"]);
        let json_v: Value = serde_json::from_str(json_stdout.trim()).unwrap();
        let yaml_v: Value = serde_yaml::from_str::<serde_yaml::Value>(&yaml_stdout)
            .map(|y| serde_json::to_value(y).unwrap())
            .unwrap();
        let json_keys: std::collections::BTreeSet<_> = json_v
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let yaml_keys: std::collections::BTreeSet<_> = yaml_v
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        assert_eq!(
            json_keys, yaml_keys,
            "[{}] JSON/YAML key sets diverge — flag injection broke shape parity",
            c.name
        );
    }
}

fn run(c: &Cmd<'_>, format_flags: &[&str]) -> String {
    let dir = tempdir().unwrap();
    (c.cwd_setup)(dir.path());

    let mut all_args: Vec<&str> = c.args.to_vec();
    all_args.extend(format_flags.iter().copied());

    let output = Command::cargo_bin("onebrain")
        .unwrap()
        .args(&all_args)
        .current_dir(dir.path())
        // Force non-TTY → pure text, no color. Mirrors what hook consumers
        // see when running this from a subprocess pipe.
        .env_remove("CLICOLOR_FORCE")
        .env("NO_COLOR", "1")
        .output()
        .expect("spawn failed");
    if !c.allow_nonzero_exit {
        assert!(
            output.status.success(),
            "[{} {:?}] non-zero exit: {:?}\nstderr: {}",
            c.name,
            format_flags,
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    } else {
        // Error-path entries (e.g. doctor outside a vault) must still
        // emit *something* on stdout — silent non-zero defeats the
        // structured-error promise.
        assert!(
            !output.stdout.is_empty()
                || matches!(format_flags, &[] | &["--pretty"]),
            "[{} {:?}] non-zero exit with empty stdout — structured error envelope expected\nstderr: {}",
            c.name,
            format_flags,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout).expect("non-utf8 stdout")
}

// Path-fixture compatibility helper for hosts where TempDir paths exceed
// path-length limits (rare on macOS / Linux; defensive for CI matrix runs).
#[allow(dead_code)]
fn _ensure_short_path(p: &std::path::Path) -> PathBuf {
    p.to_path_buf()
}
