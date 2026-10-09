//! `onebrain gateway tokens list|revoke` and `onebrain gateway clients
//! list|remove` (#406): operator UX over [`AuthStore`].
//!
//! Shape: each verb is an in-process `*_env` builder (takes an `&AuthStore`,
//! returns an `Envelope`, unit-tested against a temp store) + a pure text
//! renderer + a 3-line public wrapper that opens the real
//! `~/.onebrain/gateway/` store and `emit`s. Token values never reach this
//! module. The store only hands out secret-free `TokenView`s (see
//! `store.rs`), so nothing here can print a credential.
//!
//! Revocation needs no IPC with a running gateway: the server re-reads
//! `tokens.json` on every request, and every store write holds
//! `auth.lock`, so a revoke here is seen on the server's next request.

use anyhow::{Context, Result};
use serde::Serialize;

use super::auth::store::{
    normalize_id_prefix, AppType, ClientView, RemovedClient, RevokeOutcome, TokenKind,
    TokenSelector, TokenStatus, TokenView,
};
use super::auth::AuthStore;
use crate::cli::GatewayTokensRevokeArgs;
use crate::output::{emit, Envelope, HintedError, OutputMode};

// ── Payloads ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct TokensListData {
    pub all: bool,
    pub tokens: Vec<TokenView>,
    /// Records on disk NOT shown because they are expired/revoked (0 with `--all`).
    pub hidden: usize,
}

#[derive(Debug, Serialize)]
pub struct TokensRevokeData {
    /// `"id" | "client" | "family"`.
    pub selector: &'static str,
    /// The normalized id prefix, or the exact client id.
    pub target: String,
    /// Display ids revoked by this call.
    pub revoked: Vec<String>,
    /// Matching tokens that were already revoked.
    pub already_revoked: usize,
}

#[derive(Debug, Serialize)]
pub struct ClientsListData {
    pub clients: Vec<ClientView>,
}

#[derive(Debug, Serialize)]
pub struct ClientsRemoveData {
    pub client_id: String,
    pub tokens_revoked: usize,
    pub codes_removed: usize,
}

// ── Public verb handlers (dispatch.rs) ───────────────────────────────────

pub fn tokens_list(mode: &OutputMode, all: bool) -> Result<()> {
    let env = tokens_list_env(&open_store()?, all)?;
    emit(&env, mode, std::io::stdout().lock(), render_tokens_list)
}

pub fn tokens_revoke(mode: &OutputMode, args: &GatewayTokensRevokeArgs) -> Result<()> {
    let env = tokens_revoke_env(&open_store()?, args)?;
    emit(&env, mode, std::io::stdout().lock(), render_tokens_revoke)
}

pub fn clients_list(mode: &OutputMode) -> Result<()> {
    let env = clients_list_env(&open_store()?)?;
    emit(&env, mode, std::io::stdout().lock(), render_clients_list)
}

pub fn clients_remove(mode: &OutputMode, client_id: &str) -> Result<()> {
    let env = clients_remove_env(&open_store()?, client_id)?;
    emit(&env, mode, std::io::stdout().lock(), render_clients_remove)
}

fn open_store() -> Result<AuthStore> {
    AuthStore::open().context(HintedError::new(
        "couldn't open the gateway auth store (~/.onebrain/gateway)",
        "check that your home directory is writable, then retry",
    ))
}

fn store_unreadable() -> HintedError {
    HintedError::new(
        "couldn't read the gateway auth store — a file under ~/.onebrain/gateway is unreadable or corrupt",
        "inspect `~/.onebrain/gateway/tokens.json` and `clients.json` (deleting a corrupt file signs every client out)",
    )
}

fn store_unwritable() -> HintedError {
    HintedError::new(
        "couldn't update the gateway auth store — nothing was changed",
        "check permissions on `~/.onebrain/gateway`, then retry",
    )
}

// ── Builders (in-process, unit-tested) ───────────────────────────────────

pub(crate) fn tokens_list_env(store: &AuthStore, all: bool) -> Result<Envelope<TokensListData>> {
    let every = store.list_tokens().context(store_unreadable())?;
    let total = every.len();
    let tokens: Vec<TokenView> = if all {
        every
    } else {
        every
            .into_iter()
            .filter(|t| t.status == TokenStatus::Live)
            .collect()
    };
    let hidden = total - tokens.len();
    Ok(Envelope::ok(
        "gateway.tokens.list",
        None,
        TokensListData {
            all,
            tokens,
            hidden,
        },
    ))
}

