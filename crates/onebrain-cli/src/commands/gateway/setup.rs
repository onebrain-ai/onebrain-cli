//! `onebrain gateway setup` — the guided wizard (v3.5.2, #423, design D1–D10).
//!
//! Steps: vault → connection (Cloudflare own domain · existing tunnel token
//! · Quick Tunnel · local only) → Telegram (optional) → service → finish
//! screen (connector URL, pairing code and QR — the last two on a TTY only).
//!
//! Re-run is idempotent (D2): every step detects what is already there and
//! offers to keep it (Enter). Nothing is written until the connection step
//! has fully validated (the DNS route included), and the service is only
//! reinstalled — which restarts the gateway and denies pending approvals —
//! when this run wrote something or the agents drifted (A-I1).
//!
//! Telegram runs BEFORE the service step (D1 lists it after): the gateway
//! reads its `telegram:` block at startup, so a Telegram set up after the
//! install would need a second restart.
//!
//! The testable core is [`run_setup`] over `&mut dyn BufRead` /
//! `&mut dyn Write` and an injected [`SetupHost`]; [`SystemSetupHost`] and
//! [`gateway_setup`] are thin shells over it.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::cloudflare::{self, Cloudflared, RouteDns};
use super::config::{load_gateway_config_at, GatewayConfig, TunnelIngress};
use super::service::{self, HttpProbe, PairingLookup};
use super::{tunnel, RunOverrides};

/// How long `cloudflared` gets to print its Quick Tunnel URL (spec).
pub(crate) const QUICK_URL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the optional Quick Tunnel reachability check polls (A-T0a: a
/// fresh trycloudflare hostname took ≥ 40 s to resolve in one live probe).
pub(crate) const REACHABLE_WAIT: Duration = Duration::from_secs(60);

const RERUN: &str = "Re-run: onebrain gateway setup (safe — finished steps are kept)";

/// A prompt hit end of input (stdin EOF / closed pipe): the wizard was
/// cancelled — never read as "Enter = default" (A-I4).
#[derive(Debug)]
pub(crate) struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("setup cancelled — input ended before an answer")
    }
}

impl std::error::Error for Cancelled {}

/// What the connection step decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Connection {
    /// A tunnel is configured and the user kept it.
    Keep,
    OwnDomain,
    PasteToken,
    Quick,
    LocalOnly,
}

/// Everything [`run_setup`] needs from the machine, injectable.
pub(crate) trait SetupHost {
    fn cloudflared(&self) -> &dyn Cloudflared;
    /// `cloudflared tunnel run --help` advertises `--token-file`.
    fn token_file_supported(&self, cloudflared: &Path) -> bool;
    /// Read the tunnel token: no echo on a TTY, a plain line otherwise.
    fn read_secret(
        &self,
        prompt: &str,
        input: &mut dyn BufRead,
        out: &mut dyn Write,
    ) -> anyhow::Result<String>;
    /// `gateway service` exists on this OS (macOS only, D9).
    fn service_supported(&self) -> bool;
    /// Both agents loaded with plists matching a fresh render
    /// (`service::agents_current`).
    fn service_current(&self) -> bool;
    /// `service::install_service` with the system launcher / probe and
    /// `print_finish: false` (the wizard prints its own finish screen).
    fn install_service(&self, out: &mut dyn Write) -> anyhow::Result<()>;
    fn port_in_use(&self, port: u16) -> bool;
    /// `run_with(.., Some(port), overrides)`; blocks until Ctrl-C/SIGTERM.
    fn run_foreground(&self, port: u16, overrides: RunOverrides) -> anyhow::Result<()>;
    /// `gateway telegram setup`'s wizard.
    fn telegram_setup(&self, input: &mut dyn BufRead, out: &mut dyn Write) -> anyhow::Result<()>;
    fn pairing(&self) -> PairingLookup;
    fn stdout_is_tty(&self) -> bool;
    fn cwd(&self) -> PathBuf;
}

fn hinted(plain: impl Into<String>, hint: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(crate::output::HintedError::new(plain, hint))
}

/// Print `prompt`, read one line, trimmed. A 0-byte read is [`Cancelled`].
pub(crate) fn read_answer(
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    prompt: &str,
) -> anyhow::Result<String> {
    write!(out, "{prompt}")?;
    out.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        writeln!(out)?;
        return Err(Cancelled.into());
    }
    Ok(line.trim().to_string())
}

/// `[Y/n]` / `[y/N]`: Enter = `default`; anything but y/yes/n/no re-asks,
/// three tries.
fn ask_yes_no(
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    prompt: &str,
    default: bool,
) -> anyhow::Result<bool> {
    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    for _ in 0..3 {
        match read_answer(input, out, &format!("{prompt} {suffix} "))?
            .to_ascii_lowercase()
            .as_str()
        {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => writeln!(out, "Please answer y or n.")?,
        }
    }
    anyhow::bail!("no valid answer (y or n) after 3 tries")
}

fn missing_cloudflared() -> anyhow::Error {
    hinted(
        "cloudflared is not on PATH",
        "brew install cloudflared, then run `onebrain gateway setup` again",
    )
}

/// `gateway.yml` + the token file as they were when the wizard started, so
/// the service step can tell whether this run wrote anything (A-I1).
struct Snapshot {
    yml: Option<Vec<u8>>,
    token: Option<Vec<u8>>,
}

impl Snapshot {
    fn take(yml: &Path, token: &Path) -> Self {
        Self {
            yml: std::fs::read(yml).ok(),
            token: std::fs::read(token).ok(),
        }
    }
    fn changed(&self, yml: &Path, token: &Path) -> bool {
        let now = Self::take(yml, token);
        now.yml != self.yml || now.token != self.token
    }
}

/// What the connection step will write, once everything validated.
enum Plan {
    Nothing,
    Store {
        token: String,
        public_url: String,
        ingress: Option<TunnelIngress>,
    },
    RemoveTunnel,
}

/// The wizard. On any error the steps already finished are listed with
/// the re-run line, then the error is returned (non-zero exit).
pub(crate) fn run_setup(
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    onebrain_dir: &Path,
    host: &dyn SetupHost,
) -> anyhow::Result<()> {
    let mut done = Vec::new();
    let result = wizard(input, out, onebrain_dir, host, &mut done);
    if result.is_err() {
        writeln!(out)?;
        if done.is_empty() {
            writeln!(out, "Done so far: nothing")?;
        } else {
            writeln!(out, "Done so far:")?;
            for step in &done {
                writeln!(out, "  ✓ {step}")?;
            }
        }
        writeln!(out, "{RERUN}")?;
    }
    result
}

fn step(out: &mut dyn Write, done: &mut Vec<String>, what: String) -> anyhow::Result<()> {
    writeln!(out, "✓ {what}")?;
    done.push(what);
    Ok(())
}

