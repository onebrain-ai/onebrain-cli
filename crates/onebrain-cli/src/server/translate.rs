//! POST /api/translate — server-side bridge to Google's free (unofficial) gtx
//! translate endpoint. Runs on the daemon so the browser never talks to a
//! third-party origin, and the provider can be swapped without touching the UI.
//! No user-controlled URL (fixed host) → no SSRF surface. Selection text is
//! never logged.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};

/// Server-side cap: gtx handles a few KB fine; anything longer is truncated
/// and flagged so the UI can say so.
const MAX_TEXT_CHARS: usize = 5_000;

#[derive(Deserialize)]
pub struct TranslateRequest {
    pub text: String,
    #[serde(default)]
    pub from: Option<String>,
    pub to: String,
}

#[derive(Serialize)]
pub struct TranslateResponse {
    pub translated: String,
    pub detected_from: String,
    pub truncated: bool,
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

fn err(status: StatusCode, msg: &str) -> Response {
    (
        status,
        Json(ErrorBody {
            error: msg.to_string(),
        }),
    )
        .into_response()
}

pub async fn post_translate(Json(req): Json<TranslateRequest>) -> Response {
    let text = req.text.trim();
    if text.is_empty() {
        return err(StatusCode::BAD_REQUEST, "text is empty");
    }
    if !is_lang_code(&req.to) || !req.from.as_deref().is_none_or(is_lang_code) {
        return err(StatusCode::BAD_REQUEST, "invalid language code");
    }
    let truncated = text.chars().count() > MAX_TEXT_CHARS;
    let text: String = text.chars().take(MAX_TEXT_CHARS).collect();
    let from = req.from.unwrap_or_else(|| "auto".to_string());
    let to = req.to;
    // ureq is sync → dedicated blocking thread (same pattern as webview preflight).
    let out = tokio::task::spawn_blocking(move || fetch_translation(&text, &from, &to)).await;
    match out {
        Ok(Ok((translated, detected_from))) => Json(TranslateResponse {
            translated,
            detected_from,
            truncated,
        })
        .into_response(),
        Ok(Err(msg)) => err(StatusCode::BAD_GATEWAY, &msg),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "translate task failed"),
    }
}

/// 2-8 chars, ascii alphanumeric or '-' — covers "en", "th", "zh-CN", "auto".
fn is_lang_code(s: &str) -> bool {
    (1..=8).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn gtx_url(text: &str, from: &str, to: &str) -> String {
    format!(
        "https://translate.googleapis.com/translate_a/single?client=gtx&dt=t&sl={from}&tl={to}&q={}",
        utf8_percent_encode(text, NON_ALPHANUMERIC)
    )
}

/// gtx body shape: `[[["translated","source",…], …], null, "<detected>", …]`.
/// Segment 0-strings concatenate into the full translation.
fn parse_gtx(body: &str) -> Result<(String, String), String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "unexpected translate response".to_string())?;
    let segs = v
        .get(0)
        .and_then(|s| s.as_array())
        .ok_or("unexpected translate response")?;
    let mut translated = String::new();
    for seg in segs {
        if let Some(t) = seg.get(0).and_then(|t| t.as_str()) {
            translated.push_str(t);
        }
    }
    if translated.is_empty() {
        return Err("empty translation".to_string());
    }
    let detected = v
        .get(2)
        .and_then(|d| d.as_str())
        .unwrap_or("auto")
        .to_string();
    Ok((translated, detected))
}

fn fetch_translation(text: &str, from: &str, to: &str) -> Result<(String, String), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(8)))
        .build()
        .into();
    let mut resp = agent
        .get(&gtx_url(text, from, to))
        .call()
        .map_err(|_| "translate service unreachable".to_string())?;
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|_| "translate service unreachable".to_string())?;
    parse_gtx(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gtx_url_encodes_query_and_langs() {
        let u = gtx_url("hello world", "auto", "th");
        assert!(u.starts_with(
            "https://translate.googleapis.com/translate_a/single?client=gtx&dt=t&sl=auto&tl=th&q="
        ));
        assert!(u.ends_with("hello%20world"));
    }

    #[test]
    fn parse_gtx_joins_segments_and_reads_detected_lang() {
        // Real shape captured 2026-07-02: [[["seg1","src1",...],["seg2","src2",...]], null, "en", ...]
        let body = r#"[[["CRDT ผสาน","CRDT merges",null,null,3],["การแก้ไข","edits",null,null,3]],null,"en",null,null,null,null,[]]"#;
        let (t, d) = parse_gtx(body).unwrap();
        assert_eq!(t, "CRDT ผสานการแก้ไข");
        assert_eq!(d, "en");
    }

    #[test]
    fn parse_gtx_rejects_garbage() {
        assert!(parse_gtx("not json").is_err());
        assert!(parse_gtx(r#"{"unexpected":true}"#).is_err());
        assert!(parse_gtx(r#"[[],null,"en"]"#).is_err()); // no segments → empty translation
    }

    #[test]
    fn lang_code_allowlist() {
        assert!(
            is_lang_code("th")
                && is_lang_code("en")
                && is_lang_code("auto")
                && is_lang_code("zh-CN")
        );
        assert!(!is_lang_code("") && !is_lang_code("th th") && !is_lang_code("verylonglangcode"));
    }

    #[tokio::test]
    async fn empty_text_is_bad_request() {
        let resp = post_translate(axum::Json(TranslateRequest {
            text: "  ".into(),
            from: None,
            to: "th".into(),
        }))
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn bad_lang_code_is_bad_request() {
        let resp = post_translate(axum::Json(TranslateRequest {
            text: "hi".into(),
            from: Some("no pe".into()),
            to: "th".into(),
        }))
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    }
}
