//! Gateway health — shared by `doctor`, `gateway service status`, and
//! `gateway tunnel status` (v3.5.0 T3).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use onebrain_core::{DoctorResult, DoctorStatus};

use super::config::{load_gateway_config_at, GatewayConfig};
use super::policy::MAX_APPROVAL_WAIT_SECONDS;
pub(crate) use super::service::HttpProbe;
use super::service::{local_probe_url, Launcher, ServicePaths, PROBE_TIMEOUT};
use super::service_plist::{GATEWAY_LABEL, TUNNEL_LABEL};

pub(crate) const CHECK_CONFIG: &str = "gateway-config";
pub(crate) const CHECK_SERVICE: &str = "gateway-service";
pub(crate) const CHECK_LOCAL: &str = "gateway-local";
pub(crate) const CHECK_TUNNEL: &str = "gateway-tunnel";
pub(crate) const CHECK_AUTH: &str = "gateway-auth-store";
pub(crate) const CHECK_TELEGRAM: &str = "gateway-telegram";
pub(crate) const CHECK_APPROVAL_WAIT: &str = "gateway-approval-wait";
pub(crate) const CHECK_VAULT_LOCATION: &str = "gateway-vault-location";
#[cfg(test)]
pub(crate) const ALL_CHECKS: [&str; 8] = [
    CHECK_CONFIG,
    CHECK_SERVICE,
    CHECK_LOCAL,
    CHECK_TUNNEL,
    CHECK_AUTH,
    CHECK_TELEGRAM,
    CHECK_APPROVAL_WAIT,
    CHECK_VAULT_LOCATION,
];

/// The tunnel hostname probe crosses the network — more patient than local.
const TUNNEL_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// First `<string>` after `<key>ProgramArguments</key>` — the program a
/// LaunchAgent runs. `None` for anything that is not our rendered plist.
pub(crate) fn plist_program(xml: &str) -> Option<PathBuf> {
    let rest = &xml[xml.find("<key>ProgramArguments</key>")?..];
    let start = rest.find("<string>")? + "<string>".len();
    let end = rest[start..].find("</string>")? + start;
    Some(PathBuf::from(&rest[start..end]))
}

/// Which macOS privacy-protected (TCC) folder `vault` sits in, if any: a
/// LaunchAgent may be denied access there even though a terminal-started
/// `gateway run` works (red-team item 11). Component-wise, so
/// `~/DocumentsX` is not `~/Documents`.
pub(crate) fn protected_vault_folder(home: &Path, vault: &Path) -> Option<&'static str> {
    [
        ("Documents", "~/Documents"),
        ("Desktop", "~/Desktop"),
        ("Library/Mobile Documents", "iCloud Drive"),
    ]
    .into_iter()
    .find(|(rel, _)| vault.starts_with(home.join(rel)))
    .map(|(_, label)| label)
}

pub(crate) struct HealthEnv<'a> {
    pub onebrain_dir: &'a Path,
    /// `Some` only where `gateway service` exists (macOS).
    pub service: Option<&'a ServicePaths>,
    pub launcher: &'a dyn Launcher,
    pub http: &'a dyn HttpProbe,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AgentState {
    NotLoaded,
    Loaded,
    Running(u32),
}

pub(crate) fn agent_state(print: Option<&str>) -> AgentState {
    let Some(text) = print else {
        return AgentState::NotLoaded;
    };
    let field = |name: &str| {
        text.lines()
            .map(str::trim)
            .find_map(|l| {
                l.strip_prefix(name)
                    .and_then(|r| r.trim_start().strip_prefix('='))
            })
            .map(str::trim)
    };
    match (field("state"), field("pid").and_then(|p| p.parse().ok())) {
        (Some("running"), Some(pid)) => AgentState::Running(pid),
        _ => AgentState::Loaded,
    }
}