fn wizard(
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    onebrain_dir: &Path,
    host: &dyn SetupHost,
    done: &mut Vec<String>,
) -> anyhow::Result<()> {
    let yml = onebrain_dir.join("gateway.yml");
    let token_path = tunnel::tunnel_token_path(onebrain_dir);
    let snapshot = Snapshot::take(&yml, &token_path);
    let config = load_gateway_config_at(&yml)?;

    writeln!(out, "OneBrain gateway setup")?;
    writeln!(out)?;

    // 1. Vault — chosen now, written only after the connection validated
    //    (and never for a Quick Tunnel, which passes it in memory).
    let (vault, vault_to_write) = choose_vault(&config, input, out, host)?;
    step(out, done, format!("vault {}", vault.display()))?;

    // 2. Connection.
    let configured = config.public_url.clone().filter(|_| token_path.exists());
    let plan = match choose_connection(configured.as_deref(), input, out)? {
        Connection::Quick => return quick(&config, vault_to_write, out, host, done),
        Connection::Keep => {
            step(
                out,
                done,
                format!("tunnel kept ({})", configured.unwrap_or_default()),
            )?;
            Plan::Nothing
        }
        Connection::OwnDomain => own_domain(&config, input, out, host, done)?,
        Connection::PasteToken => paste_token(&config, input, out, host)?,
        Connection::LocalOnly => local_only(configured.as_deref(), &token_path, input, out)?,
    };

    // Everything validated: write.
    if let Some(v) = &vault_to_write {
        write_default_vault(&yml, v)?;
        step(out, done, format!("default_vault set to {}", v.display()))?;
    }
    match plan {
        Plan::Nothing => {}
        Plan::Store {
            token,
            public_url,
            ingress,
        } => {
            tunnel::store_tunnel(onebrain_dir, &token, &public_url, ingress)?;
            step(
                out,
                done,
                format!(
                    "tunnel token saved ({}, mode 0600); public_url set to {public_url}",
                    token_path.display()
                ),
            )?;
        }
        Plan::RemoveTunnel => {
            remove_tunnel(&yml)?;
            step(out, done, "tunnel removed from gateway.yml".to_string())?;
            writeln!(
                out,
                "  The token file ({}) and the Cloudflare tunnel are kept; delete them yourself if unused (`cloudflared tunnel delete <name>`).",
                token_path.display()
            )?;
        }
    }

    // 3. Telegram (before the service, so one install picks it up).
    let config = load_gateway_config_at(&yml)?;
    if !config.telegram.bot_token.is_empty() && config.telegram.chat_id > 0 {
        step(out, done, "Telegram already configured".to_string())?;
    } else if ask_yes_no(input, out, "Set up Telegram approvals now?", false)? {
        host.telegram_setup(input, out)?;
        step(out, done, "Telegram approvals configured".to_string())?;
    }

    // 4. Service.
    let config = load_gateway_config_at(&yml)?;
    let tunnel_on = config.public_url.is_some() && token_path.exists();
    if host.service_supported() {
        if !snapshot.changed(&yml, &token_path) && host.service_current() {
            step(
                out,
                done,
                "service already running (unchanged — not restarted)".to_string(),
            )?;
        } else {
            host.install_service(out)?;
            step(out, done, "service installed".to_string())?;
        }
    } else {
        writeln!(
            out,
            "No background service on {} yet (macOS only) — run these yourself:",
            std::env::consts::OS
        )?;
        let lines = if tunnel_on {
            let cf = host
                .cloudflared()
                .path()
                .unwrap_or_else(|| PathBuf::from("cloudflared"));
            tunnel::manual_run_lines(
                &cf,
                &token_path,
                host.token_file_supported(&cf),
                config.tunnel_origin().as_deref(),
            )
        } else {
            vec![
                "Next, keep this running:".to_string(),
                "  onebrain gateway run".to_string(),
            ]
        };
        for line in lines {
            writeln!(out, "{line}")?;
        }
    }

    // 5. Finish screen.
    writeln!(out)?;
    let tty = host.stdout_is_tty();
    let pairing = if tty {
        host.pairing()
    } else {
        PairingLookup::Unavailable
    };
    let code = match &pairing {
        PairingLookup::Code(c) => Some(c.as_str()),
        _ => None,
    };
    for mut line in service::finish_lines(config.public_url.as_deref(), config.port, code) {
        if pairing == PairingLookup::Busy && line.starts_with("Pairing code:") {
            line = service::PAIRING_BUSY_LINE.to_string();
        }
        writeln!(out, "{line}")?;
    }
    if let (true, Some(url)) = (tty, config.public_url.as_deref()) {
        print_qr(out, &connector_url(url))?;
    }
    Ok(())
}

fn connector_url(public_url: &str) -> String {
    format!("{}/mcp", public_url.trim_end_matches('/'))
}

fn print_qr(out: &mut dyn Write, url: &str) -> anyhow::Result<()> {
    // The QR carries the connector URL only — never the pairing code (D10).
    writeln!(out, "{}", super::qr::render_qr(url)?)?;
    Ok(())
}

fn choose_vault(
    config: &GatewayConfig,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    host: &dyn SetupHost,
) -> anyhow::Result<(PathBuf, Option<PathBuf>)> {
    if let Some(existing) = &config.default_vault {
        if !existing.as_os_str().is_empty() && onebrain_core::VaultRoot::from_path(existing).is_ok()
        {
            return Ok((existing.clone(), None));
        }
        writeln!(
            out,
            "default_vault {} is not a vault — choosing again",
            existing.display()
        )?;
    }
    let picked = tunnel::pick_vault(&host.cwd(), input, out)?;
    Ok((picked.clone(), Some(picked)))
}

/// `default_vault: <path>` into `gateway.yml` (as `ensure_default_vault`
/// renders it).
fn write_default_vault(yml: &Path, vault: &Path) -> anyhow::Result<()> {
    let value = serde_yaml::Value::String(
        vault
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("vault path is not valid UTF-8"))?
            .to_string(),
    );
    let mut one = serde_yaml::Mapping::new();
    one.insert("default_vault".into(), value.clone());
    let rendered = serde_yaml::to_string(&serde_yaml::Value::Mapping(one))?;
    super::config_write::set_top_level_key(yml, "default_vault", value, &rendered)?;
    Ok(())
}

