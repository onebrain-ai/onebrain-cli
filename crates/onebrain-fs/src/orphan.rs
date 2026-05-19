//! Orphan checkpoint scan · port of Bun's `runOrphanScan` (orphan-scan.ts).

/// Parse a checkpoint filename of the form `YYYY-MM-DD-{token}-checkpoint-NN.md`.
/// Returns `(date, token)` or `None` if the shape doesn't match.
#[allow(dead_code)] // used by upcoming orphan-scan tasks in this slice
pub(crate) fn parse_checkpoint_filename(name: &str) -> Option<(&str, &str)> {
    // YYYY-MM-DD prefix — exactly 10 chars (4-2-2 with dashes at idx 4 and 7)
    if name.len() < 11 {
        return None;
    }
    let bytes = name.as_bytes();
    let valid_date_chars = bytes[..10].iter().enumerate().all(|(i, &b)| match i {
        4 | 7 => b == b'-',
        _ => b.is_ascii_digit(),
    });
    if !valid_date_chars || bytes[10] != b'-' {
        return None;
    }
    let date = &name[..10];
    let after_date = &name[11..];
    let cp_idx = after_date.find("-checkpoint-")?;
    let token = &after_date[..cp_idx];
    if token.is_empty() {
        return None;
    }
    Some((date, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_filename() {
        let (date, token) =
            parse_checkpoint_filename("2026-05-19-abc123-checkpoint-01.md").unwrap();
        assert_eq!(date, "2026-05-19");
        assert_eq!(token, "abc123");
    }

    #[test]
    fn parses_multi_digit_nn() {
        let (date, token) = parse_checkpoint_filename("2026-05-19-tok-checkpoint-99.md").unwrap();
        assert_eq!(date, "2026-05-19");
        assert_eq!(token, "tok");
    }

    #[test]
    fn rejects_filename_without_checkpoint_infix() {
        assert!(parse_checkpoint_filename("2026-05-19-tok-session-01.md").is_none());
    }

    #[test]
    fn rejects_filename_without_date_prefix() {
        assert!(parse_checkpoint_filename("tok-checkpoint-01.md").is_none());
    }

    #[test]
    fn rejects_empty_token() {
        // "2026-05-19--checkpoint-01.md" has empty token between date and -checkpoint-
        assert!(parse_checkpoint_filename("2026-05-19--checkpoint-01.md").is_none());
    }
}
