//! `onebrain search status` — report native-search index status.
//!
//! Vault-required (exit 64 outside a vault). Reports the configured
//! collection, embed model, and on-disk cache dir WITHOUT opening the
//! engine — `Engine::open` no longer downloads a model (engine.rs's lazy
//! embedder), but `status` still avoids opening the engine at all so it
//! stays instant even before the tantivy/vector/redb state exists on disk.

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use crate::commands::search_common::{collection_cache_dir, is_indexed, resolve_collection};
use crate::output::{emit, Envelope, OutputMode};
use onebrain_core::load_vault_config;

#[derive(Debug, Serialize)]
struct SearchStatusData {
    collection: Option<String>,
    embed_model: String,
    cache_dir: Option<PathBuf>,
    indexed: bool,
}

pub fn run(vault_flag: Option<PathBuf>, mode: &OutputMode) -> Result<()> {
    let (resolved, collection) = resolve_collection(vault_flag)?;
    let vault_info = crate::vault_ctx::info_from(&resolved);
    let config = load_vault_config(&resolved.root)?;

    let (cache_dir, indexed) = match &collection {
        Some(c) => {
            let dir = collection_cache_dir(c);
            let indexed = is_indexed(&dir);
            (Some(dir), indexed)
        }
        None => (None, false),
    };

    let data = SearchStatusData {
        collection,
        embed_model: config.search.embed_model,
        cache_dir,
        indexed,
    };

    let envelope = Envelope::ok("search.status", Some(vault_info), data);
    emit(&envelope, mode, std::io::stdout().lock(), render_text)?;
    Ok(())
}

fn render_text(env: &Envelope<SearchStatusData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    let mut lines = vec!["search index status".to_string()];
    match &d.collection {
        Some(c) => lines.push(format!("  collection:  {c}")),
        None => lines
            .push("  collection:  (not set — set `search.collection` in onebrain.yml)".to_string()),
    }
    lines.push(format!("  embed model: {}", d.embed_model));
    if let Some(dir) = &d.cache_dir {
        lines.push(format!("  cache dir:   {}", dir.display()));
    }
    lines.push(format!(
        "  indexed:     {}",
        if d.indexed { "yes" } else { "no" }
    ));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(collection: Option<&str>, indexed: bool) -> Envelope<SearchStatusData> {
        Envelope::ok(
            "search.status",
            None,
            SearchStatusData {
                collection: collection.map(str::to_string),
                embed_model: "multilingual-e5-small".to_string(),
                cache_dir: collection.map(|c| PathBuf::from(format!("/cache/{c}"))),
                indexed,
            },
        )
    }

    #[test]
    fn text_shows_collection_and_model() {
        let s = render_text(&env(Some("ob-1"), true));
        assert!(s.contains("collection:  ob-1"));
        assert!(s.contains("embed model: multilingual-e5-small"));
        assert!(s.contains("indexed:     yes"));
    }

    #[test]
    fn text_flags_missing_collection() {
        let s = render_text(&env(None, false));
        assert!(s.contains("not set"));
        assert!(s.contains("indexed:     no"));
    }

    #[test]
    fn json_shape_has_expected_fields() {
        let e = env(Some("ob-1"), true);
        let v = serde_json::to_value(e.data.as_ref().unwrap()).unwrap();
        assert_eq!(v["collection"], "ob-1");
        assert!(v["embed_model"].is_string());
        assert_eq!(v["indexed"], true);
    }
}
