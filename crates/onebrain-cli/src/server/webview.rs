//! Frameability preflight for the in-app webview: inspect a URL's response
//! headers so the frontend knows whether to embed it in an iframe or fall back
//! to a new tab. Any failure degrades to "not frameable" (safe fallback).

/// http / https only — the only schemes we preflight or frame.
pub fn is_http_url(url: &str) -> bool {
    let u = url.trim();
    u.starts_with("http://") || u.starts_with("https://")
}

/// Decide framability from the two headers that govern it. Conservative: any
/// `X-Frame-Options` value blocks (DENY/SAMEORIGIN, or a legacy ALLOW-FROM we
/// can't honour from an opaque iframe); a CSP with ANY `frame-ancestors`
/// directive that isn't the wildcard `*` blocks. Absent headers → frameable.
pub fn frameable_from_headers(x_frame_options: Option<&str>, csp: Option<&str>) -> bool {
    if x_frame_options
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        return false;
    }
    if let Some(csp) = csp {
        // Find the frame-ancestors directive (case-insensitive) and read its value.
        let lower = csp.to_ascii_lowercase();
        if let Some(idx) = lower.find("frame-ancestors") {
            let after = &csp[idx + "frame-ancestors".len()..];
            let directive = after.split(';').next().unwrap_or("").trim();
            // Only a bare `*` (any ancestor) permits our arbitrary origin.
            return directive == "*";
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_and_https_only() {
        assert!(is_http_url("https://example.com"));
        assert!(is_http_url("http://example.com/a?b=c"));
        assert!(!is_http_url("ftp://example.com"));
        assert!(!is_http_url("file:///etc/passwd"));
        assert!(!is_http_url("javascript:alert(1)"));
        assert!(!is_http_url("example.com"));
    }

    #[test]
    fn x_frame_options_blocks() {
        assert!(!frameable_from_headers(Some("DENY"), None));
        assert!(!frameable_from_headers(Some("deny"), None));
        assert!(!frameable_from_headers(Some("SAMEORIGIN"), None));
        assert!(!frameable_from_headers(Some("Allow-From https://x"), None)); // unknown/legacy → treat as blocked
    }

    #[test]
    fn csp_frame_ancestors_blocks() {
        assert!(!frameable_from_headers(
            None,
            Some("frame-ancestors 'none'")
        ));
        assert!(!frameable_from_headers(
            None,
            Some("default-src 'self'; frame-ancestors 'self'")
        ));
        assert!(!frameable_from_headers(
            None,
            Some("frame-ancestors https://trusted.example")
        ));
    }

    #[test]
    fn absent_or_permissive_is_frameable() {
        assert!(frameable_from_headers(None, None));
        assert!(frameable_from_headers(None, Some("default-src 'self'"))); // no frame-ancestors directive
        assert!(frameable_from_headers(None, Some("frame-ancestors *")));
    }
}