pub(crate) fn tokens_revoke_env(
    store: &AuthStore,
    args: &GatewayTokensRevokeArgs,
) -> Result<Envelope<TokensRevokeData>> {
    let selector = selector_from_args(args)?;
    let outcome = store.revoke_tokens(&selector).context(store_unwritable())?;
    revoke_outcome_env(&selector, outcome)
}

/// Map a clap-validated arg set to a [`TokenSelector`]. Id/family values are
/// normalized. A value that isn't a 4–12 hex id (e.g. a pasted raw token) is
/// rejected WITHOUT echoing it.
fn selector_from_args(args: &GatewayTokensRevokeArgs) -> Result<TokenSelector> {
    if let Some(raw) = &args.id {
        return Ok(TokenSelector::Id(normalized(raw, "token")?));
    }
    if let Some(client) = &args.client {
        return Ok(TokenSelector::Client(client.clone()));
    }
    if let Some(raw) = &args.family {
        return Ok(TokenSelector::Family(normalized(raw, "family")?));
    }
    // Unreachable from the binary: cli.rs's `selector` ArgGroup is required.
    Err(anyhow::Error::new(HintedError::new(
        "nothing revoked — no token selector given",
        "pass a token id, `--client <CLIENT_ID>`, or `--family <FAMILY_ID>`",
    )))
}

fn normalized(raw: &str, what: &str) -> Result<String> {
    normalize_id_prefix(raw).ok_or_else(|| {
        anyhow::Error::new(HintedError::new(
            format!("nothing revoked — that is not a {what} id (ids are 4-12 hex characters)"),
            "copy an id from `onebrain gateway tokens list --all` — never paste the token itself",
        ))
    })
}

fn revoke_outcome_env(
    selector: &TokenSelector,
    outcome: RevokeOutcome,
) -> Result<Envelope<TokensRevokeData>> {
    let (kind, value) = (selector.kind(), selector.value());
    match outcome {
        RevokeOutcome::Revoked {
            newly_revoked,
            already_revoked,
        } => Ok(Envelope::ok(
            "gateway.tokens.revoke",
            None,
            TokensRevokeData {
                selector: kind,
                target: value.to_string(),
                revoked: newly_revoked,
                already_revoked,
            },
        )),
        RevokeOutcome::NotFound => Err(anyhow::Error::new(HintedError::new(
            format!("nothing revoked — no token matches {kind} `{value}`"),
            "run `onebrain gateway tokens list --all` to see token, client, and family ids",
        ))),
        RevokeOutcome::Ambiguous(ids) => Err(anyhow::Error::new(HintedError::new(
            format!(
                "nothing revoked — {kind} prefix `{value}` matches {} different ids",
                ids.len()
            ),
            format!("use more characters — one of: {}", ids.join(", ")),
        ))),
    }
}

pub(crate) fn clients_list_env(store: &AuthStore) -> Result<Envelope<ClientsListData>> {
    let clients = store.list_clients().context(store_unreadable())?;
    Ok(Envelope::ok(
        "gateway.clients.list",
        None,
        ClientsListData { clients },
    ))
}

pub(crate) fn clients_remove_env(
    store: &AuthStore,
    client_id: &str,
) -> Result<Envelope<ClientsRemoveData>> {
    match store.remove_client(client_id).context(store_unwritable())? {
        Some(RemovedClient {
            client_id,
            tokens_revoked,
            codes_removed,
        }) => Ok(Envelope::ok(
            "gateway.clients.remove",
            None,
            ClientsRemoveData {
                client_id,
                tokens_revoked,
                codes_removed,
            },
        )),
        None => Err(anyhow::Error::new(HintedError::new(
            format!(
                "nothing removed — no registered client has id `{}`",
                printable(client_id)
            ),
            "run `onebrain gateway clients list` to see client ids",
        ))),
    }
}

// ── Text renderers (pure) ────────────────────────────────────────────────

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Neutralize control characters (ANSI escapes, newlines) in client-supplied
/// strings before they reach a terminal. `client_name`/`redirect_uris` come
/// from an unauthenticated `/register` call.
fn printable(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

fn fmt_epoch(secs: u64) -> String {
    i64::try_from(secs)
        .ok()
        .and_then(|s| chrono::DateTime::<chrono::Utc>::from_timestamp(s, 0))
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| secs.to_string())
}

