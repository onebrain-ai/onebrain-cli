//! Terminal QR code for the connector URL (v3.5.2 `gateway setup`, design D10).
//!
//! The content is the connector URL ONLY — never the pairing code (a secret
//! that must not end up in a scrollback screenshot). The caller shows it only
//! on a TTY.

// Consumed by `gateway setup` (v3.5.2 T4), which lands next and drops this.
#![allow(dead_code)]

/// Render `text` as a QR code in Unicode half-blocks (`█▀▄ `), two modules
/// per character row, with the standard 4-module quiet zone (`qrcode`'s
/// quiet zone is a bool; its width is fixed by the spec). Colours are
/// inverted (light modules drawn as blocks) like `qrencode -t UTF8`, so the
/// code reads dark-on-light on the usual dark terminal background.
pub(crate) fn render_qr(text: &str) -> anyhow::Result<String> {
    use qrcode::render::unicode::Dense1x2;
    let code = qrcode::QrCode::new(text.as_bytes())
        .map_err(|e| anyhow::anyhow!("cannot encode the connector URL as a QR code: {e}"))?;
    Ok(code
        .render::<Dense1x2>()
        .dark_color(Dense1x2::Light)
        .light_color(Dense1x2::Dark)
        .quiet_zone(true)
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://gw.example.com/mcp";

    #[test]
    fn renders_half_blocks_with_a_four_module_quiet_zone() {
        let qr = render_qr(URL).unwrap();
        let modules = qrcode::QrCode::new(URL).unwrap().width();
        let lines: Vec<&str> = qr.lines().collect();
        assert!(modules >= 21, "a QR code is at least 21 modules wide");
        // A-M3: quiet zone = 4 modules each side, 2 module rows per line.
        assert_eq!(lines.len(), (modules + 8).div_ceil(2), "{qr}");
        for l in &lines {
            assert_eq!(l.chars().count(), modules + 8, "{l:?}");
        }
        assert!(
            qr.chars()
                .all(|c| matches!(c, '█' | '▀' | '▄' | ' ' | '\n')),
            "unexpected glyph in {qr}"
        );
        // Inverted: the quiet zone is drawn, so the first line is solid.
        assert!(lines[0].chars().all(|c| c == '█'), "{:?}", lines[0]);
    }

    #[test]
    fn different_urls_render_differently() {
        assert_ne!(
            render_qr(URL).unwrap(),
            render_qr("https://other.example.com/mcp").unwrap()
        );
    }

    #[test]
    fn oversized_input_is_an_error_not_a_panic() {
        let huge = "x".repeat(8000);
        let err = render_qr(&huge).unwrap_err().to_string();
        assert!(err.contains("QR code"), "{err}");
    }
}