/// `None` when `~/.onebrain/gateway.yml` does not exist: a user who never
/// set up the gateway gets no gateway section (and doctor's all-green tests
/// stay all-green).
pub(crate) fn collect(env: &HealthEnv) -> Option<Vec<DoctorResult>> {
    let yml = env.onebrain_dir.join("gateway.yml");
    if !yml.exists() {
        return None;
    }
    let mut rows = Vec::new();
    let config = match load_gateway_config_at(&yml)
        .and_then(|c| super::validate_gateway_config(&c).map(|()| c))
    {
        Ok(c) => {
            rows.push(DoctorResult::ok(
                CHECK_CONFIG,
                format!("{} parses", yml.display()),
            ));
            Some(c)
        }
        Err(e) => {
            rows.push(
                DoctorResult::error(CHECK_CONFIG, format!("{e:#}"))
                    .with_hint(format!("fix {}", yml.display())),
            );
            None
        }
    };
    let service_installed = env.service.is_some_and(|p| p.plist(GATEWAY_LABEL).exists());
    if let Some(cfg) = &config {
        if let Some(paths) = env.service {
            rows.push(service_row(paths, env.launcher, cfg));
        }
        rows.push(local_row(env.http, cfg, service_installed));
        rows.push(tunnel_row(env, cfg));
        if let Some(paths) = env.service {
            rows.push(vault_location_row(&paths.home, cfg));
        }
    }
    rows.push(
        match super::auth::store::check_files_parse(&env.onebrain_dir.join("gateway")) {
            Ok(()) => DoctorResult::ok(CHECK_AUTH, "auth store files parse"),
            Err(e) => DoctorResult::error(CHECK_AUTH, format!("{e:#}")).with_hint(
                "move the broken file aside; the gateway recreates it (connectors must pair again)",
            ),
        },
    );
    if let Some(cfg) = &config {
        rows.push(if super::telegram::is_available(&cfg.telegram) {
            DoctorResult::ok(CHECK_TELEGRAM, "Telegram approvals configured")
        } else {
            DoctorResult::warn(
                CHECK_TELEGRAM,
                "no Telegram approval channel — away from the Mac nobody can approve a write",
            )
            .with_hint("onebrain gateway telegram setup")
        });
        let wait = cfg.policy.approval_wait_seconds;
        rows.push(if wait > MAX_APPROVAL_WAIT_SECONDS {
            DoctorResult::warn(
                CHECK_APPROVAL_WAIT,
                format!(
                    "approval_wait_seconds is {wait}s; the gateway clamps it to {MAX_APPROVAL_WAIT_SECONDS}s"
                ),
            )
            .with_hint(format!(
                "set policy.approval_wait_seconds ≤ {MAX_APPROVAL_WAIT_SECONDS} in {}",
                yml.display()
            ))
        } else if wait == 0 {
            DoctorResult::warn(
                CHECK_APPROVAL_WAIT,
                "approval_wait_seconds is 0 — every gated call is refused at once",
            )
        } else {
            DoctorResult::ok(
                CHECK_APPROVAL_WAIT,
                format!("{wait}s (≤ {MAX_APPROVAL_WAIT_SECONDS}s)"),
            )
        });
    }
    Some(rows)
}

fn service_row(paths: &ServicePaths, launcher: &dyn Launcher, cfg: &GatewayConfig) -> DoctorResult {
    let mut labels = vec![GATEWAY_LABEL];
    if cfg.public_url.is_some() {
        labels.push(TUNNEL_LABEL);
    }
    if labels.iter().all(|l| !paths.plist(l).exists()) {
        return DoctorResult::warn(
            CHECK_SERVICE,
            "not installed — the gateway stops when you log out",
        )
        .with_hint("onebrain gateway service install");
    }
    let mut worst = DoctorStatus::Ok;
    let mut parts = Vec::new();
    for label in labels {
        if !paths.plist(label).exists() {
            worst = DoctorStatus::Error;
            parts.push(format!("com.onebrain.{label} not installed"));
            continue;
        }
        let xml = std::fs::read_to_string(paths.plist(label)).unwrap_or_default();
        // A plist whose program vanished (e.g. a Cellar path after
        // `brew upgrade`) can never start — say so, with the fix.
        if let Some(program) = plist_program(&xml).filter(|p| !p.exists()) {
            worst = DoctorStatus::Error;
            parts.push(format!(
                "com.onebrain.{label} runs {} which no longer exists",
                program.display()
            ));
            continue;
        }
        if label == TUNNEL_LABEL {
            if xml.contains("TUNNEL_TOKEN") {
                parts.push(
                    "tunnel token via TUNNEL_TOKEN env (cloudflared has no --token-file)"
                        .to_string(),
                );
            } else if xml.contains("--token-file") {
                parts.push("tunnel token via --token-file".to_string());
            }
        }
        match agent_state(launcher.print(label).as_deref()) {
            AgentState::Running(pid) => {
                parts.push(format!("com.onebrain.{label} running (pid {pid})"))
            }
            AgentState::Loaded => {
                worst = DoctorStatus::Error;
                parts.push(format!(
                    "com.onebrain.{label} loaded but not running — see {}",
                    paths.log_dir.display()
                ));
            }
            AgentState::NotLoaded => {
                worst = DoctorStatus::Error;
                parts.push(format!("com.onebrain.{label} not loaded"));
            }
        }
    }
    let msg = parts.join(" · ");
    match worst {
        DoctorStatus::Ok => DoctorResult::ok(CHECK_SERVICE, msg),
        _ => DoctorResult::error(CHECK_SERVICE, msg).with_hint("onebrain gateway service install"),
    }
}

