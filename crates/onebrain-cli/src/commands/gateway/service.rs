//! `onebrain gateway service install|uninstall|status` (v3.5.0 T3, D2).
//!
//! The gateway and cloudflared as KeepAlive LaunchAgents. Every decision
//! lives in [`install_service`]/[`uninstall_service`], which reach launchd
//! only through [`Launcher`] — so they are unit-tested on every OS. Only
//! [`LaunchctlLauncher`] (macOS) runs `launchctl`; with
//! `ONEBRAIN_SCHEDULER_NO_ACTIVATE` set, [`system_launcher`] hands out
//! [`NoopLauncher`] instead (the scheduler's test seam, reused).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;

use super::service_plist::{self, TunnelAuth, GATEWAY_LABEL, TUNNEL_LABEL};

pub(crate) trait Launcher {
    /// Unload `com.onebrain.<label>`; "not loaded" is not an error.
    fn bootout(&self, label: &str);
    fn bootstrap(&self, plist: &Path) -> anyhow::Result<()>;
    /// `launchctl print` output when loaded, `None` otherwise.
    fn print(&self, label: &str) -> Option<String>;
}

/// GET a URL and parse JSON. `Err` carries a short reason, never a panic.
pub(crate) trait HttpProbe {
    fn get_json(&self, url: &str, timeout: Duration) -> Result<serde_json::Value, String>;
}

pub(crate) struct NoopLauncher;
impl Launcher for NoopLauncher {
    fn bootout(&self, _label: &str) {}
    fn bootstrap(&self, _plist: &Path) -> anyhow::Result<()> {
        Ok(())
    }
    fn print(&self, _label: &str) -> Option<String> {
        None
    }
}

#[cfg(target_os = "macos")]
pub(crate) struct LaunchctlLauncher {
    pub uid: u32,
}

#[cfg(target_os = "macos")]
impl Launcher for LaunchctlLauncher {
    fn bootout(&self, label: &str) {
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &format!("gui/{}/com.onebrain.{label}", self.uid)])
            .output();
    }
    fn bootstrap(&self, plist: &Path) -> anyhow::Result<()> {
        let o = std::process::Command::new("launchctl")
            .args(["bootstrap", &format!("gui/{}", self.uid)])
            .arg(plist)
            .output()
            .context("run launchctl")?;
        if o.status.success() {
            return Ok(());
        }
        anyhow::bail!(
            "launchctl bootstrap gui/{} {} failed ({}): {}",
            self.uid,
            plist.display(),
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        )
    }
    fn print(&self, label: &str) -> Option<String> {
        let o = std::process::Command::new("launchctl")
            .args(["print", &format!("gui/{}/com.onebrain.{label}", self.uid)])
            .output()
            .ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    }
}

pub(crate) fn service_supported() -> bool {
    cfg!(target_os = "macos")
}

pub(crate) fn system_launcher() -> Box<dyn Launcher> {
    #[cfg(target_os = "macos")]
    {
        if !onebrain_core::scheduler::backend::activation_disabled() {
            // SAFETY: getuid(2) cannot fail.
            return Box::new(LaunchctlLauncher {
                uid: unsafe { libc::getuid() },
            });
        }
    }
    Box::new(NoopLauncher)
}

pub(crate) struct ServicePaths {
    pub home: PathBuf,
    pub log_dir: PathBuf,
    pub onebrain_dir: PathBuf,
}

impl ServicePaths {
    pub(crate) fn for_home(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            log_dir: home.join("Library/Logs/onebrain"),
            onebrain_dir: home.join(".onebrain"),
        }
    }
    /// `~/Library/LaunchAgents/com.onebrain.<label>.plist` — the scheduler's
    /// own path function, so both features share one namespace rule.
    pub(crate) fn plist(&self, label: &str) -> PathBuf {
        onebrain_core::scheduler::plist_path(label, &self.home)
    }
}

pub(crate) struct InstallInputs {
    pub onebrain_exe: PathBuf,
    pub cloudflared: Option<PathBuf>,
    pub token_file_supported: bool,
    /// `None` = do not poll (activation disabled).
    pub wait_for_up: Option<Duration>,
}

fn hinted(plain: impl Into<String>, hint: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(crate::output::HintedError::new(plain, hint))
}

