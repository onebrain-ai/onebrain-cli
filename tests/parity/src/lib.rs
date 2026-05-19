use std::path::{Path, PathBuf};
use std::process::Command;

pub struct ParityRunner {
    pub bun_binary: PathBuf,
    pub rust_binary: PathBuf,
}

impl ParityRunner {
    /// Locate binaries from env vars (`BUN_BINARY`, `RUST_BINARY`) with defaults.
    pub fn from_env() -> Self {
        let bun_binary = std::env::var_os("BUN_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp/onebrain-bun-v2.3.3"));
        let rust_binary = std::env::var_os("RUST_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release/onebrain")
            });
        Self {
            bun_binary,
            rust_binary,
        }
    }

    /// Returns `true` if both binaries exist on disk.
    /// Use this to skip tests early when the parity environment is not set up.
    pub fn binaries_available(&self) -> bool {
        self.bun_binary.exists() && self.rust_binary.exists()
    }

    pub fn run(&self, binary: &Path, args: &[&str], cwd: &Path) -> String {
        let output = Command::new(binary)
            .args(args)
            .current_dir(cwd)
            .env("TZ", "UTC") // fix datetime across runs
            .env("WT_SESSION", "PARITY_FIXED_TOKEN") // pin session token
            .output()
            .unwrap_or_else(|e| panic!("spawn {binary:?} failed: {e}"));
        if !output.status.success() {
            panic!(
                "binary {binary:?} exited {}: stderr=\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        String::from_utf8(output.stdout).expect("non-utf8 stdout")
    }

    pub fn assert_parity(&self, args: &[&str], fixture: &Path) {
        let bun_raw = self.run(&self.bun_binary, args, fixture);
        let rust_raw = self.run(&self.rust_binary, args, fixture);

        let bun_norm = normalize(&bun_raw);
        let rust_norm = normalize(&rust_raw);

        pretty_assertions::assert_eq!(
            bun_norm,
            rust_norm,
            "parity failure: cmd={args:?} fixture={}",
            fixture.display()
        );
    }
}

fn normalize(s: &str) -> serde_json::Value {
    let mut v: serde_json::Value = serde_json::from_str(s.trim()).expect("not JSON");
    if let Some(obj) = v.as_object_mut() {
        if obj.contains_key("datetime") {
            obj.insert("datetime".into(), serde_json::Value::String("<N>".into()));
        }
        if obj.contains_key("session_token") {
            obj.insert(
                "session_token".into(),
                serde_json::Value::String("<N>".into()),
            );
        }
    }
    v
}