fn local_row(http: &dyn HttpProbe, cfg: &GatewayConfig, service_installed: bool) -> DoctorResult {
    match http.get_json(&local_probe_url(cfg.port), PROBE_TIMEOUT) {
        Ok(_) => DoctorResult::ok(CHECK_LOCAL, format!("answering on 127.0.0.1:{}", cfg.port)),
        Err(e) if service_installed => DoctorResult::error(
            CHECK_LOCAL,
            format!("not answering on 127.0.0.1:{} ({e})", cfg.port),
        )
        .with_hint("see ~/Library/Logs/onebrain/gateway.log"),
        Err(_) => DoctorResult::warn(
            CHECK_LOCAL,
            format!("not running on 127.0.0.1:{}", cfg.port),
        )
        .with_hint("onebrain gateway service install (or onebrain gateway run)"),
    }
}

fn tunnel_row(env: &HealthEnv, cfg: &GatewayConfig) -> DoctorResult {
    let Some(public_url) = cfg.public_url.as_deref() else {
        return DoctorResult::ok(CHECK_TUNNEL, "not configured — loopback only");
    };
    let issuer = public_url.trim_end_matches('/');
    let token = super::tunnel::tunnel_token_path(env.onebrain_dir);
    if !token.exists() {
        return DoctorResult::error(
            CHECK_TUNNEL,
            format!("public_url is set but {} is missing", token.display()),
        )
        .with_hint("onebrain gateway tunnel setup");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&token)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0);
        if mode != 0o600 {
            return DoctorResult::error(
                CHECK_TUNNEL,
                format!("{} is mode {mode:o}, not 600", token.display()),
            )
            .with_hint(format!("chmod 600 {}", token.display()));
        }
    }
    let url = format!("{issuer}/.well-known/oauth-authorization-server");
    match env.http.get_json(&url, TUNNEL_PROBE_TIMEOUT) {
        Err(e) => DoctorResult::warn(
            CHECK_TUNNEL,
            format!("{issuer} not reachable from here ({e})"),
        )
        .with_hint(
            "check `onebrain gateway service status` and the Cloudflare dashboard's tunnel health",
        ),
        Ok(v) if v["issuer"].as_str() == Some(issuer) => {
            DoctorResult::ok(CHECK_TUNNEL, format!("{issuer} reaches this gateway"))
        }
        Ok(v) => DoctorResult::error(
            CHECK_TUNNEL,
            format!(
                "{issuer} answered with issuer {:?} — not this gateway",
                v["issuer"].as_str().unwrap_or("(none)")
            ),
        )
        .with_hint(format!(
            "in the dashboard, point the public hostname at http://127.0.0.1:{}, then `onebrain gateway service install`",
            cfg.port
        )),
    }
}

/// Red-team item 11 — macOS only (called when `env.service` is `Some`).
fn vault_location_row(home: &Path, cfg: &GatewayConfig) -> DoctorResult {
    for vault in cfg.default_vault.iter().chain(cfg.vaults.values()) {
        if let Some(folder) = protected_vault_folder(home, vault) {
            return DoctorResult::warn(
                CHECK_VAULT_LOCATION,
                format!(
                    "{} is inside {folder} — macOS may block the gateway LaunchAgent from reading it",
                    vault.display()
                ),
            )
            .with_hint("move the vault out of ~/Documents, ~/Desktop and iCloud Drive, or grant the onebrain binary Full Disk Access (System Settings → Privacy & Security)");
        }
    }
    DoctorResult::ok(
        CHECK_VAULT_LOCATION,
        "vaults are outside macOS-protected folders",
    )
}

