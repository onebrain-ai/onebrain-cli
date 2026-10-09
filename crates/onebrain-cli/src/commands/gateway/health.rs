//! Gateway health — shared by `doctor`, `gateway service status`, and
//! `gateway tunnel status` (v3.5.0 T3).

use std::time::Duration;

pub(crate) use super::service::HttpProbe;

/// Real [`HttpProbe`]: blocking ureq GET, short global timeout, any non-200
/// or non-JSON body is an `Err` (never a panic).
pub(crate) struct UreqProbe {
    agent: ureq::Agent,
}

impl UreqProbe {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(timeout))
                .http_status_as_error(false)
                .build()
                .into(),
        }
    }
}

impl HttpProbe for UreqProbe {
    fn get_json(&self, url: &str) -> Result<serde_json::Value, String> {
        let mut resp = self.agent.get(url).call().map_err(|e| e.to_string())?;
        if resp.status() != 200 {
            return Err(format!("HTTP {}", resp.status().as_u16()));
        }
        let body = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())?;
        serde_json::from_str(&body).map_err(|e| format!("not JSON: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One-shot std TCP server answering `status`/`body` to the first request.
    fn serve_once(status: &str, body: &str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let reply = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            s.write_all(reply.as_bytes()).unwrap();
        });
        format!("http://{addr}/")
    }

    #[test]
    fn ureq_probe_parses_json_and_reports_failures_as_err() {
        let p = UreqProbe::new(Duration::from_secs(5));
        assert_eq!(
            p.get_json(&serve_once("200 OK", r#"{"issuer":"x"}"#))
                .unwrap()["issuer"],
            "x"
        );
        assert!(p
            .get_json(&serve_once("503 Service Unavailable", "{}"))
            .unwrap_err()
            .contains("503"));
        assert!(p
            .get_json(&serve_once("200 OK", "nope"))
            .unwrap_err()
            .contains("not JSON"));
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        assert!(p.get_json(&format!("http://{closed}/")).is_err());
    }
}