pub(crate) fn unsupported_os_error() -> anyhow::Error {
    hinted(
        format!(
            "gateway service is not supported yet on {}",
            std::env::consts::OS
        ),
        "keep `onebrain gateway run` and `cloudflared tunnel --no-autoupdate run --token-file \
         ~/.onebrain/gateway/tunnel.token` running under your own supervisor — see \
         docs/gateway.md#use-it-from-your-phone",
    )
}

/// Per-request ceiling for health probes.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) fn local_probe_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/.well-known/oauth-authorization-server")
}

/// The program path for the gateway plist (red-team blocker 2, hub ruling):
/// `current_exe` — the build that ran `service install`, i.e. the build
/// under test — UNLESS `on_path` (`which onebrain`) canonicalises to the
/// same file, in which case the stable PATH entry is used (Homebrew's
/// `/opt/homebrew/bin/onebrain` survives `brew upgrade`; the versioned
/// `Cellar/…` target does not). A chosen path containing `/Cellar/` comes
/// back with a warning. Never canonicalises the RESULT.
pub(crate) fn choose_onebrain_exe(
    current_exe: &Path,
    on_path: Option<&Path>,
) -> (PathBuf, Option<String>) {
    let same_file = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    let chosen = match on_path {
        Some(p) if same_file(p, current_exe) => p.to_path_buf(),
        _ => current_exe.to_path_buf(),
    };
    let warning = chosen.to_string_lossy().contains("/Cellar/").then(|| {
        format!(
            "the gateway agent will run {} — a versioned Homebrew path that `brew upgrade` deletes; \
             put Homebrew's bin (e.g. /opt/homebrew/bin) on PATH and run `onebrain gateway service install` \
             again, or re-run it after every upgrade",
            chosen.display()
        )
    });
    (chosen, warning)
}

/// Create (or keep) `path` 0600 before launchd opens it. The gateway no
/// longer prints its pairing code to a non-TTY stdout (T1), but the log
/// still carries client ids, host paths and approval summaries, and launchd
/// would otherwise create it with the default umask (0644).
fn ensure_private_log(path: &Path) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .with_context(|| format!("create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 600 {}", path.display()))?;
    }
    Ok(())
}

/// Install does not roll back: a failure after something was written or
/// loaded says what stays, how to remove it, and where the log is.
fn leftover_hint(paths: &ServicePaths, stays: &str) -> String {
    format!(
        "{stays}; remove it with `onebrain gateway service uninstall`, and check {}",
        paths.log_dir.join("gateway.log").display()
    )
}

fn bootstrap_with_retry(launcher: &dyn Launcher, plist: &Path) -> anyhow::Result<()> {
    let mut last = None;
    for _ in 0..5 {
        match launcher.bootstrap(plist) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(if cfg!(test) { 1 } else { 400 }));
            }
        }
    }
    Err(last.expect("loop ran at least once"))
}

fn write_agent(
    paths: &ServicePaths,
    spec: &service_plist::AgentSpec,
    launcher: &dyn Launcher,
) -> anyhow::Result<PathBuf> {
    let xml = service_plist::render_keepalive_plist(spec).map_err(anyhow::Error::msg)?;
    ensure_private_log(&spec.log_path)?;
    let plist = paths.plist(spec.label);
    if let Some(dir) = plist.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    // Always 0600: the EnvToken fallback puts a secret in it, and launchd
    // only needs the owner to read it.
    super::config_write::write_private_file(&plist, xml.as_bytes())?;
    launcher.bootout(spec.label);
    bootstrap_with_retry(launcher, &plist).map_err(|e| {
        hinted(
            format!("could not load com.onebrain.{}: {e:#}", spec.label),
            leftover_hint(
                paths,
                &format!(
                    "{} stays in ~/Library/LaunchAgents{}",
                    plist.display(),
                    if spec.label == TUNNEL_LABEL {
                        format!(" and com.onebrain.{GATEWAY_LABEL} stays loaded")
                    } else {
                        String::new()
                    }
                ),
            ),
        )
    })?;
    Ok(plist)
}

fn remove_agent(
    paths: &ServicePaths,
    label: &str,
    launcher: &dyn Launcher,
) -> anyhow::Result<bool> {
    launcher.bootout(label);
    let plist = paths.plist(label);
    match std::fs::remove_file(&plist) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("remove {}", plist.display())),
    }
}

