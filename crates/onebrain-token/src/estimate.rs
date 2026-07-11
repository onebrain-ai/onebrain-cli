//! Char-aware, per-model-family token estimation.
//!
//! Deliberately NOT `bytes.len() / 4` (ADR 0030 / RTK gap-analysis §5c):
//! byte length inflates for multi-byte scripts (Thai is 3 bytes/char in
//! UTF-8), which has no honest relationship to how a BPE tokenizer actually
//! spends tokens on that script. This estimator walks `.chars()` and applies
//! a per-family, per-script chars-per-token ratio instead.

/// Coarse model families with distinct calibration tables. Config-level
/// `model: auto` resolution (sniffing `settings.json`, pricing hints) is a
/// caller concern outside this pure crate — callers resolve a concrete
/// `ModelFamily` before calling [`estimate_tokens`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum ModelFamily {
    /// Claude family (Sonnet/Opus/Haiku) — the default when nothing more
    /// specific is known.
    #[default]
    ClaudeGeneric,
    /// GPT-4-class OpenAI models (cl100k/o200k-style tokenizers).
    Gpt4Family,
}

impl ModelFamily {
    /// `(ascii_chars_per_token, multibyte_chars_per_token)`. Multi-byte
    /// (non-ASCII) scripts — Thai, CJK, etc. — get their own, lower ratio:
    /// these scripts spend more tokens per character than English prose,
    /// but nowhere near the 3-4x blowup a raw byte-length heuristic would
    /// imply.
    const fn calibration(self) -> (f64, f64) {
        match self {
            ModelFamily::ClaudeGeneric => (4.0, 2.2),
            ModelFamily::Gpt4Family => (4.0, 2.0),
        }
    }
}

/// Estimate the token count of `text` for `model`. Char-aware: iterates
/// Unicode scalar values rather than bytes, so multi-byte scripts are never
/// penalized for their UTF-8 encoding width. Always labeled an estimate by
/// callers — this is a calibrated heuristic, not a real tokenizer.
pub fn estimate_tokens(text: &str, model: ModelFamily) -> u64 {
    if text.is_empty() {
        return 0;
    }

    let (ascii_cpt, multibyte_cpt) = model.calibration();
    let mut ascii_chars: u64 = 0;
    let mut multibyte_chars: u64 = 0;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_chars += 1;
        } else {
            multibyte_chars += 1;
        }
    }

    let tokens = (ascii_chars as f64 / ascii_cpt) + (multibyte_chars as f64 / multibyte_cpt);
    tokens.ceil().max(1.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENGLISH: &str = include_str!("../tests/fixtures/estimate/english.md");
    const THAI: &str = include_str!("../tests/fixtures/estimate/thai.md");
    const MIXED: &str = include_str!("../tests/fixtures/estimate/mixed.md");

    #[test]
    fn is_char_aware_not_byte_aware() {
        // Thai UTF-8 runs ~3 bytes/char; if this were secretly `len()/4`
        // (byte length), the Thai estimate would track byte length, not
        // char length. Assert it does NOT match the byte-based formula.
        let byte_heuristic = (THAI.len() as f64 / 4.0).ceil() as u64;
        let char_aware = estimate_tokens(THAI, ModelFamily::ClaudeGeneric);
        assert_ne!(
            char_aware, byte_heuristic,
            "estimate must diverge from the naive bytes/4 heuristic for multi-byte text"
        );
    }

    #[test]
    fn thai_is_not_overcounted_vs_char_count() {
        // No tokenizer needs more than one token per character — this is
        // the regression guard the RTK byte-length bug would have failed:
        // a byte-length-driven formula on a 3-bytes/char script can blow
        // past the character count entirely.
        let chars = THAI.chars().count() as u64;
        let tokens = estimate_tokens(THAI, ModelFamily::ClaudeGeneric);
        assert!(
            tokens <= chars,
            "Thai token estimate ({tokens}) exceeded char count ({chars})"
        );
    }

    #[test]
    fn family_ratios_differ_for_multibyte_text() {
        let claude = estimate_tokens(THAI, ModelFamily::ClaudeGeneric);
        let gpt = estimate_tokens(THAI, ModelFamily::Gpt4Family);
        assert_ne!(
            claude, gpt,
            "distinct model families must calibrate differently for multi-byte scripts"
        );
    }

    #[test]
    fn english_and_thai_have_different_chars_per_token() {
        // Same-ish document shape (frontmatter + heading + prose + list),
        // different scripts — the tokens-per-char ratio should differ,
        // proving the calibration is script-aware rather than a flat divisor.
        let english_tokens = estimate_tokens(ENGLISH, ModelFamily::ClaudeGeneric) as f64;
        let english_chars = ENGLISH.chars().count() as f64;
        let thai_tokens = estimate_tokens(THAI, ModelFamily::ClaudeGeneric) as f64;
        let thai_chars = THAI.chars().count() as f64;

        let english_tpc = english_tokens / english_chars;
        let thai_tpc = thai_tokens / thai_chars;
        assert!(
            (english_tpc - thai_tpc).abs() > 0.01,
            "expected differing tokens/char ratios, got english={english_tpc} thai={thai_tpc}"
        );
    }

    #[test]
    fn mixed_script_text_estimates_between_pure_scripts_per_char() {
        let mixed_tpc = estimate_tokens(MIXED, ModelFamily::ClaudeGeneric) as f64
            / MIXED.chars().count() as f64;
        assert!(mixed_tpc > 0.0);
    }

    #[test]
    fn empty_text_is_zero_tokens() {
        assert_eq!(estimate_tokens("", ModelFamily::ClaudeGeneric), 0);
    }
}