fn kind_str(kind: TokenKind) -> &'static str {
    match kind {
        TokenKind::Access => "access",
        TokenKind::Refresh => "refresh",
    }
}

fn status_str(status: TokenStatus) -> &'static str {
    match status {
        TokenStatus::Live => "live",
        TokenStatus::Expired => "expired",
        TokenStatus::Revoked => "revoked",
    }
}

fn app_str(app: AppType) -> &'static str {
    match app {
        AppType::Native => "native",
        AppType::Web => "web",
    }
}

pub(crate) fn render_tokens_list(env: &Envelope<TokensListData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    let mut out = String::new();
    if d.tokens.is_empty() {
        out.push_str(if d.all {
            "no tokens on disk"
        } else {
            "no live tokens"
        });
    } else {
        let noun = if d.all { "token" } else { "live token" };
        out.push_str(&format!(
            "{} {noun}{}\n",
            d.tokens.len(),
            plural(d.tokens.len())
        ));
        for t in &d.tokens {
            out.push_str(&format!(
                "  {}  {:<7}  {:<7}  client {}  family {}  issued {}  expires {}\n",
                t.id,
                kind_str(t.kind),
                status_str(t.status),
                printable(&t.client_id),
                t.family_id,
                fmt_epoch(t.issued),
                fmt_epoch(t.expires),
            ));
        }
    }
    if !d.all && d.hidden > 0 {
        out.push_str(&format!(
            "\n💡 {} expired or revoked token{} hidden — run `onebrain gateway tokens list --all` to include them",
            d.hidden,
            plural(d.hidden)
        ));
    }
    out
}

pub(crate) fn render_tokens_revoke(env: &Envelope<TokensRevokeData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    let mut out = if d.revoked.is_empty() {
        format!(
            "nothing new to revoke — {} matching token{} already revoked",
            d.already_revoked,
            plural(d.already_revoked)
        )
    } else {
        format!(
            "revoked {} token{}: {}",
            d.revoked.len(),
            plural(d.revoked.len()),
            d.revoked.join(", ")
        )
    };
    if d.selector == "id" {
        out.push_str(
            "\n💡 this revoked that one token only — to cut a client off completely run \
             `onebrain gateway tokens revoke --family <FAMILY_ID>` or \
             `onebrain gateway clients remove <CLIENT_ID>`",
        );
    }
    out
}

pub(crate) fn render_clients_list(env: &Envelope<ClientsListData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    if d.clients.is_empty() {
        return "no registered clients".to_string();
    }
    let mut out = format!(
        "{} registered client{}\n",
        d.clients.len(),
        plural(d.clients.len())
    );
    for c in &d.clients {
        let name = c
            .client_name
            .as_deref()
            .map(printable)
            .unwrap_or_else(|| "(unnamed)".to_string());
        out.push_str(&format!(
            "  {}  {name}  {}  {} live token{}  registered {}\n",
            printable(&c.client_id),
            app_str(c.application_type),
            c.live_tokens,
            plural(c.live_tokens),
            fmt_epoch(c.created),
        ));
    }
    out
}