/// Production rows for `doctor`.
pub(crate) fn doctor_rows() -> Vec<DoctorResult> {
    let Ok(home) = crate::home::home_dir() else {
        return Vec::new();
    };
    let paths = ServicePaths::for_home(&home);
    let launcher = super::service::system_launcher();
    let env = HealthEnv {
        onebrain_dir: &paths.onebrain_dir,
        service: super::service::service_supported().then_some(&paths),
        launcher: launcher.as_ref(),
        http: &UreqProbe,
    };
    collect(&env).unwrap_or_default()
}

/// Print `rows` (`✓/⚠/✗ name — message`, `  └ hint`); returns the error count.
pub(crate) fn render_rows(rows: &[DoctorResult], out: &mut dyn Write) -> std::io::Result<usize> {
    let mut errors = 0;
    for r in rows {
        let glyph = match r.status {
            DoctorStatus::Ok => "✓",
            DoctorStatus::Warn => "⚠",
            DoctorStatus::Error => {
                errors += 1;
                "✗"
            }
        };
        writeln!(out, "{glyph} {} — {}", r.check, r.message)?;
        if let Some(h) = &r.hint {
            writeln!(out, "  └ {h}")?;
        }
    }
    Ok(errors)
}

/// Shared body of `service status` / `tunnel status`: print the named rows;
/// fail (exit non-zero) if any is an error.
pub(crate) fn print_status(names: &[&str]) -> anyhow::Result<()> {
    let mut out = std::io::stdout();
    let rows: Vec<DoctorResult> = doctor_rows()
        .into_iter()
        .filter(|r| names.contains(&r.check.as_str()))
        .collect();
    if rows.is_empty() {
        writeln!(
            out,
            "gateway not configured (~/.onebrain/gateway.yml missing) — start with `onebrain gateway tunnel setup`"
        )?;
        return Ok(());
    }
    let errors = render_rows(&rows, &mut out)?;
    if errors > 0 {
        anyhow::bail!("{errors} gateway check(s) failed");
    }
    Ok(())
}

/// Real [`HttpProbe`]: blocking ureq GET, per-call global timeout, any
/// non-200 or non-JSON body is an `Err` (never a panic).
pub(crate) struct UreqProbe;

impl HttpProbe for UreqProbe {
    fn get_json(&self, url: &str, timeout: Duration) -> Result<serde_json::Value, String> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build()
            .into();
        let mut resp = agent.get(url).call().map_err(|e| e.to_string())?;
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

    use crate::commands::gateway::service::{Launcher, ServicePaths};
    use onebrain_core::DoctorStatus;
    use std::collections::HashMap;
    use std::path::Path;

    struct Fakes {
        loaded: HashMap<&'static str, &'static str>,
        http: HashMap<String, Result<serde_json::Value, String>>,
    }
    impl Launcher for Fakes {
        fn bootout(&self, _: &str) {}
        fn bootstrap(&self, _: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn print(&self, label: &str) -> Option<String> {
            self.loaded.get(label).map(|s| s.to_string())
        }
    }
    impl HttpProbe for Fakes {
        fn get_json(&self, url: &str, _: Duration) -> Result<serde_json::Value, String> {
            self.http
                .get(url)
                .cloned()
                .unwrap_or_else(|| Err("connection refused".into()))
        }
    }

    const TOKEN: &str = "eyJhIjoiMDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWYiLCJ0IjoiNmZmNDJhZTItNzY1ZC00YWRmLTgxMTItMzFjNTVjMTU1MWVmIiwicyI6ImJtOTBMV0V0Y21WaGJDMXpaV055WlhRPSJ9";
    const RUNNING: &str = "state = running\npid = 4242\n";
    const LOCAL: &str = "http://127.0.0.1:7717/.well-known/oauth-authorization-server";
    const PUBLIC: &str = "https://brain.example.com/.well-known/oauth-authorization-server";