/// Idempotent: (re)writes and (re)loads both agents, which also restarts a
/// running gateway — how a changed `public_url` (T1 host guard) takes effect.
pub(crate) fn install_service(
    paths: &ServicePaths,
    inputs: &InstallInputs,
    launcher: &dyn Launcher,
    http: &dyn HttpProbe,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let gateway_yml = paths.onebrain_dir.join("gateway.yml");
    let config = super::config::load_gateway_config_at(&gateway_yml)?;
    super::validate_gateway_config(&config)?;
    if config.default_vault.is_none() && config.vaults.is_empty() {
        return Err(hinted(
            "gateway.yml names no vault — as a service the gateway starts in `/`, so no tool call could find one",
            format!("add `default_vault: /path/to/your/vault` to {}", gateway_yml.display()),
        ));
    }

    let token_path = super::tunnel::tunnel_token_path(&paths.onebrain_dir);
    let tunnel = config.public_url.is_some() && token_path.exists();
    if tunnel && inputs.cloudflared.is_none() {
        return Err(hinted(
            "a tunnel is configured but cloudflared is not on PATH",
            "brew install cloudflared, then run this again",
        ));
    }
    if launcher.print(GATEWAY_LABEL).is_none()
        && http
            .get_json(&local_probe_url(config.port), PROBE_TIMEOUT)
            .is_ok()
    {
        return Err(hinted(
            format!(
                "something is already answering on 127.0.0.1:{}",
                config.port
            ),
            "stop the foreground `onebrain gateway run` (Ctrl-C), then run this again",
        ));
    }

    let gateway = service_plist::gateway_agent(&inputs.onebrain_exe, &paths.log_dir);
    let gateway_plist = write_agent(paths, &gateway, launcher)?;
    writeln!(
        out,
        "✓ com.onebrain.{GATEWAY_LABEL} loaded ({})",
        gateway_plist.display()
    )?;

    if let (true, Some(cloudflared)) = (tunnel, inputs.cloudflared.as_deref()) {
        let mut tunnel_step = || -> anyhow::Result<()> {
            let auth = if inputs.token_file_supported {
                TunnelAuth::TokenFile(token_path.clone())
            } else {
                let token = std::fs::read_to_string(&token_path)
                    .with_context(|| format!("read {}", token_path.display()))?;
                TunnelAuth::EnvToken(token.trim().to_string())
            };
            let spec = service_plist::tunnel_agent(cloudflared, &auth, &paths.log_dir);
            let plist = write_agent(paths, &spec, launcher)?;
            writeln!(
                out,
                "✓ com.onebrain.{TUNNEL_LABEL} loaded ({})",
                plist.display()
            )?;
            // Hub ruling: record which token path the probe chose.
            if inputs.token_file_supported {
                writeln!(
                    out,
                    "  tunnel token via --token-file {}",
                    token_path.display()
                )?;
            } else {
                writeln!(
                    out,
                    "  tunnel token via TUNNEL_TOKEN in the 0600 plist (this cloudflared has no --token-file; `brew upgrade cloudflared` to switch)"
                )?;
            }
            Ok(())
        };
        tunnel_step().map_err(|e| {
            if e.downcast_ref::<crate::output::HintedError>().is_some() {
                return e;
            }
            hinted(
                format!("could not set up the tunnel agent: {e:#}"),
                leftover_hint(paths, &format!("com.onebrain.{GATEWAY_LABEL} stays loaded")),
            )
        })?;
    } else {
        if remove_agent(paths, TUNNEL_LABEL, launcher)? {
            writeln!(
                out,
                "• removed com.onebrain.{TUNNEL_LABEL} (no tunnel configured)"
            )?;
        }
        if config.public_url.is_some() {
            writeln!(out, "• public_url is set but there is no tunnel token — run `onebrain gateway tunnel setup`")?;
        } else if token_path.exists() {
            writeln!(out, "• a tunnel token exists but gateway.yml has no public_url — run `onebrain gateway tunnel setup`")?;
        }
    }

    if let Some(limit) = inputs.wait_for_up {
        let url = local_probe_url(config.port);
        let deadline = Instant::now() + limit;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if http
                .get_json(
                    &url,
                    remaining.clamp(Duration::from_millis(50), PROBE_TIMEOUT),
                )
                .is_ok()
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err(hinted(
                    format!(
                        "the gateway did not answer on 127.0.0.1:{} within {}s",
                        config.port,
                        limit.as_millis().div_ceil(1000)
                    ),
                    leftover_hint(
                        paths,
                        &format!(
                            "the loaded com.onebrain agent(s) keep restarting in the background"
                        ),
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(if cfg!(test) { 5 } else { 250 }));
        }
        writeln!(out, "✓ gateway answering on 127.0.0.1:{}", config.port)?;
    }
    if let Some(url) = config.public_url.as_deref() {
        writeln!(
            out,
            "Connector URL for the Claude app: {}/mcp",
            url.trim_end_matches('/')
        )?;
    }
    writeln!(out, "Pairing code: run `onebrain gateway pair`")?;
    writeln!(out, "Logs: {}", paths.log_dir.join("gateway.log").display())?;
    Ok(())
}

/// Never touches the token, `gateway.yml`, or the logs.
pub(crate) fn uninstall_service(
    paths: &ServicePaths,
    launcher: &dyn Launcher,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    for label in [TUNNEL_LABEL, GATEWAY_LABEL] {
        if remove_agent(paths, label, launcher)? {
            writeln!(out, "✓ removed com.onebrain.{label}")?;
        } else {
            writeln!(out, "• com.onebrain.{label} was not installed")?;
        }
    }
    Ok(())
}

fn home() -> anyhow::Result<PathBuf> {
    crate::home::home_dir().context("resolve home directory")
}

/// `onebrain gateway service install`.
pub fn service_install(_mode: &crate::output::OutputMode) -> anyhow::Result<()> {
    if !service_supported() {
        return Err(unsupported_os_error());
    }
    let paths = ServicePaths::for_home(&home()?);
    let cloudflared = which::which("cloudflared").ok();
    let token_file_supported = cloudflared
        .as_deref()
        .and_then(super::tunnel::cloudflared_run_help)
        .is_some_and(|help| super::tunnel::supports_token_file(&help));
    let activate = !onebrain_core::scheduler::backend::activation_disabled();
    let current = std::env::current_exe().context("resolve the onebrain binary path")?;
    let (onebrain_exe, exe_warning) =
        choose_onebrain_exe(&current, which::which("onebrain").ok().as_deref());
    if let Some(w) = exe_warning {
        eprintln!("⚠ {w}");
    }
    println!("gateway agent program: {}", onebrain_exe.display());
    let inputs = InstallInputs {
        onebrain_exe,
        cloudflared,
        token_file_supported,
        wait_for_up: activate.then_some(Duration::from_secs(10)),
    };
    install_service(
        &paths,
        &inputs,
        system_launcher().as_ref(),
        &super::health::UreqProbe,
        &mut std::io::stdout(),
    )
}

/// `onebrain gateway service uninstall`.
pub fn service_uninstall(_mode: &crate::output::OutputMode) -> anyhow::Result<()> {
    if !service_supported() {
        return Err(unsupported_os_error());
    }
    uninstall_service(
        &ServicePaths::for_home(&home()?),
        system_launcher().as_ref(),
        &mut std::io::stdout(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeLauncher {
        calls: RefCell<Vec<String>>,
        loaded: RefCell<Vec<String>>,
        bootstrap_failures_left: RefCell<u32>,
    }
    impl Launcher for FakeLauncher {
        fn bootout(&self, label: &str) {
            self.calls.borrow_mut().push(format!("bootout {label}"));
            self.loaded.borrow_mut().retain(|l| l != label);
        }
        fn bootstrap(&self, plist: &Path) -> anyhow::Result<()> {
            let name = plist.file_name().unwrap().to_string_lossy().into_owned();
            self.calls.borrow_mut().push(format!("bootstrap {name}"));
            if *self.bootstrap_failures_left.borrow() > 0 {
                *self.bootstrap_failures_left.borrow_mut() -= 1;
                anyhow::bail!("Bootstrap failed: 5: Input/output error");
            }
            let label = name
                .trim_start_matches("com.onebrain.")
                .trim_end_matches(".plist");
            self.loaded.borrow_mut().push(label.to_string());
            Ok(())
        }
        fn print(&self, label: &str) -> Option<String> {
            self.loaded
                .borrow()
                .iter()
                .any(|l| l == label)
                .then(|| "state = running\npid = 42\n".into())
        }
    }

    struct FakeHttp {
        up: bool,
    }
    impl HttpProbe for FakeHttp {
        fn get_json(&self, _url: &str, _t: Duration) -> Result<serde_json::Value, String> {
            if self.up {
                Ok(serde_json::json!({"issuer": "http://127.0.0.1:7717"}))
            } else {
                Err("connection refused".into())
            }
        }
    }

    const FAKE_TOKEN: &str = "eyJhIjoiMDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWYiLCJ0IjoiNmZmNDJhZTItNzY1ZC00YWRmLTgxMTItMzFjNTVjMTU1MWVmIiwicyI6ImJtOTBMV0V0Y21WaGJDMXpaV055WlhRPSJ9";

    fn home_with(gateway_yml: &str, token: bool) -> (tempfile::TempDir, ServicePaths) {
        let root = tempfile::tempdir().unwrap();
        let paths = ServicePaths::for_home(root.path());
        std::fs::create_dir_all(paths.onebrain_dir.join("gateway")).unwrap();
        std::fs::write(paths.onebrain_dir.join("gateway.yml"), gateway_yml).unwrap();
        if token {
            std::fs::write(
                crate::commands::gateway::tunnel::tunnel_token_path(&paths.onebrain_dir),
                FAKE_TOKEN,
            )
            .unwrap();
        }
        (root, paths)
    }

    fn inputs(cloudflared: bool, token_file: bool) -> InstallInputs {
        InstallInputs {
            onebrain_exe: PathBuf::from("/opt/homebrew/bin/onebrain"),
            cloudflared: cloudflared.then(|| PathBuf::from("/opt/homebrew/bin/cloudflared")),
            token_file_supported: token_file,
            wait_for_up: Some(Duration::from_millis(200)),
        }
    }

    const WITH_VAULT: &str = "default_vault: /v\npublic_url: 'https://brain.example.com'\n";

    #[test]
    fn install_writes_both_agents_0600_bootstraps_them_and_creates_0600_logs() {
        let (_r, paths) = home_with(WITH_VAULT, true);
        let l = FakeLauncher::default();
        let mut out = Vec::new();
        // Down before install (port-clash check passes), up once the gateway agent loads.
        install_service(
            &paths,
            &inputs(true, true),
            &l,
            &UpAfterBootstrap(&l),
            &mut out,
        )
        .unwrap();
        let calls = l.calls.borrow().clone();
        assert_eq!(
            calls,
            [
                "bootout gateway",
                "bootstrap com.onebrain.gateway.plist",
                "bootout gateway-tunnel",
                "bootstrap com.onebrain.gateway-tunnel.plist",
            ]
        );
        let gw = std::fs::read_to_string(paths.plist("gateway")).unwrap();
        assert!(gw.contains("<string>/opt/homebrew/bin/onebrain</string>"));
        let tn = std::fs::read_to_string(paths.plist("gateway-tunnel")).unwrap();
        assert!(tn.contains("<string>--token-file</string>"));
        assert!(!tn.contains(FAKE_TOKEN));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&paths.plist("gateway")), 0o600);
            assert_eq!(mode(&paths.plist("gateway-tunnel")), 0o600);
            assert_eq!(mode(&paths.log_dir.join("gateway.log")), 0o600);
            assert_eq!(mode(&paths.log_dir.join("gateway-tunnel.log")), 0o600);
        }
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("https://brain.example.com/mcp"), "{text}");
        assert!(text.contains("onebrain gateway pair"), "{text}");
        // Hub ruling: say which token path was used.
        assert!(text.contains("tunnel token via --token-file"), "{text}");
    }

    /// `HttpProbe` that reports "down" until the fake gateway agent is
    /// loaded — models launchd starting the process.
    struct UpAfterBootstrap<'a>(&'a FakeLauncher);
    impl HttpProbe for UpAfterBootstrap<'_> {
        fn get_json(&self, url: &str, t: Duration) -> Result<serde_json::Value, String> {
            FakeHttp {
                up: self.0.print("gateway").is_some(),
            }
            .get_json(url, t)
        }
    }

    #[test]
    fn install_without_token_file_support_puts_the_token_in_the_0600_plist_env() {
        let (_r, paths) = home_with(WITH_VAULT, true);
        let l = FakeLauncher::default();
        let mut out = Vec::new();
        install_service(
            &paths,
            &inputs(true, false),
            &l,
            &UpAfterBootstrap(&l),
            &mut out,
        )
        .unwrap();
        let tn = std::fs::read_to_string(paths.plist("gateway-tunnel")).unwrap();
        assert!(tn.contains("<key>TUNNEL_TOKEN</key>") && tn.contains(FAKE_TOKEN));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("tunnel token via TUNNEL_TOKEN"), "{text}");
        assert!(
            !text.contains(FAKE_TOKEN),
            "token leaked into install output"
        );
    }

    /// Red-team blocker 2: the plist runs the build that ran `service
    /// install` — its stable PATH alias only when that is the SAME file.
    #[cfg(unix)]
    #[test]
    fn choose_onebrain_exe_prefers_current_exe_unless_path_resolves_to_it() {
        let root = tempfile::tempdir().unwrap();
        let cellar = root.path().join("Cellar/onebrain/3.5.0/bin");
        std::fs::create_dir_all(&cellar).unwrap();
        let real = cellar.join("onebrain");
        std::fs::write(&real, "bin").unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let link = bin.join("onebrain");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let other = root.path().join("dev/onebrain");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, "dev build").unwrap();

        // PATH alias resolves to the running binary → stable alias, no warning.
        let (exe, warn) = choose_onebrain_exe(&real, Some(&link));
        assert_eq!(exe, link);
        assert!(warn.is_none(), "{warn:?}");
        // PATH points at a DIFFERENT binary (a dev build ran install) →
        // the build under test wins.
        let (exe, _) = choose_onebrain_exe(&other, Some(&link));
        assert_eq!(exe, other);
        // Nothing on PATH → current_exe.
        let (exe, _) = choose_onebrain_exe(&other, None);
        assert_eq!(exe, other);
        // A Cellar path with no stable alias → kept, but warned about.
        let (exe, warn) = choose_onebrain_exe(&real, None);
        assert_eq!(exe, real);
        let warn = warn.expect("a Cellar program path must warn");
        assert!(
            warn.contains("/Cellar/") && warn.contains("brew upgrade"),
            "{warn}"
        );
    }

    #[test]
    fn install_without_a_tunnel_installs_the_gateway_only_and_removes_a_stale_tunnel_agent() {
        let (_r, paths) = home_with("default_vault: /v\n", false);
        std::fs::create_dir_all(paths.plist("x").parent().unwrap()).unwrap();
        std::fs::write(paths.plist("gateway-tunnel"), "stale").unwrap();
        let l = FakeLauncher::default();
        install_service(
            &paths,
            &inputs(false, false),
            &l,
            &UpAfterBootstrap(&l),
            &mut Vec::new(),
        )
        .unwrap();
        assert!(!paths.plist("gateway-tunnel").exists());
        assert!(l
            .calls
            .borrow()
            .contains(&"bootout gateway-tunnel".to_string()));
    }

    #[test]
    fn install_with_a_tunnel_but_no_cloudflared_refuses_with_a_brew_hint() {
        let (_r, paths) = home_with(WITH_VAULT, true);
        let l = FakeLauncher::default();
        let err = install_service(
            &paths,
            &inputs(false, false),
            &l,
            &FakeHttp { up: false },
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(err
            .downcast_ref::<crate::output::HintedError>()
            .unwrap()
            .hint
            .contains("brew install cloudflared"));
        assert!(l.calls.borrow().is_empty());
    }

    /// Review Focus 2.
    #[test]
    fn install_refuses_a_zero_config_gateway_because_launchd_runs_it_from_slash() {
        let (_r, paths) = home_with("port: 7717\n", false);
        let l = FakeLauncher::default();
        let err = install_service(
            &paths,
            &inputs(false, false),
            &l,
            &FakeHttp { up: false },
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(err
            .downcast_ref::<crate::output::HintedError>()
            .unwrap()
            .hint
            .contains("default_vault"));
        assert!(!paths.plist("gateway").exists());
    }

    /// Review Focus 3.
    #[test]
    fn install_refuses_while_a_foreground_gateway_holds_the_port() {
        let (_r, paths) = home_with("default_vault: /v\n", false);
        let l = FakeLauncher::default();
        let err = install_service(
            &paths,
            &inputs(false, false),
            &l,
            &FakeHttp { up: true },
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("already answering"), "{err:#}");
        assert!(l.calls.borrow().is_empty());
    }

    #[test]
    fn install_rejects_an_invalid_public_url_like_gateway_run_does() {
        let (_r, paths) = home_with(
            "default_vault: /v\npublic_url: 'http://brain.example.com'\n",
            false,
        );
        let err = install_service(
            &paths,
            &inputs(false, false),
            &FakeLauncher::default(),
            &FakeHttp { up: false },
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("public_url"), "{err:#}");
    }

    /// `bootout` of a running KeepAlive job can race the next `bootstrap`.
    #[test]
    fn install_retries_a_transient_bootstrap_failure() {
        let (_r, paths) = home_with("default_vault: /v\n", false);
        let l = FakeLauncher::default();
        *l.bootstrap_failures_left.borrow_mut() = 2;
        install_service(
            &paths,
            &inputs(false, false),
            &l,
            &UpAfterBootstrap(&l),
            &mut Vec::new(),
        )
        .unwrap();
    }

    #[test]
    fn install_reports_a_gateway_that_never_comes_up_and_points_at_the_log() {
        let (_r, paths) = home_with("default_vault: /v\n", false);
        let err = install_service(
            &paths,
            &inputs(false, false),
            &FakeLauncher::default(),
            &FakeHttp { up: false },
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(err
            .downcast_ref::<crate::output::HintedError>()
            .unwrap()
            .hint
            .contains("gateway.log"));
    }

    #[test]
    fn uninstall_boots_out_and_removes_both_plists_but_never_the_token_or_config() {
        let (_r, paths) = home_with(WITH_VAULT, true);
        let l = FakeLauncher::default();
        install_service(
            &paths,
            &inputs(true, true),
            &l,
            &UpAfterBootstrap(&l),
            &mut Vec::new(),
        )
        .unwrap();
        uninstall_service(&paths, &l, &mut Vec::new()).unwrap();
        assert!(!paths.plist("gateway").exists() && !paths.plist("gateway-tunnel").exists());
        assert!(crate::commands::gateway::tunnel::tunnel_token_path(&paths.onebrain_dir).exists());
        assert!(paths.onebrain_dir.join("gateway.yml").exists());
        assert!(l.loaded.borrow().is_empty());
    }

    #[test]
    fn unsupported_os_error_names_the_os_and_the_manual_way() {
        let err = unsupported_os_error();
        let h = err.downcast_ref::<crate::output::HintedError>().unwrap();
        assert!(h.plain.contains("not supported yet on"), "{}", h.plain);
        assert!(
            h.hint.contains("onebrain gateway run") && h.hint.contains("cloudflared tunnel"),
            "{}",
            h.hint
        );
    }

    /// Fix round 1 (Ruling 9): a failure after something was written or
    /// loaded must tell the user how to remove what stays.
    #[test]
    fn install_bootstrap_failure_hint_names_uninstall_and_the_log() {
        let (_r, paths) = home_with("default_vault: /v\n", false);
        let l = FakeLauncher::default();
        *l.bootstrap_failures_left.borrow_mut() = 99;
        let err = install_service(
            &paths,
            &inputs(false, false),
            &l,
            &FakeHttp { up: false },
            &mut Vec::new(),
        )
        .unwrap_err();
        let h = err.downcast_ref::<crate::output::HintedError>().unwrap();
        assert!(
            h.hint.contains("`onebrain gateway service uninstall`")
                && h.hint.contains("gateway.log"),
            "{}",
            h.hint
        );
        assert!(
            h.hint.contains("stays in ~/Library/LaunchAgents"),
            "{}",
            h.hint
        );
        assert!(paths.plist("gateway").exists());
    }

    #[test]
    fn install_tunnel_step_failure_hint_says_the_gateway_stays_loaded() {
        let (_r, paths) = home_with(WITH_VAULT, true);
        let l = FakeLauncher::default();
        // Gateway bootstrap succeeds; the tunnel's fails every retry.
        struct FailTunnel<'a>(&'a FakeLauncher);
        impl Launcher for FailTunnel<'_> {
            fn bootout(&self, label: &str) {
                self.0.bootout(label)
            }
            fn bootstrap(&self, plist: &Path) -> anyhow::Result<()> {
                if plist.to_string_lossy().contains("gateway-tunnel") {
                    anyhow::bail!("Bootstrap failed: 5");
                }
                self.0.bootstrap(plist)
            }
            fn print(&self, label: &str) -> Option<String> {
                self.0.print(label)
            }
        }
        let err = install_service(
            &paths,
            &inputs(true, true),
            &FailTunnel(&l),
            &UpAfterBootstrap(&l),
            &mut Vec::new(),
        )
        .unwrap_err();
        let h = err.downcast_ref::<crate::output::HintedError>().unwrap();
        assert!(
            h.hint.contains("`onebrain gateway service uninstall`"),
            "{}",
            h.hint
        );
        assert!(
            h.hint.contains("com.onebrain.gateway stays loaded"),
            "{}",
            h.hint
        );
    }

    #[test]
    fn install_tunnel_token_read_failure_hint_names_uninstall() {
        let (_r, paths) = home_with(WITH_VAULT, true);
        let l = FakeLauncher::default();
        // EnvToken mode reads the token file; make it a directory so the read fails.
        let tp = crate::commands::gateway::tunnel::tunnel_token_path(&paths.onebrain_dir);
        std::fs::remove_file(&tp).unwrap();
        std::fs::create_dir(&tp).unwrap();
        let err = install_service(
            &paths,
            &inputs(true, false),
            &l,
            &UpAfterBootstrap(&l),
            &mut Vec::new(),
        )
        .unwrap_err();
        let h = err.downcast_ref::<crate::output::HintedError>().unwrap();
        assert!(
            h.hint.contains("`onebrain gateway service uninstall`")
                && h.hint.contains("stays loaded"),
            "{}",
            h.hint
        );
    }

    #[test]
    fn install_poll_timeout_hint_names_uninstall_and_never_reads_zero_seconds() {
        let (_r, paths) = home_with("default_vault: /v\n", false);
        let err = install_service(
            &paths,
            &inputs(false, false),
            &FakeLauncher::default(),
            &FakeHttp { up: false },
            &mut Vec::new(),
        )
        .unwrap_err();
        let h = err.downcast_ref::<crate::output::HintedError>().unwrap();
        assert!(
            h.hint.contains("`onebrain gateway service uninstall`")
                && h.hint.contains("gateway.log"),
            "{}",
            h.hint
        );
        assert!(h.plain.contains("within 1s"), "{}", h.plain);
    }

    #[test]
    fn install_poll_timeouts_are_clamped_to_the_remaining_budget() {
        struct Rec(RefCell<Vec<Duration>>);
        impl HttpProbe for Rec {
            fn get_json(&self, _u: &str, t: Duration) -> Result<serde_json::Value, String> {
                self.0.borrow_mut().push(t);
                Err("down".into())
            }
        }
        let (_r, paths) = home_with("default_vault: /v\n", false);
        let rec = Rec(RefCell::new(Vec::new()));
        let _ = install_service(
            &paths,
            &inputs(false, false),
            &FakeLauncher::default(),
            &rec,
            &mut Vec::new(),
        );
        let seen = rec.0.borrow();
        // [0] is the port-clash probe (full ceiling); every poll after it is within the 200ms budget.
        assert!(
            seen.len() > 1 && seen[1..].iter().all(|t| *t <= Duration::from_millis(200)),
            "{seen:?}"
        );
    }

    #[test]
    fn install_hints_when_a_tunnel_token_exists_but_public_url_is_unset() {
        let (_r, paths) = home_with("default_vault: /v\n", true);
        let l = FakeLauncher::default();
        let mut out = Vec::new();
        install_service(
            &paths,
            &inputs(true, true),
            &l,
            &UpAfterBootstrap(&l),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("no public_url"), "{text}");
    }
}
