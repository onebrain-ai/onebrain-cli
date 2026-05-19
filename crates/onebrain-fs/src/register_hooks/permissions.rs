//! `permissions.allow` management: append the 14-entry OneBrain permission set
//! idempotently, or strip them on `--remove`.

use serde_json::{Map, Value};

pub(crate) const PERMISSIONS_TO_ADD: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Glob",
    "Grep",
    "Bash(git *)",
    "Bash(bun *)",
    "Bash(gh *)",
    "Bash(node *)",
    "Bash(onebrain *)",
    "Bash(bun install -g @onebrain-ai/cli*)",
    "Bash(npm install -g @onebrain-ai/cli*)",
    "WebFetch",
    "WebSearch",
];

/// Append every missing entry from `PERMISSIONS_TO_ADD` to
/// `settings.permissions.allow`. Returns the list of entries appended (empty
/// on idempotent re-run).
pub(crate) fn apply_permissions(settings: &mut Value) -> Vec<String> {
    let root = settings
        .as_object_mut()
        .expect("settings is a JSON object");
    let permissions = root
        .entry("permissions".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !permissions.is_object() {
        *permissions = Value::Object(Map::new());
    }
    let perm_obj = permissions.as_object_mut().unwrap();
    let allow = perm_obj
        .entry("allow".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !allow.is_array() {
        *allow = Value::Array(Vec::new());
    }
    let arr = allow.as_array_mut().unwrap();

    let mut added = Vec::new();
    for perm in PERMISSIONS_TO_ADD {
        let present = arr.iter().any(|v| v.as_str() == Some(*perm));
        if !present {
            arr.push(Value::String((*perm).to_string()));
            added.push((*perm).to_string());
        }
    }
    added
}

/// Strip every `PERMISSIONS_TO_ADD` entry from `settings.permissions.allow`.
/// Idempotent. Returns the list of entries actually removed.
pub(crate) fn strip_permissions(settings: &mut Value) -> Vec<String> {
    let Some(root) = settings.as_object_mut() else {
        return Vec::new();
    };
    let Some(perm) = root.get_mut("permissions").and_then(|v| v.as_object_mut()) else {
        return Vec::new();
    };
    let Some(allow) = perm.get_mut("allow").and_then(|v| v.as_array_mut()) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    let mut kept: Vec<Value> = Vec::new();
    for v in allow.drain(..) {
        match v.as_str() {
            Some(s) if PERMISSIONS_TO_ADD.contains(&s) => removed.push(s.to_string()),
            _ => kept.push(v),
        }
    }
    *allow = kept;
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn apply_permissions_fresh_returns_14() {
        let mut s = json!({});
        let added = apply_permissions(&mut s);
        assert_eq!(added.len(), PERMISSIONS_TO_ADD.len());
        assert_eq!(added.len(), 14);
        let arr = s["permissions"]["allow"].as_array().unwrap();
        assert_eq!(arr.len(), 14);
    }

    #[test]
    fn apply_permissions_idempotent_returns_empty() {
        let mut s = json!({});
        apply_permissions(&mut s);
        let added = apply_permissions(&mut s);
        assert!(added.is_empty());
    }

    #[test]
    fn apply_permissions_partial_existing_returns_missing_only() {
        let mut s = json!({"permissions": {"allow": ["Read", "Write"]}});
        let added = apply_permissions(&mut s);
        assert_eq!(added.len(), 12);
        assert!(!added.contains(&"Read".to_string()));
        assert!(added.contains(&"Bash(onebrain *)".to_string()));
    }

    #[test]
    fn apply_permissions_preserves_user_entry() {
        let mut s = json!({"permissions": {"allow": ["Bash(my-script *)"]}});
        apply_permissions(&mut s);
        let arr = s["permissions"]["allow"].as_array().unwrap();
        assert!(arr.iter().any(|v| v.as_str() == Some("Bash(my-script *)")));
    }

    #[test]
    fn apply_permissions_preserves_existing_permission_order() {
        let mut s = json!({"permissions": {"allow": ["existing-1", "Read", "existing-2"]}});
        apply_permissions(&mut s);
        let arr = s["permissions"]["allow"].as_array().unwrap();
        assert_eq!(arr[0], "existing-1");
        assert_eq!(arr[1], "Read");
        assert_eq!(arr[2], "existing-2");
    }

    #[test]
    fn strip_permissions_removes_only_onebrain_entries() {
        let mut s = json!({"permissions": {"allow": [
            "Read",
            "Bash(onebrain *)",
            "Bash(my-script *)",
            "WebFetch",
        ]}});
        let removed = strip_permissions(&mut s);
        assert_eq!(removed.len(), 3);
        let arr = s["permissions"]["allow"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0], "Bash(my-script *)");
    }

    #[test]
    fn strip_permissions_idempotent_returns_empty_second_time() {
        let mut s = json!({"permissions": {"allow": ["Read", "Write"]}});
        strip_permissions(&mut s);
        let removed2 = strip_permissions(&mut s);
        assert!(removed2.is_empty());
    }

    #[test]
    fn strip_permissions_no_permissions_field_returns_empty() {
        let mut s = json!({});
        let removed = strip_permissions(&mut s);
        assert!(removed.is_empty());
    }
}
