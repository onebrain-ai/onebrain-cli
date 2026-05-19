use anyhow::{Context, Result};
use onebrain_fs::detect_harnesses;
use serde::Serialize;
use std::env;

#[derive(Serialize)]
struct HarnessOutput {
    harnesses: Vec<String>,
}

/// Run the `harness` diagnostic · emits JSON listing detected harnesses.
///
/// Uses `cwd` as the vault root (no walk-up) · matches the contract for all
/// other internal subcommands that read vault.yml: invoked from vault root by
/// the agent / SessionStart hook. Add `find_vault_root` walk-up if a future
/// invocation pattern needs it (Slice 7 register-hooks should pin this).
pub fn run() -> Result<()> {
    let cwd = env::current_dir().context("read current directory")?;
    let harnesses = detect_harnesses(&cwd);
    let output = HarnessOutput {
        harnesses: harnesses.iter().map(|h| h.as_str().to_string()).collect(),
    };
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_struct_serializes_correctly() {
        let out = HarnessOutput {
            harnesses: vec!["claude".to_string()],
        };
        assert_eq!(
            serde_json::to_string(&out).unwrap(),
            r#"{"harnesses":["claude"]}"#
        );
    }
}
