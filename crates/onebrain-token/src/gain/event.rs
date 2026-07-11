//! `GainEvent` — one line per optimized call, per design 0030 (Tier 1 raw
//! log). `Surface` and `CacheKind` are its dimension fields.

use serde::{Deserialize, Serialize};

use crate::level::OptLevel;

/// Every place an agent-facing response can pass through the funnel. The
/// per-surface integration tests (Track 4) assert one of these lands a
/// `GainEvent` per optimized call — "no surface escapes metering."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    McpQuery,
    McpGet,
    McpMultiGet,
    CliSearch,
    DaemonHttp,
    ReadHook,
}

impl std::fmt::Display for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Surface::McpQuery => "mcp_query",
            Surface::McpGet => "mcp_get",
            Surface::McpMultiGet => "mcp_multi_get",
            Surface::CliSearch => "cli_search",
            Surface::DaemonHttp => "daemon_http",
            Surface::ReadHook => "read_hook",
        };
        f.write_str(s)
    }
}

/// Which cache layer, if any, served this call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheKind {
    None,
    MemoHit,
    LedgerRef,
    HookFailopen,
}

impl std::fmt::Display for CacheKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            CacheKind::None => "none",
            CacheKind::MemoHit => "memo_hit",
            CacheKind::LedgerRef => "ledger_ref",
            CacheKind::HookFailopen => "hook_failopen",
        };
        f.write_str(s)
    }
}

/// One optimized call, as appended to `token/gain/YYYY-MM.jsonl` (design
/// 0030 Tier 1) and rolled up into `token.redb` (Track 2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GainEvent {
    pub ts: i64,
    pub surface: Surface,
    pub transform: String,
    pub level: OptLevel,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub cache: CacheKind,
    pub session_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_event_round_trips_through_json() {
        let event = GainEvent {
            ts: 1_752_000_000,
            surface: Surface::McpQuery,
            transform: "whitespace,json_compact".to_string(),
            level: OptLevel::Conservative,
            bytes_before: 1000,
            bytes_after: 400,
            cache: CacheKind::None,
            session_token: Some("abc123".to_string()),
        };

        let json = serde_json::to_string(&event).unwrap();
        let back: GainEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn field_names_and_variant_casing_match_design_0030() {
        let event = GainEvent {
            ts: 0,
            surface: Surface::ReadHook,
            transform: "none".to_string(),
            level: OptLevel::Off,
            bytes_before: 0,
            bytes_after: 0,
            cache: CacheKind::HookFailopen,
            session_token: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"surface\":\"read_hook\""));
        assert!(json.contains("\"cache\":\"hook_failopen\""));
        assert!(json.contains("\"level\":\"off\""));
        assert!(json.contains("\"session_token\":null"));
    }

    #[test]
    fn display_matches_serde_snake_case_for_every_surface_and_cache_kind() {
        // rollup.rs's composite keys and the CLI renderer both format via
        // Display — it must never drift from the wire (serde) casing.
        for (variant, expect) in [
            (Surface::McpQuery, "mcp_query"),
            (Surface::McpGet, "mcp_get"),
            (Surface::McpMultiGet, "mcp_multi_get"),
            (Surface::CliSearch, "cli_search"),
            (Surface::DaemonHttp, "daemon_http"),
            (Surface::ReadHook, "read_hook"),
        ] {
            assert_eq!(variant.to_string(), expect);
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, format!("\"{expect}\""));
        }
        for (variant, expect) in [
            (CacheKind::None, "none"),
            (CacheKind::MemoHit, "memo_hit"),
            (CacheKind::LedgerRef, "ledger_ref"),
            (CacheKind::HookFailopen, "hook_failopen"),
        ] {
            assert_eq!(variant.to_string(), expect);
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, format!("\"{expect}\""));
        }
    }
}