pub(crate) fn render_clients_remove(env: &Envelope<ClientsRemoveData>) -> String {
    let d = env.data.as_ref().expect("ok envelope always has data");
    format!(
        "removed client {} — revoked {} token{}, deleted {} pending authorization code{}",
        printable(&d.client_id),
        d.tokens_revoked,
        plural(d.tokens_revoked),
        d.codes_removed,
        plural(d.codes_removed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::gateway::auth::store::RegisteredClient;

    fn temp_store() -> (tempfile::TempDir, AuthStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::open_at(dir.path().join("gateway")).unwrap();
        (dir, store)
    }

    fn args(
        id: Option<&str>,
        client: Option<&str>,
        family: Option<&str>,
    ) -> GatewayTokensRevokeArgs {
        GatewayTokensRevokeArgs {
            id: id.map(str::to_string),
            client: client.map(str::to_string),
            family: family.map(str::to_string),
        }
    }

    fn hinted(e: &anyhow::Error) -> &HintedError {
        e.downcast_ref::<HintedError>()
            .expect("error must carry a HintedError")
    }

    #[test]
    fn tokens_list_env_default_hides_non_live_and_counts_them() {
        let (_d, store) = temp_store();
        let (a, _r) = store.issue_token_pair("c1", "brain").unwrap();
        store.revoke_token(&a.token).unwrap();
        let env = tokens_list_env(&store, false).unwrap();
        let d = env.data.as_ref().unwrap();
        assert_eq!((d.tokens.len(), d.hidden), (1, 1));
        let env = tokens_list_env(&store, true).unwrap();
        let d = env.data.as_ref().unwrap();
        assert_eq!((d.tokens.len(), d.hidden), (2, 0));
        assert!(render_tokens_list(&env).starts_with("2 tokens\n"));
    }

    #[test]
    fn tokens_list_text_and_json_never_contain_a_raw_secret() {
        let (_d, store) = temp_store();
        let (a, r) = store.issue_token_pair("c1", "brain").unwrap();
        for all in [false, true] {
            let env = tokens_list_env(&store, all).unwrap();
            let text = render_tokens_list(&env);
            let json = serde_json::to_string(&env).unwrap();
            for secret in [&a.token, &r.token, &a.family] {
                assert!(!text.contains(secret.as_str()), "raw secret in text output");
                assert!(!json.contains(secret.as_str()), "raw secret in JSON output");
            }
        }
    }

    #[test]
    fn tokens_list_env_on_a_corrupt_store_is_a_hinted_error_not_empty() {
        let (d, store) = temp_store();
        std::fs::write(d.path().join("gateway").join("tokens.json"), "{not json").unwrap();
        let err = tokens_list_env(&store, false).unwrap_err();
        assert!(hinted(&err).plain.contains("unreadable or corrupt"));
    }

    #[test]
    fn render_tokens_list_empty_and_hidden_hint() {
        let (_d, store) = temp_store();
        assert_eq!(
            render_tokens_list(&tokens_list_env(&store, false).unwrap()),
            "no live tokens"
        );
        assert_eq!(
            render_tokens_list(&tokens_list_env(&store, true).unwrap()),
            "no tokens on disk"
        );
        let (a, r) = store.issue_token_pair("c1", "brain").unwrap();
        store.revoke_token(&a.token).unwrap();
        store.revoke_token(&r.token).unwrap();
        let text = render_tokens_list(&tokens_list_env(&store, false).unwrap());
        assert!(
            text.contains("2 expired or revoked tokens hidden"),
            "{text}"
        );
        assert!(text.contains("`onebrain gateway tokens list --all`"));
    }

    #[test]
    fn tokens_revoke_env_by_id_prefix_revokes_and_reports_it() {
        let (_d, store) = temp_store();
        let (a, _r) = store.issue_token_pair("c1", "brain").unwrap();
        let id = crate::commands::gateway::auth::store::display_id(&a.token);
        let env =
            tokens_revoke_env(&store, &args(Some(&id[..6].to_uppercase()), None, None)).unwrap();
        let d = env.data.as_ref().unwrap();
        assert_eq!(d.selector, "id");
        assert_eq!(d.target, id[..6]);
        assert_eq!(d.revoked, vec![id]);
        assert!(store.check_access(&a.token).unwrap().is_none());
    }

    #[test]
    fn tokens_revoke_env_rejects_a_raw_token_without_echoing_it() {
        let (_d, store) = temp_store();
        let (a, _r) = store.issue_token_pair("c1", "brain").unwrap();
        let err = tokens_revoke_env(&store, &args(Some(&a.token), None, None)).unwrap_err();
        let h = hinted(&err);
        assert!(
            !h.plain.contains(a.token.as_str()),
            "raw token echoed in plain"
        );
        assert!(
            !h.hint.contains(a.token.as_str()),
            "raw token echoed in hint"
        );
        assert!(
            !format!("{err:#}").contains(a.token.as_str()),
            "raw token in chain"
        );
        assert!(
            store.check_access(&a.token).unwrap().is_some(),
            "nothing revoked"
        );
    }

    #[test]
    fn revoke_outcome_env_maps_not_found_and_ambiguous_to_hinted_errors() {
        let sel = TokenSelector::Family("abcd".into());
        let nf = revoke_outcome_env(&sel, RevokeOutcome::NotFound).unwrap_err();
        assert_eq!(
            hinted(&nf).plain,
            "nothing revoked — no token matches family `abcd`"
        );
        let amb = revoke_outcome_env(
            &sel,
            RevokeOutcome::Ambiguous(vec!["abcd01".into(), "abcd02".into()]),
        )
        .unwrap_err();
        assert!(hinted(&amb).plain.contains("matches 2 different ids"));
        assert!(hinted(&amb).hint.contains("abcd01, abcd02"));
        assert!(!hinted(&amb).hint.ends_with('.'));
    }

    #[test]
    fn selector_from_args_with_no_selector_is_a_hinted_error() {
        let err = selector_from_args(&args(None, None, None)).unwrap_err();
        assert!(hinted(&err).hint.contains("--client <CLIENT_ID>"));
    }

    #[test]
    fn render_tokens_revoke_by_id_carries_the_cut_off_hint() {
        let env = |selector| {
            Envelope::ok(
                "gateway.tokens.revoke",
                None,
                TokensRevokeData {
                    selector,
                    target: "abcd".into(),
                    revoked: vec!["abcd12345678".into()],
                    already_revoked: 0,
                },
            )
        };
        let by_id = render_tokens_revoke(&env("id"));
        assert!(by_id.starts_with("revoked 1 token: abcd12345678"));
        assert!(by_id.contains("`onebrain gateway tokens revoke --family <FAMILY_ID>`"));
        assert!(!render_tokens_revoke(&env("family")).contains('💡'));
        let none = Envelope::ok(
            "gateway.tokens.revoke",
            None,
            TokensRevokeData {
                selector: "client",
                target: "c1".into(),
                revoked: vec![],
                already_revoked: 2,
            },
        );
        assert_eq!(
            render_tokens_revoke(&none),
            "nothing new to revoke — 2 matching tokens already revoked"
        );
    }

    #[test]
    fn clients_list_and_remove_envs() {
        let (_d, store) = temp_store();
        assert_eq!(
            render_clients_list(&clients_list_env(&store).unwrap()),
            "no registered clients"
        );
        store
            .register_client(RegisteredClient {
                client_id: "c1".into(),
                client_name: Some("Claude".into()),
                redirect_uris: vec!["http://127.0.0.1/cb".into()],
                application_type: AppType::Native,
                created: 0,
            })
            .unwrap();
        store.issue_token_pair("c1", "brain").unwrap();
        let text = render_clients_list(&clients_list_env(&store).unwrap());
        assert!(text.starts_with("1 registered client\n"), "{text}");
        assert!(text.contains("c1  Claude  native  2 live tokens  registered 1970-01-01 00:00 UTC"));

        let env = clients_remove_env(&store, "c1").unwrap();
        assert_eq!(
            render_clients_remove(&env),
            "removed client c1 — revoked 2 tokens, deleted 0 pending authorization codes"
        );
        let err = clients_remove_env(&store, "c1").unwrap_err();
        assert!(hinted(&err)
            .hint
            .contains("`onebrain gateway clients list`"));
    }

    #[test]
    fn render_clients_list_neutralizes_control_characters() {
        let env = Envelope::ok(
            "gateway.clients.list",
            None,
            ClientsListData {
                clients: vec![ClientView {
                    client_id: "c1".into(),
                    client_name: Some("Evil\u{1b}[31m\nName".into()),
                    application_type: AppType::Web,
                    redirect_uris: vec![],
                    created: 0,
                    live_tokens: 1,
                }],
            },
        );
        let text = render_clients_list(&env);
        assert!(!text.contains('\u{1b}'), "ANSI escape reached the terminal");
        assert!(text.contains("Evil?[31m?Name"), "{text}");
        assert!(text.contains("1 live token  "));
    }

    #[test]
    fn clients_remove_not_found_neutralizes_control_characters_in_the_id() {
        let (_d, store) = temp_store();
        let err = clients_remove_env(&store, "evil\u{1b}[31m\nid").unwrap_err();
        let plain = &hinted(&err).plain;
        assert!(!plain.contains('\u{1b}') && !plain.contains('\n'));
        assert!(plain.contains("evil?[31m?id"), "{plain}");
    }

    #[test]
    fn fmt_epoch_formats_utc_and_falls_back_on_overflow() {
        assert_eq!(fmt_epoch(0), "1970-01-01 00:00 UTC");
        assert_eq!(fmt_epoch(u64::MAX), u64::MAX.to_string());
    }
}