    fn healthy_home() -> (tempfile::TempDir, ServicePaths) {
        let root = tempfile::tempdir().unwrap();
        let paths = ServicePaths::for_home(root.path());
        let gw = paths.onebrain_dir.join("gateway");
        std::fs::create_dir_all(&gw).unwrap();
        std::fs::write(
            paths.onebrain_dir.join("gateway.yml"),
            "default_vault: /v\npublic_url: 'https://brain.example.com'\ntelegram:\n  bot_token: 'x'\n  chat_id: 5\n",
        )
        .unwrap();
        crate::commands::gateway::config_write::write_private_file(
            &gw.join("tunnel.token"),
            TOKEN.as_bytes(),
        )
        .unwrap();
        std::fs::create_dir_all(paths.plist("x").parent().unwrap()).unwrap();
        std::fs::write(paths.plist("gateway"), "x").unwrap();
        std::fs::write(paths.plist("gateway-tunnel"), "x").unwrap();
        (root, paths)
    }

    fn green_fakes() -> Fakes {
        let mut http = HashMap::new();
        for url in [LOCAL, PUBLIC] {
            http.insert(
                url.to_string(),
                Ok(serde_json::json!({"issuer": "https://brain.example.com"})),
            );
        }
        Fakes {
            loaded: HashMap::from([("gateway", RUNNING), ("gateway-tunnel", RUNNING)]),
            http,
        }
    }

    fn rows(paths: &ServicePaths, f: &Fakes) -> HashMap<String, (DoctorStatus, String)> {
        let env = HealthEnv {
            onebrain_dir: &paths.onebrain_dir,
            service: Some(paths),
            launcher: f,
            http: f,
        };
        collect(&env)
            .expect("gateway.yml present")
            .into_iter()
            .map(|r| (r.check.clone(), (r.status, r.message.clone())))
            .collect()
    }

    #[test]
    fn a_fully_set_up_gateway_is_all_green() {
        let (_r, paths) = healthy_home();
        let got = rows(&paths, &green_fakes());
        for name in ALL_CHECKS {
            assert_eq!(got[name].0, DoctorStatus::Ok, "{name}: {}", got[name].1);
        }
        assert!(got[CHECK_SERVICE].1.contains("4242"));
    }

    #[test]
    fn no_gateway_yml_means_no_gateway_section_at_all() {
        let root = tempfile::tempdir().unwrap();
        let paths = ServicePaths::for_home(root.path());
        let f = green_fakes();
        let env = HealthEnv {
            onebrain_dir: &paths.onebrain_dir,
            service: Some(&paths),
            launcher: &f,
            http: &f,
        };
        assert!(collect(&env).is_none());
    }

    #[test]
    fn an_unreachable_tunnel_is_a_warning_not_an_error() {
        let (_r, paths) = healthy_home();
        let mut f = green_fakes();
        f.http.remove(PUBLIC);
        assert_eq!(rows(&paths, &f)[CHECK_TUNNEL].0, DoctorStatus::Warn);
    }

    #[test]
    fn a_tunnel_answering_with_another_issuer_is_an_error() {
        let (_r, paths) = healthy_home();
        let mut f = green_fakes();
        f.http.insert(
            PUBLIC.into(),
            Ok(serde_json::json!({"issuer": "https://elsewhere.example.com"})),
        );
        let got = rows(&paths, &f);
        assert_eq!(got[CHECK_TUNNEL].0, DoctorStatus::Error);
        assert!(got[CHECK_TUNNEL].1.contains("elsewhere.example.com"));
        let hint = collect(&HealthEnv {
            onebrain_dir: &paths.onebrain_dir,
            service: Some(&paths),
            launcher: &f,
            http: &f,
        })
        .unwrap()
        .into_iter()
        .find(|r| r.check == CHECK_TUNNEL)
        .unwrap()
        .hint
        .unwrap();
        assert!(hint.contains("http://127.0.0.1:7717") && !hint.contains("localhost"));
    }

