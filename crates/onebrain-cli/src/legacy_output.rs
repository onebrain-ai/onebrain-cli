use crate::output::OutputMode;
use serde::Serialize;

/// Successful session-init output. Extends Bun v2.3.3's JSON shape with
/// `headless` (v3.2.6): `true` when invoked under `onebrain skill run`
/// (`ONEBRAIN_HEADLESS=1`), letting INSTRUCTIONS.md skip the interactive
/// startup ceremony. Unknown to older consumers, which ignore extra fields.
///
/// The unembedded-doc count is `Option` (v3.4): `null` means the probe could
/// not determine the count (index dir missing, or the engine failed to open /
/// report status), `0` means a genuine zero (or a vault with no search
/// collection configured), `N` means N pending. The value is sourced from the
/// native search index directly (`Engine::status` under
/// `crate::commands::session_init::native_pending`). Note the value is total
/// index drift (`pending_new + pending_changed + pending_removed`), not
/// strictly *unembedded* docs as the name implies — a vault with only pending
/// deletions reports a nonzero count. This is intentional: any drift means the
/// index is stale and warrants a reindex, which is exactly what the startup
/// warning nudges. The field is always serialised (never skipped) so the hook
/// contract keeps the key. A false `0` on probe failure silently hid pending
/// embeddings at startup — `null` lets the SessionStart consumer surface
/// "unknown" instead. Consumers MUST treat `null` as "couldn't check" rather
/// than a count: the OneBrain INSTRUCTIONS.md startup status keys its embed
/// warning on `= 0` / `> 0`, both of which `null` correctly falls outside of.
///
/// **v3.4.5 field rename (transition):** the canonical key is now
/// `search_unembedded`. The former name `qmd_unembedded` is emitted alongside
/// it with the SAME value for one transition version, so plugins that still
/// read `qmd_unembedded` keep working. Once the OneBrain plugin reads
/// `search_unembedded` everywhere (with a fallback to the old key), the
/// deprecated `qmd_unembedded` field is dropped.
#[derive(Debug, Serialize)]
pub struct SessionInitOutput {
    pub datetime: String,
    pub session_token: String,
    /// Canonical unembedded/drift count (v3.4.5+).
    pub search_unembedded: Option<usize>,
    /// Deprecated alias for `search_unembedded`, emitted with the same value
    /// for one transition version so pre-v3.4.5 plugins keep parsing. Remove
    /// once the plugin reads `search_unembedded` everywhere.
    pub qmd_unembedded: Option<usize>,
    /// Count of session logs under `<logs>/session/**/*-session-*.md` whose
    /// frontmatter has no `recapped:` key — the same discovery criterion the
    /// `/recap` skill uses. `null` means the bounded probe timed out; `0`
    /// means a genuine zero (including a fresh vault with no session logs
    /// yet); `N` means N session logs are awaiting `/recap`. Always
    /// serialised (never skipped) so the hook contract keeps the key.
    pub recap_pending: Option<u64>,
    pub headless: bool,
}

/// Serialize a hook-protocol block / output to the wire format for the given
/// output mode.
///
/// **v3.1 behaviour:** default (`OutputMode::Text { .. }`) renderers live
/// next to each command's body — text formatting is shape-specific (a session
/// metadata line vs. a "no vault found" message vs. an orphan-count line) so
/// this generic serializer does NOT render text. For text mode, callers
/// must dispatch on `OutputMode::Text { .. }` and render themselves; the
/// Text arm here is a last-resort fallback that emits compact JSON rather
/// than panicking — but the output is incorrect for text-mode consumers,
/// so fix the caller. `debug_assert!` catches the contract violation in
/// debug builds.
///
/// For the structured branches (`Json` / `Yaml`) this function is the single
/// emit point; it keeps the JSON shape rules (compact vs. pretty) in one
/// place. (v3.2.15: `Table` / `Tsv` variants dropped — both fell through to
/// the JSON encoder unchanged.)
///
/// **Error handling:** serialisation failures are surfaced via a stderr
/// warning AND the function returns whatever string emerged (typically
/// empty for the failed mode, or the fallback JSON form for YAML
/// downgrades). Silent empty stdout would mimic the v3.0 regression the
/// release is named after, so failures are now LOUD on stderr. Static
/// shapes don't fail today, but every future field is one breakage away.
///
/// `--pretty` flag is honoured for explicit JSON mode (`OutputMode::Json
/// { pretty: true }`). YAML is already multi-line / "pretty" by construction.
///
/// **Hook-protocol contract:** machine consumers (Claude Code SessionStart /
/// Stop hooks) must pass `--json` explicitly in v3.1+. The hook rewriter
/// adds the flag during `onebrain plugin update`; fresh installs scaffold
/// it directly. See `v31/hook_rewriter.rs` + `register_hooks/hooks.rs`.
pub fn serialize_for_mode<T: Serialize>(value: &T, mode: &OutputMode) -> String {
    match mode {
        OutputMode::Yaml => match serde_yaml::to_string(value) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "onebrain: yaml serialisation failed ({e}); falling back to compact JSON"
                );
                serialize_json(value, false)
            }
        },
        OutputMode::Json { pretty } => serialize_json(value, *pretty),
        // Text-mode arrival is a caller bug — the per-command text renderer
        // should have absorbed this. Loud in debug, compact-JSON fallback in
        // release (compact, not pretty, because pretty-text-via-JSON would
        // be a stranger output than compact-text-via-JSON).
        OutputMode::Text { .. } => {
            debug_assert!(
                false,
                "serialize_for_mode invoked with Text mode — caller must dispatch first"
            );
            serialize_json(value, false)
        }
    }
}

/// Serialise `value` as JSON (compact or pretty). Surfaces serde failures
/// to stderr so silent empty stdout cannot recreate the v3.0 regression
/// class — empty stdout from a hook-protocol command is parsed by Claude
/// Code as malformed JSON and triggers the "vault not initialised" path
/// with no clue why.
fn serialize_json<T: Serialize>(value: &T, pretty: bool) -> String {
    let result = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
    match result {
        Ok(s) => s,
        Err(e) => {
            eprintln!("onebrain: json serialisation failed ({e})");
            String::new()
        }
    }
}

/// Block output emitted when session-init can't proceed.
///
/// Two reasons:
/// - `onebrain-vault-not-found` — no `vault.yml` / `onebrain.yml` found
///   anywhere up from cwd. Renamed in v3.1 from the v3.0 spelling
///   `onebrain-init-required` (clearer · brand-prefixed).
/// - `onebrain-vault-malformed` — vault config exists but failed to parse.
///   Carries an `error_detail` field so the SessionStart hook consumer can
///   surface the parse-error message to the user.
#[derive(Debug, Serialize)]
pub struct SessionInitBlock {
    pub decision: &'static str,
    pub reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<String>,
}

impl SessionInitBlock {
    pub fn init_required() -> Self {
        Self {
            decision: "block",
            reason: "onebrain-vault-not-found",
            error_detail: None,
        }
    }

    /// `vault.yml` exists but is unreadable / malformed. Distinct reason
    /// so the SessionStart consumer can route the user to "fix your
    /// vault.yml" instead of "run /onboarding".
    pub fn vault_malformed(detail: impl Into<String>) -> Self {
        Self {
            decision: "block",
            reason: "onebrain-vault-malformed",
            error_detail: Some(detail.into()),
        }
    }
}
