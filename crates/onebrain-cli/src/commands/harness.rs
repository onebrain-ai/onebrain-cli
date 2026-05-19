use anyhow::{Context, Result};
use onebrain_fs::detect_harnesses;
use serde::Serialize;
use std::env;

#[derive(Serialize)]
struct HarnessOutput {
    harnesses: Vec<String>,
}

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