    #[cfg(unix)]
    #[test]
    fn a_world_readable_token_file_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let (_r, paths) = healthy_home();
        let token = paths.onebrain_dir.join("gateway/tunnel.token");
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            rows(&paths, &green_fakes())[CHECK_TUNNEL].0,
            DoctorStatus::Error
        );
    }

    #[test]
    fn a_missing_token_with_public_url_set_is_an_error() {
        let (_r, paths) = healthy_home();
        std::fs::remove_file(paths.onebrain_dir.join("gateway/tunnel.token")).unwrap();
        assert_eq!(
            rows(&paths, &green_fakes())[CHECK_TUNNEL].0,
            DoctorStatus::Error
        );
    }

    #[test]
    fn agents_installed_but_not_loaded_is_an_error_and_not_installed_is_a_warning() {
        let (_r, paths) = healthy_home();
        let mut f = green_fakes();
        f.loaded.clear();
        assert_eq!(rows(&paths, &f)[CHECK_SERVICE].0, DoctorStatus::Error);
        std::fs::remove_file(paths.plist("gateway")).unwrap();
        std::fs::remove_file(paths.plist("gateway-tunnel")).unwrap();
        assert_eq!(rows(&paths, &f)[CHECK_SERVICE].0, DoctorStatus::Warn);
    }

    #[test]
    fn local_down_is_an_error_under_the_service_and_a_warning_without_it() {
        let (_r, paths) = healthy_home();
        let mut f = green_fakes();
        f.http.remove(LOCAL);
        assert_eq!(rows(&paths, &f)[CHECK_LOCAL].0, DoctorStatus::Error);
        f.loaded.clear();
        std::fs::remove_file(paths.plist("gateway")).unwrap();
        assert_eq!(rows(&paths, &f)[CHECK_LOCAL].0, DoctorStatus::Warn);
    }

    #[test]
    fn no_telegram_and_an_oversized_wait_are_warnings() {
        let (_r, paths) = healthy_home();
        std::fs::write(
            paths.onebrain_dir.join("gateway.yml"),
            "default_vault: /v\npublic_url: 'https://brain.example.com'\npolicy:\n  approval_wait_seconds: 900\n",
        )
        .unwrap();
        let got = rows(&paths, &green_fakes());
        assert_eq!(got[CHECK_TELEGRAM].0, DoctorStatus::Warn);
        assert_eq!(got[CHECK_APPROVAL_WAIT].0, DoctorStatus::Warn);
        assert!(got[CHECK_APPROVAL_WAIT].1.contains("270"));
    }

    #[test]
    fn an_unparsable_gateway_yml_and_a_broken_auth_store_are_errors() {
        let (_r, paths) = healthy_home();
        std::fs::write(paths.onebrain_dir.join("gateway.yml"), "port: [\n").unwrap();
        std::fs::write(paths.onebrain_dir.join("gateway/tokens.json"), "[1,2,").unwrap();
        let got = rows(&paths, &green_fakes());
        assert_eq!(got[CHECK_CONFIG].0, DoctorStatus::Error);
        assert_eq!(got[CHECK_AUTH].0, DoctorStatus::Error);
        assert!(
            !got.contains_key(CHECK_TUNNEL),
            "config-dependent rows need a config"
        );
    }

    #[test]
    fn off_macos_there_is_no_service_row() {
        let (_r, paths) = healthy_home();
        let f = green_fakes();
        let env = HealthEnv {
            onebrain_dir: &paths.onebrain_dir,
            service: None,
            launcher: &f,
            http: &f,
        };
        let rows = collect(&env).unwrap();
        assert!(rows.iter().all(|r| r.check != CHECK_SERVICE));
        assert!(
            rows.iter().all(|r| r.check != CHECK_VAULT_LOCATION),
            "TCC is a LaunchAgent (macOS) concern"
        );
    }

    #[test]
    fn a_plist_whose_program_no_longer_exists_is_an_error() {
        let (r, paths) = healthy_home();
        let gone = r.path().join("Cellar/onebrain/3.4.0/bin/onebrain");
        std::fs::write(
            paths.plist("gateway"),
            format!(
                "<key>ProgramArguments</key>\n    <array>\n        <string>{}</string>\n        <string>gateway</string>\n",
                gone.display()
            ),
        )
        .unwrap();
        let got = rows(&paths, &green_fakes());
        assert_eq!(
            got[CHECK_SERVICE].0,
            DoctorStatus::Error,
            "{}",
            got[CHECK_SERVICE].1
        );
        assert!(
            got[CHECK_SERVICE].1.contains("no longer exists"),
            "{}",
            got[CHECK_SERVICE].1
        );
    }

    #[test]
    fn service_row_names_the_tunnel_token_mode() {
        let (_r, paths) = healthy_home();
        std::fs::write(
            paths.plist("gateway-tunnel"),
            "<string>--token-file</string>",
        )
        .unwrap();
        assert!(rows(&paths, &green_fakes())[CHECK_SERVICE]
            .1
            .contains("token via --token-file"));
        std::fs::write(paths.plist("gateway-tunnel"), "<key>TUNNEL_TOKEN</key>").unwrap();
        assert!(rows(&paths, &green_fakes())[CHECK_SERVICE]
            .1
            .contains("token via TUNNEL_TOKEN env"));
    }

    #[test]
    fn a_vault_in_a_tcc_protected_folder_is_a_warning() {
        let home = Path::new("/Users/test");
        assert_eq!(
            protected_vault_folder(home, Path::new("/Users/test/Documents/brain")),
            Some("~/Documents")
        );
        assert_eq!(
            protected_vault_folder(home, Path::new("/Users/test/Desktop/brain")),
            Some("~/Desktop")
        );
        assert_eq!(
            protected_vault_folder(
                home,
                Path::new("/Users/test/Library/Mobile Documents/com~apple~CloudDocs/brain")
            ),
            Some("iCloud Drive")
        );
        assert_eq!(
            protected_vault_folder(home, Path::new("/Users/test/brain")),
            None
        );
        assert_eq!(
            protected_vault_folder(home, Path::new("/Users/test/DocumentsX/brain")),
            None
        );

        let (_r, paths) = healthy_home();
        let vault = paths.home.join("Documents/brain");
        std::fs::write(
            paths.onebrain_dir.join("gateway.yml"),
            format!(
                "default_vault: {}\npublic_url: 'https://brain.example.com'\ntelegram:\n  bot_token: 'x'\n  chat_id: 5\n",
                vault.display()
            ),
        )
        .unwrap();
        let got = rows(&paths, &green_fakes());
        assert_eq!(got[CHECK_VAULT_LOCATION].0, DoctorStatus::Warn);
        assert!(
            got[CHECK_VAULT_LOCATION].1.contains("~/Documents"),
            "{}",
            got[CHECK_VAULT_LOCATION].1
        );
    }

    #[test]
    fn no_row_ever_carries_the_tunnel_token() {
        let (_r, paths) = healthy_home();
        let mut f = green_fakes();
        f.http.remove(PUBLIC);
        f.loaded.clear();
        for (status, msg) in rows(&paths, &f).values() {
            assert!(
                !msg.contains(TOKEN),
                "a row leaked the tunnel token ({status:?})"
            );
        }
    }

    #[test]
    fn launchctl_print_parsing() {
        assert_eq!(agent_state(Some(RUNNING)), AgentState::Running(4242));
        assert_eq!(agent_state(Some("state = waiting\n")), AgentState::Loaded);
        assert_eq!(agent_state(None), AgentState::NotLoaded);
    }

    #[test]
    fn render_rows_counts_errors_and_prints_hints() {
        let rows = vec![
            DoctorResult::ok("gateway-local", "up"),
            DoctorResult::error("gateway-tunnel", "down").with_hint("run setup"),
        ];
        let mut out = Vec::new();
        assert_eq!(render_rows(&rows, &mut out).unwrap(), 1);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("✓ gateway-local — up")
                && text.contains("✗ gateway-tunnel — down")
                && text.contains("└ run setup"),
            "{text}"
        );
    }

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
        let p = UreqProbe;
        let t = Duration::from_secs(5);
        assert_eq!(
            p.get_json(&serve_once("200 OK", r#"{"issuer":"x"}"#), t)
                .unwrap()["issuer"],
            "x"
        );
        assert!(p
            .get_json(&serve_once("503 Service Unavailable", "{}"), t)
            .unwrap_err()
            .contains("503"));
        assert!(p
            .get_json(&serve_once("200 OK", "nope"), t)
            .unwrap_err()
            .contains("not JSON"));
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        assert!(p.get_json(&format!("http://{closed}/"), t).is_err());
    }
}