fn choose_connection(
    configured: Option<&str>,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> anyhow::Result<Connection> {
    if let Some(url) = configured {
        if ask_yes_no(input, out, &format!("Keep tunnel {url}?"), true)? {
            return Ok(Connection::Keep);
        }
    }
    let (default, n) = if configured.is_some() {
        (Connection::OwnDomain, 1)
    } else {
        (Connection::LocalOnly, 4)
    };
    writeln!(out, "How should your phone reach the gateway?")?;
    writeln!(out, "  1) Cloudflare — your own domain")?;
    writeln!(out, "  2) Cloudflare — I already have a tunnel token")?;
    writeln!(out, "  3) Cloudflare Quick Tunnel (testing only)")?;
    writeln!(out, "  4) Local only (this Mac)")?;
    for _ in 0..3 {
        match read_answer(input, out, &format!("Choose 1-4 [Enter = {n}]: "))?.as_str() {
            "" => return Ok(default),
            "1" => return Ok(Connection::OwnDomain),
            "2" => return Ok(Connection::PasteToken),
            "3" => return Ok(Connection::Quick),
            "4" => return Ok(Connection::LocalOnly),
            _ => writeln!(out, "Please enter 1, 2, 3 or 4.")?,
        }
    }
    anyhow::bail!("no valid connection choice after 3 tries")
}

/// Ask for the hostname; `normalize_hostname` turns a pasted URL into the
/// bare lowercase name and refuses anything that is not a DNS name —
/// including a label starting with `-`, which would otherwise reach
/// `cloudflared`'s argv as a flag (A-T3r b).
fn read_hostname(input: &mut dyn BufRead, out: &mut dyn Write) -> anyhow::Result<String> {
    let raw = read_answer(
        input,
        out,
        "Hostname for the gateway (e.g. brain.example.com): ",
    )?;
    let host = tunnel::normalize_hostname(&raw).map_err(|r| anyhow::anyhow!("hostname {r}"))?;
    let public_url = format!("https://{host}");
    super::validate_public_url(&public_url)
        .map_err(|r| anyhow::anyhow!("public_url {public_url:?} {r}"))?;
    Ok(host)
}

/// A CLI-created tunnel forwards to `--url http://127.0.0.1:<port>`, so it
/// needs a fixed port (the same rule `validate_gateway_config` applies to
/// `tunnel_ingress: local`) — checked before any `cloudflared` call.
fn require_fixed_port(config: &GatewayConfig) -> anyhow::Result<u16> {
    if config.port == 0 {
        return Err(hinted(
            "gateway.yml sets `port: 0` — a tunnel needs a fixed port to forward to",
            "set `port:` in ~/.onebrain/gateway.yml to a fixed port (the default is 7717)",
        ));
    }
    Ok(config.port)
}

fn own_domain(
    config: &GatewayConfig,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    host: &dyn SetupHost,
    done: &mut Vec<String>,
) -> anyhow::Result<Plan> {
    let cf = host.cloudflared();
    cf.path().ok_or_else(missing_cloudflared)?;
    require_fixed_port(config)?;
    let hostname = read_hostname(input, out)?;
    if !cf.has_login() {
        writeln!(
            out,
            "Log in to Cloudflare in the browser window that opens and pick the zone (domain) of {hostname}."
        )?;
        cf.login()?;
        step(out, done, "Cloudflare login".to_string())?;
    }
    let name = cloudflare::tunnel_name_for(&hostname);
    let tunnel = match cf.find_tunnel(&name)? {
        Some(t) => {
            step(out, done, format!("tunnel {name} (reused)"))?;
            t
        }
        None => {
            let t = cf.create_tunnel(&name)?;
            step(out, done, format!("tunnel {name} created"))?;
            t
        }
    };
    route_dns(cf, &tunnel.name, &hostname, input, out)?;
    step(out, done, format!("DNS {hostname} → tunnel {name}"))?;
    let token = cf.token(&tunnel.name)?;
    tunnel::validate_tunnel_token(&token).map_err(|e| {
        anyhow::anyhow!("`cloudflared tunnel token {name}` did not return a usable token: {e}")
    })?;
    Ok(Plan::Store {
        token,
        public_url: format!("https://{hostname}"),
        ingress: Some(TunnelIngress::Local),
    })
}

/// `route dns` without `--overwrite-dns`; an existing record is replaced
/// only on an explicit "yes" in this run.
fn route_dns(
    cf: &dyn Cloudflared,
    tunnel: &str,
    hostname: &str,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let call = |overwrite: bool| {
        cf.route_dns(tunnel, hostname, overwrite).map_err(|e| {
            hinted(
                format!("{e:#}"),
                "re-running `onebrain gateway setup` is safe — if the record was created anyway, the next run finds it already routed",
            )
        })
    };
    let mut outcome = call(false)?;
    if outcome == RouteDns::ConflictingRecord {
        if !ask_yes_no(
            input,
            out,
            &format!("A DNS record for {hostname} already exists. Replace it with this tunnel?"),
            false,
        )? {
            return Err(hinted(
                format!(
                    "a DNS record for {hostname} already exists — left unchanged, nothing written"
                ),
                "pick another hostname, or re-run and answer y to replace that record",
            ));
        }
        outcome = call(true)?;
    }
    match outcome {
        RouteDns::Created | RouteDns::AlreadyRouted => Ok(()),
        RouteDns::ConflictingRecord => Err(hinted(
            format!("cloudflared still reports a conflicting DNS record for {hostname}"),
            "delete that record in the Cloudflare dashboard, then re-run `onebrain gateway setup`",
        )),
        RouteDns::WrongZone { stray: None } => Err(hinted(
            format!("this Cloudflare login can't manage {hostname}'s zone — nothing written"),
            format!(
                "move ~/.cloudflared/cert.pem aside, run `cloudflared tunnel login` and pick {hostname}'s zone, then re-run `onebrain gateway setup`"
            ),
        )),
        RouteDns::WrongZone { stray: Some(fqdn) } => Err(hinted(
            format!(
                "this Cloudflare login can't manage {hostname}'s zone — cloudflared created {fqdn} instead; nothing written"
            ),
            format!(
                "delete the stray DNS record {fqdn} in the Cloudflare dashboard, then move ~/.cloudflared/cert.pem aside, run `cloudflared tunnel login`, pick {hostname}'s zone, and re-run `onebrain gateway setup`"
            ),
        )),
        RouteDns::Failed(msg) => Err(hinted(
            format!("`cloudflared tunnel route dns` failed: {msg} — nothing written"),
            "fix the cause above, then re-run `onebrain gateway setup`",
        )),
    }
}

/// "I already have a tunnel token" (a dashboard tunnel, A-I9): hostname +
/// token, stored with no `tunnel_ingress` (its ingress is remote).
fn paste_token(
    config: &GatewayConfig,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    host: &dyn SetupHost,
) -> anyhow::Result<Plan> {
    host.cloudflared().path().ok_or_else(missing_cloudflared)?;
    let hostname = read_hostname(input, out)?;
    writeln!(
        out,
        "In the Cloudflare dashboard this hostname must route to http://127.0.0.1:{} (Service: HTTP), with no Host header rewriting.",
        config.port
    )?;
    let raw = host.read_secret("Tunnel token:", input, out)?;
    let token = tunnel::extract_token(&raw).to_string();
    tunnel::validate_tunnel_token(&token).map_err(|e| {
        hinted(
            e.to_string(),
            "copy the token again from the tunnel's install command in the dashboard",
        )
    })?;
    Ok(Plan::Store {
        token,
        public_url: format!("https://{hostname}"),
        ingress: None,
    })
}

fn local_only(
    configured: Option<&str>,
    token_path: &Path,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> anyhow::Result<Plan> {
    let Some(url) = configured else {
        return Ok(Plan::Nothing);
    };
    if ask_yes_no(
        input,
        out,
        &format!("Remove the tunnel ({url}) from gateway.yml?"),
        false,
    )? {
        Ok(Plan::RemoveTunnel)
    } else {
        writeln!(
            out,
            "Keeping the tunnel (token at {}).",
            token_path.display()
        )?;
        Ok(Plan::Nothing)
    }
}

/// `public_url` (and `tunnel_ingress`, when set) to null: no tunnel.
fn remove_tunnel(yml: &Path) -> anyhow::Result<()> {
    let config = load_gateway_config_at(yml)?;
    if config.public_url.is_some() {
        super::config_write::set_top_level_key(
            yml,
            "public_url",
            serde_yaml::Value::Null,
            "public_url: null\n",
        )?;
    }
    if config.tunnel_ingress.is_some() {
        super::config_write::set_top_level_key(
            yml,
            "tunnel_ingress",
            serde_yaml::Value::Null,
            "tunnel_ingress: null\n",
        )?;
    }
    Ok(())
}

/// Quick Tunnel (D4): foreground test mode. Writes nothing — the URL and,
/// on a fresh HOME, the vault go to the in-process gateway in memory.
fn quick(
    config: &GatewayConfig,
    vault_override: Option<PathBuf>,
    out: &mut dyn Write,
    host: &dyn SetupHost,
    done: &mut Vec<String>,
) -> anyhow::Result<()> {
    writeln!(
        out,
        "Quick Tunnel — testing only: the URL changes on every start; re-add the connector each time. Nothing is saved."
    )?;
    let cf = host.cloudflared();
    cf.path().ok_or_else(missing_cloudflared)?;
    // The port cloudflared forwards to is the port the gateway binds:
    // the configured one, never 0 (A-T3r c).
    let port = require_fixed_port(config)?;
    if host.port_in_use(port) {
        return Err(hinted(
            format!("the gateway service is running on {port} (127.0.0.1:{port} is in use)"),
            "`onebrain gateway service uninstall` first, or pick another connection",
        ));
    }
    let tunnel = cf.spawn_quick(port, QUICK_URL_TIMEOUT)?;
    // Only a URL `parse_quick_url` accepts in full ever becomes the
    // in-memory public_url — the host guard and the OAuth issuer (A-T3r a).
    let url = match cloudflare::parse_quick_url(&tunnel.url) {
        Some(u) if u == tunnel.url => u,
        _ => anyhow::bail!(
            "cloudflared reported an unexpected Quick Tunnel URL ({:?}) — not a https://<name>.trycloudflare.com address",
            tunnel.url
        ),
    };
    step(out, done, format!("Quick Tunnel {url}"))?;
    writeln!(out)?;
    writeln!(
        out,
        "Connector URL for the Claude app: {}",
        connector_url(&url)
    )?;
    writeln!(
        out,
        "The URL can take up to a minute to start working (new hostname)."
    )?;
    if host.stdout_is_tty() {
        print_qr(out, &connector_url(&url))?;
    }
    writeln!(
        out,
        "Testing only — Ctrl-C stops the gateway and the tunnel; the next start gets a new URL."
    )?;
    out.flush()?;
    let overrides = RunOverrides {
        public_url: Some(url),
        default_vault: vault_override,
    };
    debug_assert!(
        overrides
            .public_url
            .as_deref()
            .is_some_and(|u| cloudflare::parse_quick_url(u).as_deref() == Some(u)),
        "only a parsed Quick Tunnel URL may reach run_with"
    );
    let result = host.run_foreground(port, overrides);
    drop(tunnel); // kills cloudflared
    result
}

/// Poll `<public_url>/.well-known/oauth-authorization-server` until it
/// answers or `limit` passes. Advisory only (A-T0a).
pub(crate) fn poll_reachable(
    http: &dyn HttpProbe,
    public_url: &str,
    limit: Duration,
    step: Duration,
) -> bool {
    let url = format!(
        "{}/.well-known/oauth-authorization-server",
        public_url.trim_end_matches('/')
    );
    let deadline = Instant::now() + limit;
    loop {
        if http.get_json(&url, service::PROBE_TIMEOUT).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(step);
    }
}

pub(crate) fn reachability_line(reachable: bool, public_url: &str, limit: Duration) -> String {
    if reachable {
        format!("✓ {public_url} is reachable")
    } else {
        format!(
            "⚠ {public_url} did not answer within {}s — it may need a little longer; the gateway keeps running",
            limit.as_secs()
        )
    }
}

/// The wizard's service install: the machine's [`service::InstallInputs`]
/// with `print_finish: false` (A-I8b), the exe warning printed first.
pub(crate) fn install_from_wizard(
    paths: &service::ServicePaths,
    mut inputs: service::InstallInputs,
    launcher: &dyn service::Launcher,
    http: &dyn HttpProbe,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    inputs.print_finish = false;
    if let Some(w) = &inputs.exe_warning {
        writeln!(out, "⚠ {w}")?;
    }
    writeln!(
        out,
        "gateway agent program: {}",
        inputs.onebrain_exe.display()
    )?;
    service::install_service(paths, &inputs, launcher, http, out)
}

/// The real machine.
pub(crate) struct SystemSetupHost {
    pub cloudflared: cloudflare::SystemCloudflared,
    pub paths: service::ServicePaths,
    pub mode: crate::output::OutputMode,
    pub stdin_is_tty: bool,
    pub stdout_is_tty: bool,
    pub cwd: PathBuf,
}

impl SetupHost for SystemSetupHost {
    fn cloudflared(&self) -> &dyn Cloudflared {
        &self.cloudflared
    }
    fn token_file_supported(&self, cloudflared: &Path) -> bool {
        tunnel::cloudflared_run_help(cloudflared).is_some_and(|h| tunnel::supports_token_file(&h))
    }
    fn read_secret(
        &self,
        prompt: &str,
        input: &mut dyn BufRead,
        out: &mut dyn Write,
    ) -> anyhow::Result<String> {
        if self.stdin_is_tty {
            return inquire::Password::new(prompt)
                .without_confirmation()
                .prompt()
                .map_err(|e| anyhow::anyhow!("read tunnel token: {e}"));
        }
        read_answer(input, out, &format!("{prompt} "))
    }
    fn service_supported(&self) -> bool {
        service::service_supported()
    }
    fn service_current(&self) -> bool {
        service::system_install_inputs().is_ok_and(|inputs| {
            service::agents_current(&self.paths, &inputs, service::system_launcher().as_ref())
        })
    }
    fn install_service(&self, out: &mut dyn Write) -> anyhow::Result<()> {
        install_from_wizard(
            &self.paths,
            service::system_install_inputs()?,
            service::system_launcher().as_ref(),
            &super::health::UreqProbe,
            out,
        )
    }
    fn port_in_use(&self, port: u16) -> bool {
        std::net::TcpListener::bind(("127.0.0.1", port)).is_err()
    }
    fn run_foreground(&self, port: u16, overrides: RunOverrides) -> anyhow::Result<()> {
        // Advisory reachability check over the public internet. Skipped
        // under `ONEBRAIN_SCHEDULER_NO_ACTIVATE` — the test seam that also
        // turns off `service install`'s health polling — so tests never
        // touch the network.
        let probe = !onebrain_core::scheduler::backend::activation_disabled();
        if let (true, Some(url)) = (probe, overrides.public_url.clone()) {
            std::thread::spawn(move || {
                let ok = poll_reachable(
                    &super::health::UreqProbe,
                    &url,
                    REACHABLE_WAIT,
                    Duration::from_secs(2),
                );
                eprintln!("{}", reachability_line(ok, &url, REACHABLE_WAIT));
            });
        }
        super::run_with(&self.mode, Some(port), overrides)
    }
    fn telegram_setup(&self, input: &mut dyn BufRead, out: &mut dyn Write) -> anyhow::Result<()> {
        super::telegram_setup::run_setup(
            input,
            out,
            &super::telegram::api_base(),
            &self.paths.onebrain_dir,
        )
    }
    fn pairing(&self) -> PairingLookup {
        super::auth::AuthStore::open_at(self.paths.onebrain_dir.join("gateway"))
            .map_or(PairingLookup::Unavailable, |s| service::lookup_pairing(&s))
    }
    fn stdout_is_tty(&self) -> bool {
        self.stdout_is_tty
    }
    fn cwd(&self) -> PathBuf {
        self.cwd.clone()
    }
}

/// `onebrain gateway setup`.
pub fn gateway_setup(mode: &crate::output::OutputMode) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    let home = crate::home::home_dir()?;
    let paths = service::ServicePaths::for_home(&home);
    let onebrain_dir = paths.onebrain_dir.clone();
    let stdin = std::io::stdin();
    let host = SystemSetupHost {
        cloudflared: cloudflare::SystemCloudflared::discover(home),
        paths,
        mode: mode.clone(),
        stdin_is_tty: stdin.is_terminal(),
        stdout_is_tty: std::io::stdout().is_terminal(),
        cwd: std::env::current_dir()?,
    };
    let mut input = stdin.lock();
    run_setup(&mut input, &mut std::io::stdout(), &onebrain_dir, &host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::gateway::cloudflare::fixtures::*;
    use crate::commands::gateway::cloudflare::{QuickTunnel, TunnelRef};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    type Log = Rc<RefCell<Vec<String>>>;

    /// A child that exits on its own right away, on every OS: this test
    /// binary asked to run a test that does not exist.
    fn dummy_child() -> std::process::Child {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "__no_such_test__", "--quiet"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    /// `cloudflared` whose answers are the shared fixtures run through the
    /// real parsers/classifier, so a corrected fixture flows through here.
    struct FakeCf {
        log: Log,
        present: bool,
        logged_in: Cell<bool>,
        login_fails: bool,
        list: &'static str,
        /// `(exit_ok, output)` per `route dns` call, in order; then
        /// `ROUTE_ADDED` for the asked host.
        routes: RefCell<VecDeque<(bool, String)>>,
        route_err: Option<&'static str>,
        quick_url: Option<String>,
    }

    impl FakeCf {
        fn new(log: &Log) -> Self {
            Self {
                log: log.clone(),
                present: true,
                logged_in: Cell::new(false),
                login_fails: false,
                list: LIST_JSON_EMPTY,
                routes: RefCell::new(VecDeque::new()),
                route_err: None,
                quick_url: Some(QUICK_URL.to_string()),
            }
        }
        fn rec(&self, s: String) {
            self.log.borrow_mut().push(s);
        }
    }

    impl Cloudflared for FakeCf {
        fn path(&self) -> Option<PathBuf> {
            self.present
                .then(|| PathBuf::from("/opt/homebrew/bin/cloudflared"))
        }
        fn has_login(&self) -> bool {
            self.rec("has_login".into());
            self.logged_in.get()
        }
        fn login(&self) -> anyhow::Result<()> {
            self.rec("login".into());
            if self.login_fails {
                anyhow::bail!("`cloudflared tunnel login` failed (exit status: 1)");
            }
            self.logged_in.set(true);
            Ok(())
        }
        fn find_tunnel(&self, name: &str) -> anyhow::Result<Option<TunnelRef>> {
            self.rec(format!("find_tunnel {name}"));
            cloudflare::parse_tunnel_list(self.list, name)
        }
        fn create_tunnel(&self, name: &str) -> anyhow::Result<TunnelRef> {
            self.rec(format!("create_tunnel {name}"));
            // The fixture names the brain.example.com tunnel.
            let output = CREATE_OUTPUT.replace(NAME, name);
            Ok(cloudflare::parse_created_tunnel(&output, name).unwrap())
        }
        fn route_dns(&self, tunnel: &str, host: &str, overwrite: bool) -> anyhow::Result<RouteDns> {
            self.rec(format!("route_dns {tunnel} {host} {overwrite}"));
            if let Some(e) = self.route_err {
                anyhow::bail!("{e}");
            }
            let (ok, output) = self
                .routes
                .borrow_mut()
                .pop_front()
                // Default: created, for whichever host was asked.
                .unwrap_or_else(|| (true, ROUTE_ADDED.replace("brain.example.com", host)));
            Ok(cloudflare::classify_route_dns(ok, &output, host))
        }
        fn token(&self, tunnel: &str) -> anyhow::Result<String> {
            self.rec(format!("token {tunnel}"));
            cloudflare::parse_token(TOKEN_STDOUT)
        }
        fn spawn_quick(&self, port: u16, timeout: Duration) -> anyhow::Result<QuickTunnel> {
            self.rec(format!("spawn_quick {port}"));
            match &self.quick_url {
                Some(url) => Ok(QuickTunnel {
                    url: url.clone(),
                    child: dummy_child(),
                }),
                None => {
                    anyhow::bail!(
                        "cloudflared did not report a Quick Tunnel URL within {timeout:?}"
                    )
                }
            }
        }
    }

    struct FakeHost {
        log: Log,
        cf: FakeCf,
        service: bool,
        current: bool,
        port_busy: bool,
        tty: bool,
        pairing_busy: bool,
        cwd: PathBuf,
        install_fails: bool,
    }

    impl FakeHost {
        fn rec(&self, s: String) {
            self.log.borrow_mut().push(s);
        }
    }

    impl SetupHost for FakeHost {
        fn cloudflared(&self) -> &dyn Cloudflared {
            &self.cf
        }
        fn token_file_supported(&self, _c: &Path) -> bool {
            true
        }
        fn read_secret(
            &self,
            prompt: &str,
            input: &mut dyn BufRead,
            out: &mut dyn Write,
        ) -> anyhow::Result<String> {
            read_answer(input, out, prompt)
        }
        fn service_supported(&self) -> bool {
            self.service
        }
        fn service_current(&self) -> bool {
            self.current
        }
        fn install_service(&self, out: &mut dyn Write) -> anyhow::Result<()> {
            self.rec("install_service".into());
            if self.install_fails {
                anyhow::bail!("launchctl bootstrap failed");
            }
            writeln!(out, "(fake install)")?;
            Ok(())
        }
        fn port_in_use(&self, port: u16) -> bool {
            self.rec(format!("port_in_use {port}"));
            self.port_busy
        }
        fn run_foreground(&self, port: u16, o: RunOverrides) -> anyhow::Result<()> {
            self.rec(format!(
                "run_foreground {port} {} {:?}",
                o.public_url.unwrap_or_default(),
                o.default_vault
            ));
            Ok(())
        }
        fn telegram_setup(&self, _i: &mut dyn BufRead, out: &mut dyn Write) -> anyhow::Result<()> {
            self.rec("telegram_setup".into());
            writeln!(out, "(fake telegram)")?;
            Ok(())
        }
        fn pairing(&self) -> PairingLookup {
            if self.pairing_busy {
                PairingLookup::Busy
            } else {
                PairingLookup::Code("ABCD-2345".into())
            }
        }
        fn stdout_is_tty(&self) -> bool {
            self.tty
        }
        fn cwd(&self) -> PathBuf {
            self.cwd.clone()
        }
    }

    /// A temp HOME: `<root>/.onebrain` (not created) and a vault the cwd
    /// points at.
    struct Env {
        _root: tempfile::TempDir,
        dir: PathBuf,
        vault: PathBuf,
        log: Log,
    }

    impl Env {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let vault = root.path().join("vault");
            std::fs::create_dir_all(&vault).unwrap();
            std::fs::write(vault.join("onebrain.yml"), "").unwrap();
            Self {
                dir: root.path().join(".onebrain"),
                vault,
                _root: root,
                log: Rc::default(),
            }
        }
        fn yml(&self) -> PathBuf {
            self.dir.join("gateway.yml")
        }
        fn token(&self) -> PathBuf {
            tunnel::tunnel_token_path(&self.dir)
        }
        fn host(&self) -> FakeHost {
            FakeHost {
                log: self.log.clone(),
                cf: FakeCf::new(&self.log),
                service: true,
                current: false,
                port_busy: false,
                tty: false,
                pairing_busy: false,
                cwd: self.vault.clone(),
                install_fails: false,
            }
        }
        /// gateway.yml with a valid vault, a fixed scratch port, `extra`.
        fn configure(&self, extra: &str) {
            std::fs::create_dir_all(&self.dir).unwrap();
            std::fs::write(
                self.yml(),
                format!(
                    "default_vault: {}\nport: 7905\n{extra}",
                    self.vault.display()
                ),
            )
            .unwrap();
        }
        /// A fully configured own-domain tunnel.
        fn configure_tunnel(&self) {
            self.configure("public_url: 'https://brain.example.com'\ntunnel_ingress: local\n");
            std::fs::create_dir_all(self.token().parent().unwrap()).unwrap();
            std::fs::write(self.token(), TOKEN_STDOUT.trim()).unwrap();
        }
        fn calls(&self) -> Vec<String> {
            self.log.borrow().clone()
        }
        fn config(&self) -> GatewayConfig {
            load_gateway_config_at(&self.yml()).unwrap()
        }
    }

    fn run(stdin: &str, env: &Env, host: &FakeHost) -> (anyhow::Result<()>, String) {
        let mut input = std::io::Cursor::new(stdin.as_bytes().to_vec());
        let mut out = Vec::new();
        let r = run_setup(&mut input, &mut out, &env.dir, host);
        (r, String::from_utf8(out).unwrap())
    }

    fn files(env: &Env) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        (
            std::fs::read(env.yml()).ok(),
            std::fs::read(env.token()).ok(),
        )
    }

    fn hint_of(e: &anyhow::Error) -> String {
        e.downcast_ref::<crate::output::HintedError>()
            .map(|h| format!("{} | {}", h.plain, h.hint))
            .unwrap_or_else(|| format!("{e:#}"))
    }

    const NAME: &str = "onebrain-brain-example-com";

    // ── own domain ──────────────────────────────────────────────────────

    /// Review Focus 1: a hostname typed as a URL is normalised.
    #[test]
    fn fresh_own_domain_creates_tunnel_routes_dns_and_installs() {
        let env = Env::new();
        let h = env.host();
        let (r, out) = run("\n1\nhttps://Brain.Example.com/\nn\n", &env, &h);
        r.unwrap();
        assert_eq!(
            env.calls(),
            [
                "has_login".to_string(),
                "login".into(),
                format!("find_tunnel {NAME}"),
                format!("create_tunnel {NAME}"),
                format!("route_dns {NAME} brain.example.com false"),
                format!("token {NAME}"),
                "install_service".into(),
            ]
        );
        let cfg = env.config();
        assert_eq!(cfg.public_url.as_deref(), Some("https://brain.example.com"));
        assert_eq!(cfg.tunnel_ingress, Some(TunnelIngress::Local));
        assert_eq!(cfg.default_vault.as_deref(), Some(env.vault.as_path()));
        let yml = std::fs::read_to_string(env.yml()).unwrap();
        assert!(
            yml.contains("public_url: 'https://brain.example.com'"),
            "{yml}"
        );
        assert_eq!(
            std::fs::read_to_string(env.token()).unwrap(),
            TOKEN_STDOUT.trim()
        );
        #[cfg(unix)]
        assert_token_is_private(&env.token());
        assert!(out.contains("Connector URL for the Claude app: https://brain.example.com/mcp"));
        assert!(!out.contains(TOKEN_STDOUT.trim()), "token printed: {out}");
    }

    #[cfg(unix)]
    fn assert_token_is_private(p: &Path) {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(p).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn existing_tunnel_is_reused_not_created() {
        let env = Env::new();
        let mut h = env.host();
        h.cf.list = LIST_JSON_MATCH;
        h.cf.logged_in.set(true);
        h.cf.routes = RefCell::new(VecDeque::from([(true, ROUTE_ALREADY.to_string())]));
        let (r, out) = run("\n1\nbrain.example.com\nn\n", &env, &h);
        r.unwrap();
        let calls = env.calls();
        assert!(
            !calls.iter().any(|c| c.starts_with("create_tunnel")),
            "{calls:?}"
        );
        assert!(!calls.contains(&"login".to_string()), "{calls:?}");
        assert!(out.contains(&format!("tunnel {NAME} (reused)")), "{out}");
        assert_eq!(env.config().tunnel_ingress, Some(TunnelIngress::Local));
    }

    /// Review Focus 5 / A-I1: Enter at every prompt on a configured machine
    /// writes nothing, calls no cloudflared, and does not reinstall.
    #[test]
    fn rerun_keep_everything_is_byte_identical() {
        let env = Env::new();
        env.configure_tunnel();
        let before = files(&env);
        let mut h = env.host();
        h.current = true;
        let (r, out) = run("\n\n", &env, &h);
        r.unwrap();
        assert_eq!(files(&env), before);
        assert!(env.calls().is_empty(), "{:?}", env.calls());
        assert!(out.contains("service already running"), "{out}");
    }

    /// A-I1 revised: the plists hold no public_url, so "current" agents
    /// after a config write still need a reinstall (restart).
    #[test]
    fn changed_public_url_with_current_plists_reinstalls() {
        let env = Env::new();
        env.configure_tunnel();
        let mut h = env.host();
        h.current = true;
        h.cf.logged_in.set(true);
        let (r, _out) = run("n\n1\nnew.example.com\n\n", &env, &h);
        r.unwrap();
        assert_eq!(
            env.config().public_url.as_deref(),
            Some("https://new.example.com")
        );
        assert_eq!(
            env.calls().last().map(String::as_str),
            Some("install_service")
        );
    }

    /// Drifted agents are reinstalled even when nothing was written.
    #[test]
    fn rerun_with_drifted_agents_reinstalls() {
        let env = Env::new();
        env.configure_tunnel();
        let h = env.host(); // current = false
        let (r, _) = run("\n\n", &env, &h);
        r.unwrap();
        assert_eq!(env.calls(), ["install_service"]);
    }

    #[test]
    fn dns_conflict_declined_writes_nothing() {
        let env = Env::new();
        let mut h = env.host();
        h.cf.logged_in.set(true);
        h.cf.routes = RefCell::new(VecDeque::from([(false, ROUTE_CONFLICT.to_string())]));
        let (r, out) = run("\n1\nbrain.example.com\n\n", &env, &h);
        let e = r.unwrap_err();
        assert!(hint_of(&e).contains("already exists"), "{}", hint_of(&e));
        assert_eq!(files(&env), (None, None), "nothing written");
        let routes: Vec<_> = env
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("route_dns"))
            .collect();
        assert_eq!(
            routes,
            [format!("route_dns {NAME} brain.example.com false")]
        );
        assert!(out.contains("Done so far:"), "{out}");
        assert!(out.contains(RERUN), "{out}");
    }

    #[test]
    fn dns_conflict_accepted_overwrites() {
        let env = Env::new();
        let mut h = env.host();
        h.cf.logged_in.set(true);
        h.cf.routes = RefCell::new(VecDeque::from([
            (false, ROUTE_CONFLICT.to_string()),
            (true, ROUTE_ADDED.to_string()),
        ]));
        let (r, _) = run("\n1\nbrain.example.com\ny\nn\n", &env, &h);
        r.unwrap();
        let routes: Vec<_> = env
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("route_dns"))
            .collect();
        assert_eq!(
            routes,
            [
                format!("route_dns {NAME} brain.example.com false"),
                format!("route_dns {NAME} brain.example.com true"),
            ]
        );
        assert_eq!(
            env.config().public_url.as_deref(),
            Some("https://brain.example.com")
        );
    }

    /// Review Focus 2 / A-B2 / A-T3a: a login that can't manage the zone —
    /// refused (no record) or zone-appended (stray record) — writes nothing
    /// and says what to do; an existing gateway.yml stays byte-identical.
    #[test]
    fn dns_route_wrong_zone_writes_nothing() {
        for (fixture, ok, stray) in [
            (ROUTE_ZONE_REFUSED, false, false),
            (ROUTE_WRONG_ZONE_APPENDED, true, true),
        ] {
            let env = Env::new();
            env.configure("");
            let before = files(&env);
            let mut h = env.host();
            h.cf.logged_in.set(true);
            h.cf.routes = RefCell::new(VecDeque::from([(ok, fixture.to_string())]));
            let (r, _) = run("1\nbrain.other.com\n", &env, &h);
            let msg = hint_of(&r.unwrap_err());
            assert!(
                msg.contains("this Cloudflare login can't manage brain.other.com's zone"),
                "{msg}"
            );
            assert!(msg.contains("cloudflared tunnel login"), "{msg}");
            assert_eq!(
                msg.contains("delete the stray DNS record brain.other.com.example.com"),
                stray,
                "{msg}"
            );
            assert_eq!(files(&env), before);
            assert!(!env.calls().iter().any(|c| c.starts_with("token")));
        }
    }

    #[test]
    fn dns_route_failed_is_a_hinted_error_and_writes_nothing() {
        let env = Env::new();
        let mut h = env.host();
        h.cf.logged_in.set(true);
        h.cf.routes = RefCell::new(VecDeque::from([(false, "ERR boom\n".to_string())]));
        let (r, _) = run("\n1\nbrain.example.com\n", &env, &h);
        let msg = hint_of(&r.unwrap_err());
        assert!(msg.contains("route dns` failed: ERR boom"), "{msg}");
        assert_eq!(files(&env), (None, None));
    }

    /// A-T3r d: a timed-out `route dns` may have created the record.
    #[test]
    fn dns_route_timeout_says_rerun_is_safe() {
        let env = Env::new();
        let mut h = env.host();
        h.cf.logged_in.set(true);
        h.cf.route_err = Some("`cloudflared tunnel route dns` did not finish within 60s");
        let (r, _) = run("\n1\nbrain.example.com\n", &env, &h);
        let msg = hint_of(&r.unwrap_err());
        assert!(msg.contains("did not finish within 60s"), "{msg}");
        assert!(msg.contains("re-running"), "{msg}");
        assert!(msg.contains("already routed"), "{msg}");
        assert_eq!(files(&env), (None, None));
    }

    #[test]
    fn login_cancelled_writes_nothing() {
        let env = Env::new();
        let mut h = env.host();
        h.cf.login_fails = true;
        let (r, out) = run("\n1\nbrain.example.com\n", &env, &h);
        assert!(format!("{:#}", r.unwrap_err()).contains("login` failed"));
        assert_eq!(files(&env), (None, None));
        assert!(out.contains("✓ vault"), "{out}");
    }

    /// A-T3r b: a hostname that would read as a flag never reaches argv.
    #[test]
    fn hostname_starting_with_a_dash_never_reaches_cloudflared() {
        for bad in ["-x.example.com", "--overwrite-dns", "-.example.com"] {
            let env = Env::new();
            let h = env.host();
            h.cf.logged_in.set(true);
            let (r, _) = run(&format!("\n1\n{bad}\n"), &env, &h);
            assert!(r.unwrap_err().to_string().contains("hostname"), "{bad}");
            assert!(env.calls().is_empty(), "{bad}: {:?}", env.calls());
            assert_eq!(files(&env), (None, None));
        }
    }

    #[test]
    fn own_domain_with_port_zero_refuses_before_cloudflared() {
        let env = Env::new();
        std::fs::create_dir_all(&env.dir).unwrap();
        std::fs::write(
            env.yml(),
            format!("default_vault: {}\nport: 0\n", env.vault.display()),
        )
        .unwrap();
        let h = env.host();
        let (r, _) = run("1\n", &env, &h);
        assert!(hint_of(&r.unwrap_err()).contains("fixed port"));
        assert!(env.calls().is_empty());
    }

    #[test]
    fn missing_cloudflared_errors_before_any_write() {
        for choice in ["1", "2", "3"] {
            let env = Env::new();
            let mut h = env.host();
            h.cf.present = false;
            let (r, _) = run(&format!("\n{choice}\nbrain.example.com\n"), &env, &h);
            let msg = hint_of(&r.unwrap_err());
            assert!(
                msg.contains("cloudflared is not on PATH"),
                "{choice}: {msg}"
            );
            assert!(msg.contains("brew install cloudflared"), "{msg}");
            assert_eq!(files(&env), (None, None), "{choice}");
            assert!(env.calls().is_empty(), "{choice}: {:?}", env.calls());
        }
    }

    // ── paste token ─────────────────────────────────────────────────────

    #[test]
    fn paste_token_stores_a_dashboard_tunnel() {
        let env = Env::new();
        let h = env.host();
        let pasted = format!("sudo cloudflared service install {}", TOKEN_STDOUT.trim());
        let (r, out) = run(&format!("\n2\nbrain.example.com\n{pasted}\nn\n"), &env, &h);
        r.unwrap();
        let cfg = env.config();
        assert_eq!(cfg.public_url.as_deref(), Some("https://brain.example.com"));
        assert_eq!(cfg.tunnel_ingress, None);
        assert_eq!(
            std::fs::read_to_string(env.token()).unwrap(),
            TOKEN_STDOUT.trim()
        );
        assert!(out.contains("http://127.0.0.1:7717"), "{out}");
        assert!(
            !out.contains("Next:"),
            "no dashboard Next lines mid-wizard: {out}"
        );
    }

    /// A-T3b: a dashboard token must not inherit `--url` from an earlier
    /// CLI-created tunnel.
    #[test]
    fn paste_token_after_own_domain_clears_tunnel_ingress() {
        let env = Env::new();
        env.configure_tunnel();
        let h = env.host();
        let (r, _) = run(
            &format!("n\n2\nother.example.com\n{}\n\n", TOKEN_STDOUT.trim()),
            &env,
            &h,
        );
        r.unwrap();
        let cfg = env.config();
        assert_eq!(cfg.tunnel_ingress, None);
        assert_eq!(cfg.tunnel_origin(), None);
        assert_eq!(cfg.public_url.as_deref(), Some("https://other.example.com"));
    }

    #[test]
    fn paste_token_rejects_a_bad_token_and_writes_nothing() {
        let env = Env::new();
        let h = env.host();
        let (r, out) = run("\n2\nbrain.example.com\nnot-a-token-SECRETISH\n", &env, &h);
        let msg = hint_of(&r.unwrap_err());
        assert!(msg.contains("not a tunnel token"), "{msg}");
        assert!(!out.contains("SECRETISH") && !msg.contains("SECRETISH"));
        assert_eq!(files(&env), (None, None));
    }

    // ── Quick Tunnel ────────────────────────────────────────────────────

    /// Review Focus 4.
    #[test]
    fn quick_refuses_when_port_busy() {
        let env = Env::new();
        env.configure("");
        let mut h = env.host();
        h.port_busy = true;
        let (r, _) = run("3\n", &env, &h);
        let msg = hint_of(&r.unwrap_err());
        assert!(
            msg.contains("the gateway service is running on 7905"),
            "{msg}"
        );
        assert!(msg.contains("onebrain gateway service uninstall"), "{msg}");
        assert!(!env.calls().iter().any(|c| c.starts_with("spawn_quick")));
        assert!(!env.calls().iter().any(|c| c.starts_with("run_foreground")));
    }

    /// D4 / A-I3: on a FRESH HOME Quick writes nothing at all — the vault
    /// and the URL reach the in-process gateway in memory. (Port 7717 here
    /// is only a number handed to fakes; nothing binds or probes it.)
    #[test]
    fn quick_never_writes_gateway_yml() {
        let env = Env::new();
        let h = env.host();
        let (r, out) = run("\n3\n", &env, &h);
        r.unwrap();
        assert!(!env.yml().exists(), "gateway.yml written");
        assert!(!env.dir.exists(), "~/.onebrain touched");
        assert_eq!(
            env.calls(),
            [
                "port_in_use 7717".to_string(),
                "spawn_quick 7717".into(),
                format!("run_foreground 7717 {QUICK_URL} {:?}", Some(&env.vault)),
            ]
        );
        assert!(out.contains("testing only"), "{out}");
        assert!(out.contains("up to a minute"), "{out}");
        assert!(out.contains(&format!("{QUICK_URL}/mcp")), "{out}");
        assert!(!out.contains("Pairing code"), "run_with prints it: {out}");
    }

    /// A configured HOME: byte-identical, and the configured vault is not
    /// overridden.
    #[test]
    fn quick_on_a_configured_home_leaves_every_file_alone() {
        let env = Env::new();
        env.configure_tunnel();
        let before = files(&env);
        let h = env.host();
        let (r, _) = run("n\n3\n", &env, &h);
        r.unwrap();
        assert_eq!(files(&env), before);
        assert!(env
            .calls()
            .contains(&format!("run_foreground 7905 {QUICK_URL} None")));
    }

    /// A-T3r c: cloudflared forwards to the port the gateway binds.
    #[test]
    fn quick_uses_the_configured_port_and_refuses_port_zero() {
        let env = Env::new();
        env.configure("");
        let h = env.host();
        run("3\n", &env, &h).0.unwrap();
        let calls = env.calls();
        assert!(calls.contains(&"spawn_quick 7905".to_string()), "{calls:?}");
        assert!(calls[2].starts_with("run_foreground 7905 "), "{calls:?}");

        let env = Env::new();
        std::fs::create_dir_all(&env.dir).unwrap();
        std::fs::write(
            env.yml(),
            format!("default_vault: {}\nport: 0\n", env.vault.display()),
        )
        .unwrap();
        let h = env.host();
        let (r, _) = run("3\n", &env, &h);
        assert!(hint_of(&r.unwrap_err()).contains("fixed port"));
        assert!(env.calls().is_empty(), "{:?}", env.calls());
    }

    /// A-T3r a: only a parsed trycloudflare URL may become public_url.
    #[test]
    fn quick_rejects_a_url_that_is_not_a_quick_tunnel() {
        for bad in [
            "https://evil.example",
            "https://api.trycloudflare.com",
            "https://x.trycloudflare.com.evil.net",
            "https://x.trycloudflare.com/../evil",
        ] {
            let env = Env::new();
            env.configure("");
            let mut h = env.host();
            h.cf.quick_url = Some(bad.to_string());
            let (r, _) = run("3\n", &env, &h);
            assert!(
                r.unwrap_err()
                    .to_string()
                    .contains("unexpected Quick Tunnel URL"),
                "{bad}"
            );
            assert!(
                !env.calls().iter().any(|c| c.starts_with("run_foreground")),
                "{bad}"
            );
        }
    }

    #[test]
    fn quick_url_timeout_is_an_error_with_the_summary() {
        let env = Env::new();
        env.configure("");
        let mut h = env.host();
        h.cf.quick_url = None;
        let (r, out) = run("3\n", &env, &h);
        assert!(r.unwrap_err().to_string().contains("within 30s"));
        assert!(out.contains("Done so far:") && out.contains(RERUN), "{out}");
    }

    // ── local only ──────────────────────────────────────────────────────

    #[test]
    fn fresh_local_only_installs_with_no_tunnel() {
        let env = Env::new();
        let h = env.host();
        let (r, out) = run("\n4\n\n", &env, &h);
        r.unwrap();
        assert_eq!(env.config().public_url, None);
        assert_eq!(env.calls(), ["install_service"]);
        assert!(
            out.contains("Local MCP URL (this Mac only): http://127.0.0.1:7717/mcp"),
            "{out}"
        );
    }

    #[test]
    fn local_only_removes_the_tunnel_only_on_yes() {
        let env = Env::new();
        env.configure_tunnel();
        let before = files(&env);
        let mut h = env.host();
        h.current = true;
        run("n\n4\nn\n\n", &env, &h).0.unwrap();
        assert_eq!(files(&env), before, "no → kept");

        let (r, out) = run("n\n4\ny\n\n", &env, &h);
        r.unwrap();
        let cfg = env.config();
        assert_eq!(cfg.public_url, None);
        assert_eq!(cfg.tunnel_ingress, None);
        assert!(env.token().exists(), "the token file is kept");
        assert!(out.contains("cloudflared tunnel delete"), "{out}");
        assert_eq!(env.calls(), ["install_service"]);
    }

    // ── telegram, platform, finish, EOF ─────────────────────────────────

    #[test]
    fn telegram_is_offered_then_skipped_when_configured() {
        let env = Env::new();
        env.configure("");
        let h = env.host();
        let (r, out) = run("4\ny\n", &env, &h);
        r.unwrap();
        assert_eq!(env.calls(), ["telegram_setup", "install_service"]);
        assert!(out.contains("(fake telegram)"));

        let env = Env::new();
        env.configure("telegram:\n  bot_token: T\n  chat_id: 5\n");
        let h = env.host();
        let (r, out) = run("4\n", &env, &h);
        r.unwrap();
        assert!(out.contains("Telegram already configured"), "{out}");
        assert!(!env.calls().contains(&"telegram_setup".to_string()));
    }

    #[test]
    fn non_macos_prints_manual_commands_instead_of_installing() {
        let env = Env::new();
        let mut h = env.host();
        h.service = false;
        h.cf.logged_in.set(true);
        let (r, out) = run("\n1\nbrain.example.com\nn\n", &env, &h);
        r.unwrap();
        assert!(!env.calls().contains(&"install_service".to_string()));
        assert!(out.contains("  onebrain gateway run"), "{out}");
        assert!(
            out.contains(&format!(
                "--token-file {} --url http://127.0.0.1:7717",
                env.token().display()
            )),
            "{out}"
        );
        assert!(!out.contains(TOKEN_STDOUT.trim()));

        let env = Env::new();
        let mut h = env.host();
        h.service = false;
        let (r, out) = run("\n4\n\n", &env, &h);
        r.unwrap();
        assert!(
            out.contains("Next, keep this running:\n  onebrain gateway run"),
            "{out}"
        );
    }

    #[test]
    fn qr_and_pairing_code_only_on_a_tty() {
        let env = Env::new();
        env.configure_tunnel();
        let mut h = env.host();
        h.current = true;
        let (r, out) = run("\n\n", &env, &h);
        r.unwrap();
        assert!(!out.contains("ABCD-2345"), "{out}");
        assert!(!out.contains('█'), "{out}");
        assert!(
            out.contains("Pairing code: run `onebrain gateway pair`"),
            "{out}"
        );

        h.tty = true;
        let (r, out) = run("\n\n", &env, &h);
        r.unwrap();
        assert!(out.contains("Pairing code: ABCD-2345"), "{out}");
        let qr = super::super::qr::render_qr("https://brain.example.com/mcp").unwrap();
        assert!(out.contains(&qr), "QR of the connector URL: {out}");

        h.pairing_busy = true;
        let (r, out) = run("\n\n", &env, &h);
        r.unwrap();
        assert!(out.contains(service::PAIRING_BUSY_LINE), "{out}");
    }

    #[test]
    fn quick_shows_the_qr_only_on_a_tty() {
        let env = Env::new();
        env.configure("");
        let mut h = env.host();
        let (_, out) = run("3\n", &env, &h);
        assert!(!out.contains('█'));
        h.tty = true;
        let (_, out) = run("3\n", &env, &h);
        let qr = super::super::qr::render_qr(&format!("{QUICK_URL}/mcp")).unwrap();
        assert!(out.contains(&qr), "{out}");
    }

    /// Review Focus 3 / A-I4: EOF before the connection choice is a
    /// cancel — non-zero, the finished steps listed, the re-run line.
    #[test]
    fn eof_mid_wizard_prints_progress_summary() {
        let env = Env::new();
        let h = env.host();
        let (r, out) = run("\n", &env, &h);
        let e = r.unwrap_err();
        assert!(e.downcast_ref::<Cancelled>().is_some(), "{e:#}");
        assert!(out.contains("Done so far:\n  ✓ vault "), "{out}");
        assert!(out.contains(RERUN), "{out}");
        assert_eq!(files(&env), (None, None));

        // EOF at the very first prompt: nothing done.
        let (r, out) = run("", &env, &h);
        assert!(r.is_err());
        assert!(out.contains("Done so far: nothing"), "{out}");
    }

    #[test]
    fn eof_at_a_yes_no_prompt_is_a_cancel_not_the_default() {
        let env = Env::new();
        env.configure_tunnel();
        let before = files(&env);
        let h = env.host();
        // Keep tunnel? -> Enter; Telegram? -> EOF (default would be "no").
        let (r, _) = run("\n", &env, &h);
        assert!(r.unwrap_err().downcast_ref::<Cancelled>().is_some());
        assert_eq!(files(&env), before);
        assert!(env.calls().is_empty(), "no install after a cancel");
    }

    #[test]
    fn bad_answers_re_ask_then_give_up() {
        let env = Env::new();
        env.configure("");
        let h = env.host();
        let (r, out) = run("9\nx\n\n", &env, &h);
        r.unwrap_err();
        assert!(out.contains("Please enter 1, 2, 3 or 4."), "{out}");
        let (r, _) = run("7\n8\n9\n", &env, &h);
        assert!(r.unwrap_err().to_string().contains("after 3 tries"));
        let (r, out) = run("4\nmaybe\nwhat\nhuh\n", &env, &h);
        assert!(r.unwrap_err().to_string().contains("after 3 tries"));
        assert!(out.contains("Please answer y or n."), "{out}");
    }

    #[test]
    fn install_failure_lists_what_was_done() {
        let env = Env::new();
        let mut h = env.host();
        h.install_fails = true;
        let (r, out) = run("\n4\n\n", &env, &h);
        r.unwrap_err();
        assert!(out.contains("  ✓ default_vault set to"), "{out}");
        assert!(out.contains(RERUN), "{out}");
    }

    #[test]
    fn an_invalid_configured_vault_is_chosen_again() {
        let env = Env::new();
        std::fs::create_dir_all(&env.dir).unwrap();
        std::fs::write(env.yml(), "default_vault: /no/such/vault\nport: 7905\n").unwrap();
        let h = env.host();
        let (r, out) = run("\n4\n\n", &env, &h);
        r.unwrap();
        assert!(out.contains("is not a vault — choosing again"), "{out}");
        assert_eq!(
            env.config().default_vault.as_deref(),
            Some(env.vault.as_path())
        );
    }

    // ── helpers ─────────────────────────────────────────────────────────

    struct SeqProbe {
        ok_on: usize,
        calls: Cell<usize>,
        urls: RefCell<Vec<String>>,
    }
    impl HttpProbe for SeqProbe {
        fn get_json(&self, url: &str, _t: Duration) -> Result<serde_json::Value, String> {
            self.calls.set(self.calls.get() + 1);
            self.urls.borrow_mut().push(url.to_string());
            if self.calls.get() >= self.ok_on {
                Ok(serde_json::json!({}))
            } else {
                Err("nope".into())
            }
        }
    }

    #[test]
    fn poll_reachable_retries_until_an_answer_or_the_limit() {
        let p = SeqProbe {
            ok_on: 3,
            calls: Cell::new(0),
            urls: RefCell::default(),
        };
        assert!(poll_reachable(
            &p,
            &format!("{QUICK_URL}/"),
            Duration::from_secs(5),
            Duration::from_millis(1)
        ));
        assert_eq!(p.calls.get(), 3);
        assert_eq!(
            p.urls.borrow()[0],
            format!("{QUICK_URL}/.well-known/oauth-authorization-server")
        );
        let never = SeqProbe {
            ok_on: usize::MAX,
            calls: Cell::new(0),
            urls: RefCell::default(),
        };
        assert!(!poll_reachable(
            &never,
            QUICK_URL,
            Duration::from_millis(20),
            Duration::from_millis(5)
        ));
        assert!(never.calls.get() >= 2);
        assert!(reachability_line(true, QUICK_URL, REACHABLE_WAIT).starts_with('✓'));
        let warn = reachability_line(false, QUICK_URL, REACHABLE_WAIT);
        assert!(
            warn.contains("within 60s") && warn.contains("keeps running"),
            "{warn}"
        );
    }

    struct NoAnswer;
    impl HttpProbe for NoAnswer {
        fn get_json(&self, _u: &str, _t: Duration) -> Result<serde_json::Value, String> {
            Err("connection refused".into())
        }
    }

    /// A-I8b / A-I9b / A-T3d: the wizard's install prints the exe warning
    /// and never the service's own finish block.
    #[test]
    fn install_from_wizard_prints_the_exe_warning_and_no_finish_block() {
        let env = Env::new();
        env.configure("");
        let paths = service::ServicePaths {
            home: env.dir.parent().unwrap().to_path_buf(),
            log_dir: env.dir.parent().unwrap().join("logs"),
            onebrain_dir: env.dir.clone(),
        };
        let inputs = service::InstallInputs {
            onebrain_exe: PathBuf::from("/opt/homebrew/Cellar/onebrain/3.5.2/bin/onebrain"),
            cloudflared: None,
            token_file_supported: true,
            wait_for_up: None,
            show_pairing_code: true,
            print_finish: true,
            exe_warning: Some("versioned Homebrew path".into()),
        };
        let mut out = Vec::new();
        install_from_wizard(&paths, inputs, &service::NoopLauncher, &NoAnswer, &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with("⚠ versioned Homebrew path\n"), "{out}");
        assert!(
            out.contains("gateway agent program: /opt/homebrew/Cellar"),
            "{out}"
        );
        assert!(!out.contains("Pairing code"), "{out}");
        assert!(!out.contains("Logs:"), "{out}");
    }

    #[test]
    fn read_answer_eof_is_cancelled() {
        let mut input = std::io::Cursor::new(Vec::new());
        let mut out = Vec::new();
        let e = read_answer(&mut input, &mut out, "Q? ").unwrap_err();
        assert!(e.downcast_ref::<Cancelled>().is_some());
        assert!(e.to_string().contains("cancelled"));
    }
}
