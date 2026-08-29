# OneBrain Gateway — `onebrain gateway run`

`onebrain gateway run` starts a loopback Streamable HTTP MCP server serving a multi-vault tool pack — the v3.5 Gateway epic's shipped-so-far surface. This page covers the `gateway.yml` schema and its defaults, zero-config behavior, OAuth 2.1 authentication, the policy/approval/audit machinery every tool call passes through, and the current (deliberately narrow) security posture. See [`docs/reference/mcp.md`](reference/mcp.md#gateway-streamable-http) for the tool-by-tool reference and [ADR 0019](decisions/0019-native-mcp-server-staged-qmd-cutover.md) for the wider native-MCP architecture this sits alongside.

```bash
onebrain gateway run              # bind the configured (or default 7717) port
onebrain gateway run --port 0     # let the OS assign an ephemeral port
```

- Runs in the foreground until Ctrl-C.
- Binds **`127.0.0.1` only** — see [Loopback + no remote exposure yet](#loopback--no-remote-exposure-yet) below.
- The bound URL prints once to stdout on startup: `gateway listening on http://<bound-addr>/mcp`.

## What this skeleton ships

- One loopback HTTP endpoint (`http://127.0.0.1:<port>/mcp`), Streamable HTTP, protocol `2026-07-28` pinned as the negotiation fallback.
- Five tools — the **Brain pack**: four read-only (`capabilities`, `brain_tasks`, `brain_get`, `brain_search`) plus one write tool, `brain_capture` (see [`brain_capture`](#brain_capture) below) — gated by the [policy engine](#policy--approvals) like every other tool.
- **Multi-vault**: any vault named in `gateway.yml`'s `vaults:` map is reachable by name, per tool call — the first part of the codebase that tracks more than one vault at a time.
- `brain_search` always routes through the warm per-vault daemon rather than opening a direct search engine itself: a long-lived, multi-vault gateway process must never take an exclusive per-vault engine lock, or one vault's request would starve every other vault's request against the same gateway.
- **OAuth 2.1 authentication**: `/mcp` requires a Bearer access token — see [Authentication](#authentication) below. The `/.well-known/*` discovery documents and `/register`/`/authorize`/`/token` stay reachable without one (a client with no token yet must be able to bootstrap OAuth before it has one).
- **Policy, human approval, and an audit trail** for every tool call — see [Policy & approvals](#policy--approvals) below. `capabilities` reports each tool's risk class, the policy mode currently in force, and which approval channels can actually deliver a prompt on this machine right now — see [`capabilities` truthfulness](#capabilities-truthfulness).

**Not yet shipped** (a later PR in the v3.5 epic): a remote tunnel, so a phone or another machine can reach your gateway safely — see [Loopback + no remote exposure yet](#loopback--no-remote-exposure-yet). `capabilities` already lists the roadmapped `developer`/`files`/`mac` packs with `enabled: false` so a caller can see what's coming without probing for tools that don't exist yet.

## Zero-config behavior

`~/.onebrain/gateway.yml` is entirely optional. With no file present:

- The gateway still starts, on the default port `7717`.
- Every tool call resolves its vault the normal way when `vault` is omitted: the same env (`$ONEBRAIN_VAULT`) / walk-up chain `onebrain mcp` and the CLI search verbs use, rooted at the gateway process's own working directory.

So a bare `onebrain gateway run` launched from inside a vault directory serves that one vault with zero configuration. The config file exists for the **multi-vault** case: naming several vaults by name, or pinning a default vault that differs from wherever the process happens to be launched from.

## Logs

`gateway run` is a foreground process. Two lines go to **stdout** and are a stable contract, not log output: the pairing code, and `gateway listening on http://<addr>/mcp`. Everything else — startup warnings about your `gateway.yml`, a refused approval, a failed audit write, the full detail behind an error a client was only told a sanitized version of — goes to **stderr** at `info` and above.

Set `RUST_LOG` to change that, exactly as for `onebrain daemon`: `RUST_LOG=debug onebrain gateway run`, or `RUST_LOG=onebrain=trace` to turn up only this crate. Redirect with `2>gateway.log`; colour is only emitted when stderr is a terminal, so a redirected log is plain text.

If a gated tool call is failing and you cannot tell why, this is the first place to look — most of the gateway's refusals deliberately tell the *client* very little, and tell the *operator* here instead.

## `gateway.yml` schema

Machine-level config at `~/.onebrain/gateway.yml` — deliberately **not** per-vault (unlike `onebrain.yml`), because one gateway process spans multiple vaults.

| Key | Type | Default | Notes |
|---|---|---|---|
| `port` | number | `7717` | Loopback port `gateway run` binds when `--port` is omitted. `--port` on the command line always wins over this. |
| `default_vault` | path | unset | Vault served when a tool call omits `vault`. Unset falls through to `$ONEBRAIN_VAULT`, then walk-up from the gateway process's cwd — exactly like an explicit CLI `--vault` flag would win over both of those when it IS set. |
| `vaults` | map (name → path) | `{}` | Named vaults a tool call may select via its `vault` argument. An unknown name is a JSON-RPC `invalid_params` error listing the known names. |
| `public_url` | string | unset | The gateway's OAuth issuer base URL, for the still-unshipped remote tunnel (see [Loopback + no remote exposure yet](#loopback--no-remote-exposure-yet)). When set, `gateway run` advertises `public_url` as the issuer in every discovery document (`/.well-known/oauth-protected-resource`, `/.well-known/oauth-authorization-server`) and in the `/mcp` 401 `WWW-Authenticate` challenge, instead of `http://127.0.0.1:<bound-port>`. Must be a bare origin — `scheme://host[:port]` — with no path, query, or fragment (a single trailing `/` is trimmed automatically); `http://` is accepted only for a loopback host (`localhost`/`127.0.0.1`), every other host must use `https://`. `gateway run` validates this at startup and refuses to start on an invalid value (naming the `public_url` key in the error) rather than silently falling back to the loopback issuer. Setting this alone does not expose anything remotely — this build still binds `127.0.0.1` only. |
| `policy` | map | see below | Per-risk-class approval policy — see [Policy & approvals](#policy--approvals). |
| `telegram` | map | see below | Telegram approval-channel credentials — see [Telegram approval channel](#telegram-approval-channel). |

All keys are optional; a missing file behaves identically to an empty one.

Example naming two vaults with a default:

```yaml
port: 7717
default_vault: /Users/you/ob-1
vaults:
  personal: /Users/you/ob-1
  work: /Users/you/ob-work
```

A `brain_tasks`/`brain_get`/`brain_search` call passing `"vault": "work"` then serves `/Users/you/ob-work`; omitting `vault` serves `default_vault` (`/Users/you/ob-1`).

## Policy & approvals

Every gateway tool call — read or write — passes through a policy gate before it runs: classified into a **risk class**, checked against that class's configured **mode**, and appended to an **audit log** regardless of outcome. A call that needs human approval blocks until one of the **approval channels** below answers it, or it times out.

### `policy` block

```yaml
policy:
  read_only: auto            # capabilities, brain_tasks, brain_get, brain_search
  mutating: ask_once          # brain_capture
  destructive: ask_always     # no tool is classified destructive yet
  grant_ttl_minutes: 30       # how long an approval's resulting consent lasts
  approval_wait_seconds: 300  # how long a blocked call waits for a decision
```

Every tool call is classified into one of three **risk classes**, and each class has its own **mode**:

| Mode | Behavior |
|---|---|
| `auto` | Always allow — no approval, no grant involved. |
| `ask_once` | Requires approval once per `(client, vault, risk class)` triple; a live grant (see below) satisfies later calls until it expires. |
| `ask_always` | Always requires approval, even with a live grant — "ask every time." A grant recorded by an earlier approval never satisfies `ask_always`. |
| `deny` | Always refused — no approval is ever offered. |

The **defaults keep today's behavior safe with zero configuration**: every read-only tool defaults to `auto` (unchanged from before the policy engine existed), while any write tool defaults to `ask_once` and any future destructive tool defaults to `ask_always` — a config nobody wrote never silently auto-allows a write. `read_only`/`mutating`/`destructive` may each be set independently; a partial `policy:` block fills only the keys it omits with these defaults.

A **grant** is recorded the moment a human approves an `ask_once` call — an in-memory `(client_id, vault, risk class) → expires-at` entry, scoped to `grant_ttl_minutes` (default 30). The **vault is part of the scope**: approving a write into one vault never authorizes writes into another, because that is the consent the human was actually shown (the dialog and the audit summary both name the vault). The **tool is deliberately not** part of it — the modes are named per risk class, and every tool in a class is by definition equally powerful, so consent is per class. An `ask_always` approval records nothing at all: "always ask" must never leave standing consent behind. It is **never persisted**: restarting `gateway run` clears every grant, same as it clears every pending approval — a grant is consent for THIS running gateway process, not a standing credential written to disk. `approval_wait_seconds` (default 300 — five minutes) bounds how long a BLOCKED call waits for a first decision before giving up; this is a separate knob from `grant_ttl_minutes`, since "ask me and wait up to five minutes" and "then remember it for a day" are independent choices an operator may want to make separately. **`approval_wait_seconds: 0` is legal and means "time out immediately"** — fail-CLOSED, so every call needing approval is refused and nothing is ever written. It exists because this repo's own tests set it deliberately; in a real config it is almost always a typo, so `gateway run` logs a startup warning naming the key when it loads one (on stderr — see [Logs](#logs)).

Every tool call is also checked against the OAuth token's own `scope` — a token whose scope doesn't cover the pack a tool belongs to (today, only the `"brain"` pack exists) is denied outright, before the mode above is even consulted, regardless of how permissive that mode is.

### Approval channels

When a call needs approval (`ask_once` with no live grant, or `ask_always`), the gateway registers a pending approval and blocks the tool call until a human answers it — through whichever of these channels responds first:

| Channel | Status | Notes |
|---|---|---|
| **Native macOS dialog** | Shipped | A `display dialog` popup (via `osascript`) shown directly on the machine running `gateway run` — Approve/Deny buttons, no browser needed. The dialog **dismisses itself** when the call it belongs to reaches its `approval_wait_seconds` deadline, so you are never left looking at an Approve button that would silently do nothing. Available only when this build targets macOS AND `osascript` resolves on `$PATH`; `capabilities` reports the live truth for this machine (see below). |
| **Operator HTTP surface** (`/approvals`) | Shipped | `GET /approvals` lists every pending call; `POST /approvals/{id}` with `{"decision":"approve"}` or `{"decision":"deny"}` resolves one. Gated by the gateway's **pairing code** via the `X-OneBrain-Pairing` header — the SAME code that pairs a new OAuth client at `/authorize` — never by a connector's OAuth bearer token. This is deliberate: `/approvals` sits OUTSIDE the `/mcp` Bearer layer entirely, so a connector can never satisfy its own approval gate with the very token it's asking permission to keep using. Always available in this build (mounted unconditionally); using it still requires a human who knows the pairing code, and wrong codes are rate-limited (see below). |
| **Telegram bot** | Shipped — Gateway PR 5 | Inline Approve/Deny buttons delivered by a dedicated Telegram bot you set up once with `onebrain gateway telegram setup`. Its own auth model is unrelated to the pairing code below — see [Telegram approval channel](#telegram-approval-channel). |

Every shipped channel resolves the SAME pending-approval registry — whichever answers first wins, and the others are simply a no-op from then on. If nothing answers within `approval_wait_seconds`, the call fails with a timeout error and no grant is recorded.

**Wrong pairing codes on `/approvals` are rate-limited by the same budget as `/authorize`**: five consecutive wrong codes anywhere — the consent page, the approvals header, or a mix of both — lock *every* pairing-code check for 60 seconds, and a correct code inside that window is refused too. One credential, one budget: a second, separately-counted limiter would simply double the guess rate available to whoever is guessing. The visible consequence is that a burst of failed `/authorize` attempts can briefly lock you out of approving as well; that is the intended trade, and it matches the limiter's existing design (one global counter, not per-client or per-IP, because there is exactly one pairing code and one human).

**The adversarial direction of that same trade, stated plainly so you can recognize it.** Because the budget is global and `/approvals` sits outside the Bearer layer, anyone who can reach the gateway at all — holding **no OAuth token whatsoever** — can send five wrong `X-OneBrain-Pairing` headers a minute, indefinitely, and hold the operator out of both `/approvals` and `/authorize`: every pending write then times out unanswered, and no new connector can pair. This is fail-safe and deliberate (a lockout refuses writes; it never permits one), and it stays that way. But the symptom is easy to misread as a bug — **`/approvals` returning `401` for a pairing code you know is correct, persistently, is what this attack looks like from the operator's side**, not a stale or rotated code. On a loopback-only gateway the reachability required is already close to game over; it becomes materially reachable the moment a tunnel or `public_url` puts the gateway on a network someone else can address, which is why it belongs to the pre-tunnel security checklist ([#404](https://github.com/onebrain-ai/onebrain-cli/issues/404)) rather than to this PR.

**None of the above applies to the Telegram channel.** It has no pairing code and sits entirely outside this budget — see [Telegram approval channel](#telegram-approval-channel) for its own (different) auth model. A burst of wrong `/authorize`/`/approvals` guesses can lock you out of *those* two surfaces without touching Telegram at all, and an attacker spamming your bot cannot draw down or interact with the pairing-code lockout either — the two systems share no state.

The registry is **bounded**: at most 16 pending approvals overall, and at most 4 for any one client. Past either limit a gated call is refused immediately with a policy error (audited as `denied`) instead of queueing another prompt. The caps are fixed, not configurable: they sit far above what a human at a console could answer, so they only ever bite a runaway client. An entry whose caller has gone away stops counting once its own `approval_wait_seconds` has elapsed, so abandoned calls cannot wedge the limit.

Precisely: those caps bound how many approvals are pending **at once**, not how many prompts a client can cause over time — each timeout frees a slot for the next call. What bounds a native dialog's *lifetime* is a separate mechanism: every dialog is opened with a self-dismiss deadline equal to its own call's remaining `approval_wait_seconds`, so it is never left on screen (nor its worker thread held) after the call it belongs to has given up. The two work together; neither replaces the other. Note that a dialog answered after its call has already timed out grants nothing in any case — the pending entry is gone before a grant could be recorded, so the failure mode here was always a stale prompt, never an unintended approval.

### Telegram approval channel

`onebrain gateway telegram setup` walks you through wiring up a bot: paste a token from `@BotFather`, send a short one-time code back to the bot in Telegram, and the wizard writes the resulting credentials into `~/.onebrain/gateway.yml`'s `telegram:` block and sends a confirmation message through the freshly-configured channel.

```bash
onebrain gateway telegram setup
```

**A dedicated bot is required.** You cannot point this at the same bot the OneBrain Claude Code plugin already uses for its own Telegram integration (`onebrain.yml`'s `notifications.telegram_chat_id` — a completely separate, vault-level key, unrelated to anything below): Telegram allows exactly **one** `getUpdates` long-poll consumer per bot token, and the plugin's own poller already holds that slot for its bot. Pointing the gateway at the same token would have the two pollers fight over the same update stream, each stealing updates the other needed. Create a fresh bot with `@BotFather`'s `/newbot` for the gateway specifically — it costs nothing and takes under a minute.

**Config keys** (`~/.onebrain/gateway.yml`, under `telegram:`):

| Key | Type | Notes |
|---|---|---|
| `bot_token` | string | Minted by `@BotFather`. Machine-level, never vault-level. Never logged, never echoed back over any HTTP response, and dropped from any config dump this crate might ever produce (`serde`'s `skip_serializing`) — only ever read back in from `gateway.yml` on disk. |
| `chat_id` | number | The Telegram chat this bot sends prompts to and reads button-presses from. **Must be a POSITIVE id** — Telegram gives one-to-one private chats positive ids and gives groups/supergroups/channels negative ones, and this channel only works for a private chat with exactly one human on the other end (see the auth model below). A `bot_token` set with a non-positive `chat_id` does **not** activate the channel at all — the same as leaving `telegram:` out entirely, `capabilities.approval_channels.telegram` reports `false`, and no prompt is ever sent through it (a blocked call falls back to whichever other channel is configured, or simply times out). `gateway run` still warns about exactly this shape on stderr at startup, since a `bot_token` alone signals the operator clearly meant to configure Telegram — that warning is the only place this misconfiguration is visible; nothing in Telegram itself will show a missing message. |

Both fields default to empty/zero, which reads as "not configured" — Telegram never issues a bot an empty token or a chat the id `0`, so the plain default doubles as an unambiguous sentinel with no `Option` wrapper needed. Setting `bot_token` alone (with `chat_id` still `0`, or negative) does not activate the channel.

**Auth model — no pairing code.** Unlike the native dialog and `/approvals`, Telegram approvals are never gated by the gateway's pairing code at all. Authorization is: does this button-press's own Telegram user id (`from_id`) equal the configured `chat_id`? That holds because a private chat's id *is* the human's own user id under Telegram's own convention, and it is the check the whole channel's security rests on. A press from anyone else is refused per-tap (logged, and the tapper's client is told "not authorized") with no shared rate-limit budget to exhaust — see the note under [Approval channels](#approval-channels) above for why this also means an attacker hammering your bot can never lock you out of `/authorize` or `/approvals`, or vice versa. A SECOND, purely defensive check can also produce the same "not authorized" toast: if the tapped message's own chat (as Telegram reports it) disagrees with the configured `chat_id` — a message forwarded into, or otherwise visible from, another chat — the press is refused too, even though `from_id` alone would already have been the deciding factor either way. If you ever see "not authorized" from a tap you made yourself, either check could be why.

**Message flow.** A pending approval's Telegram message shows the same `Client: <id>` / `Tool: <name>` framing and bounded summary the native dialog and `/approvals` JSON show — never the raw tool-call body (a `brain_capture` note's actual text never leaves the process; only its character count does) — with two inline buttons, **✅ Approve** and **⛔ Deny**. Once the approval resolves, through *whichever* channel actually answered it, the Telegram message is edited to show the outcome and its buttons are cleared — so a Telegram-side approver never sees a stale, tappable button for a call that already finished one way or another.

**The poller.** Watching for a button-press is demand-driven, not always-on: firing a prompt spawns one background thread (per gateway process, the first time it's needed) that long-polls Telegram's `getUpdates` for up to 25 seconds per cycle. Because of that long-poll window, the thread can linger up to **25 seconds** past the last pending approval going away before it notices there's nothing left to watch for and exits — bounded, and deliberate, not a leak. Its progress cursor is persisted to a bot-token-keyed file, `~/.onebrain/gateway/telegram-<hash-of-token>.offset` (0600, alongside the rest of the gateway's on-disk state), written once per `getUpdates` batch — so a clean `gateway run` restart resumes from where it left off, a crash between a batch and its persisted cursor at worst re-fetches and re-handles a few already-seen updates on the next start (harmless: approvals are in-memory and per-process, so a stale resolve for an already-decided id is just a no-op), and rotating `bot_token` to a different bot starts that new bot with a clean cursor rather than inheriting one scoped to the old bot's own update stream.

An `ONEBRAIN_GATEWAY_DISABLE_TELEGRAM_APPROVAL` test-only escape hatch also exists — see the environment variables listed under [`capabilities` truthfulness](#capabilities-truthfulness) just below, where its two siblings are documented.

### `capabilities` truthfulness

`capabilities` reports, for every tool in every pack: its risk class, and the policy mode CURRENTLY in force for that class (reading straight off the live `gateway.yml` — never a hardcoded default), plus an `approval_channels` object naming which channels can actually deliver an approval prompt **on this machine, right now**:

```json
"approval_channels": {
  "native": true,
  "http": true,
  "telegram": false,
  "note": "..."
}
```

This exists so a caller is never told a write CAN be approved through a channel that cannot actually carry the prompt to a human — `native` reflects the real, live `osascript`-on-`$PATH` probe (and can be forced off — see the environment variable below), `http` is always `true` in this build (the surface is unconditionally mounted), and `telegram` is `true` iff `gateway.yml`'s `telegram.bot_token` and `telegram.chat_id` are both set to a valid shape (see [Telegram approval channel](#telegram-approval-channel)) and the channel hasn't been explicitly disabled.

Like `native`, **`telegram` reports configured-ness, not liveness**: it never makes a network call to Telegram, so it can't tell you whether the token is still valid or the bot has been blocked — only that `gateway.yml` names one. Validating the token for real (a live `getMe` call) happens once, at setup time, in `onebrain gateway telegram setup` — a human is already watching then and can act on a bad-token error immediately; re-probing Telegram on every `capabilities` call would add real latency and a real failure mode (a transient Telegram outage) to a field every other channel here answers from purely local state.

`ONEBRAIN_GATEWAY_DISABLE_NATIVE_APPROVAL` (any non-empty value) forces the native channel off regardless of platform — a test-only escape hatch (this crate's own end-to-end test suite sets it on the gateway subprocess it spawns, so CI never pops a real, unattended dialog), not a documented operator-facing config key. An operator who wants the native channel off for real should set `policy.mutating`/`policy.destructive` to `auto` or `deny` instead — that never reaches the approval flow at all, on any channel.

`ONEBRAIN_GATEWAY_DISABLE_DAEMON_REINDEX` (any non-empty value) is its sibling: it switches off `brain_capture`'s best-effort reindex, which would otherwise start a warm daemon subprocess for the vault. Also test-only, for the same reason — a test binary must not leave an `onebrain daemon __run` process behind on a developer machine or a CI runner — and it covers only that best-effort step, never `brain_search`, whose daemon use is load-bearing. Note this is a different switch from the CLI-wide `ONEBRAIN_NO_DAEMON`, which gates only passive routing to an already-running daemon and does not reach either of the gateway's paths.

`ONEBRAIN_GATEWAY_DISABLE_TELEGRAM_APPROVAL` (any non-empty value) is the Telegram sibling of the native switch above: it force-disables the channel regardless of what `gateway.yml` configures. Same test-only shape and same real-off alternative (`policy.mutating`/`policy.destructive` to `auto` or `deny`).

All three are **presence switches**, matching `ONEBRAIN_NO_DAEMON`'s existing convention: any non-empty value turns them ON, and a set-but-empty value counts as unset. They do not parse booleans — `=0` and `=false` are non-empty, and therefore ON.

### Audit log

Every tool call — allowed or not — is appended as one JSON line to `~/.onebrain/gateway/audit/YYYY-MM.jsonl` (one file per month, created 0700/0600 like the rest of the gateway's on-disk state). Each line:

```json
{"ts":1735689600,"client_id":"...","tool":"brain_capture","vault":"t1","args_summary":"capture: title=Some(\"...\") vault=None text_chars=42","decision":"approved","channel":"telegram","duration_ms":812,"outcome":"ok"}
```

| Field | Meaning |
|---|---|
| `ts` | Unix epoch seconds the call was recorded at. |
| `client_id` | The calling OAuth client's id. |
| `tool` | Tool name. |
| `vault` | Named vault the call resolved, when one was resolvable. |
| `args_summary` | A **redacted**, one-line description of the call's arguments — e.g. a `brain_capture` call's own note body NEVER appears here, only its character count. Bounded in length: a summary built from an oversized caller argument is cut and marked `[truncated, N bytes total]`, so one client cannot grow this file by sending large parameters. |
| `decision` | `auto` (policy allowed it outright), `approved` (a human approved it), `denied` (refused — either a human answered "deny", or policy refused it with no human involved at all: a `deny` mode, an OAuth scope/pack mismatch, an unidentifiable caller, or the pending-approval cap being reached), or `timedout` (nothing answered within `approval_wait_seconds`). |
| `channel` | Which approval channel produced a human decision: `"native"`, `"http"` (the `/approvals` surface), or `"telegram"`. `null` whenever no channel was ever consulted — `auto` (policy allowed it outright, no human involved), a policy `denied` with no human involved, and `timedout` (nothing ever answered) all report `null`; only `approved` and a human `denied` ever carry a channel name. |
| `duration_ms` | Wall-clock time the call took, including any time spent blocked on approval. |
| `outcome` | `ok` or `error` — whether the tool's own logic succeeded once it was allowed to run. |

Writing an entry never blocks or fails the tool call it describes — by the time an entry exists to write, the call it records has already happened; a write failure (full disk, a yanked permission bit) is logged to the gateway's own process log and otherwise ignored.

**No rotation yet.** A month file grows without limit and is never pruned or compressed; a busy gateway's audit directory grows forever until someone deletes old months by hand. Each individual line is bounded (see `args_summary` above), so no single call can inflate the file disproportionately, but the number of lines is not. Retention/rotation is deliberately out of scope for this PR and is tracked separately — it is a policy question (how long should a security record be kept, and who is allowed to delete one?) rather than a missing size check.

### `brain_capture`

The gateway's first WRITE tool: creates a new inbox note (`<inbox-folder>/YYYY-MM-DD-<slug>.md`) from a `title` (optional) and `text` body, classified `RiskClass::Mutating` — so under the default policy it needs a human's `ask_once` approval the first time, then proceeds automatically for `grant_ttl_minutes`. Three independent guards confine every derived path to the vault: **syntactic** (every path component must be a plain, non-`..`, non-absolute segment), **canonicalization** (the resolved parent directory must still live under the canonicalized vault root, catching a symlinked-out folder too), and an **equality check** that the confined path is exactly the path the write will open — the underlying note writer joins the vault root and relative path with no confinement of its own, so anything that resolves elsewhere (even somewhere still inside the vault, via a symlinked inbox) is refused rather than written through a link the guard did not vouch for. So a crafted `title` can never write outside the vault. What the guards do NOT cover, stated plainly: the parent is canonicalized *before* the write, so swapping it for an escaping symlink in the window between the check and the write is not caught — closing that needs handle-relative I/O in the filesystem layer, and it requires an attacker who already has write access inside the vault. The note's filename slug is derived from `title`, falling back to the first words of `text`. It keeps Unicode alphanumerics, so a Thai, Japanese, Korean, or Cyrillic title produces a filename in that script rather than collapsing to a marker. That charset rule is **modelled on** the one `onebrain note new` uses — keep alphanumerics, collapse every run of anything else to a single `-` — but the two are separate helpers and do **not** produce the same filename for the same title. Three deliberate differences, stated so this is not re-derived wrongly later: (1) `onebrain note new` derives no filename from a title at all — the caller supplies the relative path, and the note's *title* is derived from that path's stem; its slug helper only fills the `{{slug}}` template variable. (2) The gateway re-filters the output of Unicode lowercasing, so `İ` becomes `i`; the filesystem helper does not, and keeps the combining mark that `İ` lowercases into. (3) The gateway caps the slug at 60 characters and 120 bytes; the filesystem helper has no cap. That byte half of the cap is what keeps a multi-byte title from overrunning a filesystem name limit — a character cap alone would not, since one character can be four bytes. Only input with no alphanumeric content at all in any script (punctuation-only, emoji-only) falls through to the fixed `capture` marker, and that marker gets a short random suffix so repeated fallbacks in one day do not collide with each other. A same-day, same-slug collision surfaces as a clean tool error naming the vault-relative path, rather than overwriting the existing note. An empty (or whitespace-only) `text` is rejected as `invalid_params` — a capture with no body would write a titled, empty stub indistinguishable from one whose body was lost. A best-effort reindex request follows the write, so `brain_search` can find the new note without waiting for the vault's next scheduled reindex. That request is **detached**: the tool call returns as soon as the note is on disk and does not wait for the daemon, which may need a full cold start. A reindex failure never fails the capture either — the note is already written by that point; the only consequence is that `brain_search` lags until the next reindex.

## Authentication

`/mcp` is an OAuth 2.1 resource server: every request needs `Authorization: Bearer <access-token>`, or it gets a `401` with a `WWW-Authenticate` header pointing at the discovery document below. Getting a token is a standard OAuth 2.1 authorization-code + PKCE flow, gated by a **device-pairing code** — the human-in-the-loop step that stands in for a client secret this authorization server deliberately never issues (see [Token semantics](#token-semantics)).

### The flow, in prose

1. **Discovery.** A client with no token GETs `{issuer}/.well-known/oauth-protected-resource` (RFC 9728 — also served at the `/mcp`-suffixed path, since `/mcp` is itself the protected resource) to learn its authorization server, then that server's `{issuer}/.well-known/oauth-authorization-server` (RFC 8414) to learn the `/register`/`/authorize`/`/token` endpoints and that only PKCE `S256` is supported (`plain` is rejected — OAuth 2.1 drops it entirely).
2. **Registration.** The client `POST`s `{issuer}/register` (RFC 7591) with its `redirect_uris` and an `application_type` of `"web"` (must use `https://`) or `"native"` (must use a loopback `http://localhost`/`http://127.0.0.1` redirect, RFC 8252 §7.3). Every client this authorization server mints is **public** — no client secret is ever generated or stored; `token_endpoint_auth_method` is always `"none"`.
3. **Authorize.** The client sends the user's browser to `GET {issuer}/authorize` with the standard `response_type=code`, `client_id`, `redirect_uri`, `code_challenge`/`code_challenge_method=S256`, and `state` parameters. This renders the **one human checkpoint** in the whole surface: a consent page showing the requesting application's name, requested scope, and the redirect target's host, with a single field — the pairing code shown on the gateway (see [Pairing](#pairing) below). A wrong code re-renders the same page with a generic "incorrect" notice (never which check failed); five consecutive wrong submissions lock pairing out for 60 seconds, even for a correct code submitted mid-lockout. A correct code mints a single-use authorization code and redirects back to `redirect_uri` with `code`, `state`, and `iss` (RFC 9207) query parameters.
4. **Token exchange.** The client `POST`s `{issuer}/token` with `grant_type=authorization_code`, the code, and its PKCE `code_verifier`. A valid exchange returns an opaque `access_token` + `refresh_token` pair; the code is consumed on presentation regardless of outcome, so it can never be redeemed twice.
5. **Calling `/mcp`.** Every request carries `Authorization: Bearer <access_token>` until it expires (see below), at which point the client exchanges its `refresh_token` for a fresh pair the same way (`grant_type=refresh_token`).

### Pairing

The device-pairing code is the credential a human types once, at `/authorize`, to approve a new client — the gateway's stand-in for "you, physically at this machine, said yes."

- **`onebrain gateway run`** mints the code on first run and prints it to stdout — the *only* place it is ever shown: never logged, never returned over any HTTP response (an automated test asserts this directly). The code is stable across restarts; running `gateway run` again does not rotate it.
- **`onebrain gateway pair`** prints the current code without starting the gateway — useful from a second terminal, or when the gateway is already running headless/backgrounded. `onebrain gateway pair --rotate` mints a fresh code in its place, immediately invalidating the old one (any client mid-pairing with the old code must restart `/authorize`).

### Token semantics

Every credential this authorization server mints — authorization codes, access tokens, refresh tokens — is a random **opaque** string (32 bytes from the OS CSPRNG, base64url-encoded), never a signed/self-describing token like a JWT. Opaque tokens give exact revocation for free: revoking one is deleting a store entry, not maintaining a denylist alongside a signature scheme.

| Credential | Lifetime | Notes |
|---|---|---|
| Authorization code | 10 minutes (600s) | Single-use — consumed the moment it's presented at `/token`, whether or not the rest of the request is valid. A later replay of an already-used code revokes every token that code ever produced. |
| Access token | 1 hour | Presented as `Authorization: Bearer <token>` on every `/mcp` call. |
| Refresh token | 30 days | **Rotates on every use** (RFC 6749 §6 / OAuth 2.1 §4.14.3): each `grant_type=refresh_token` exchange invalidates the presented token and mints a new pair. Presenting an already-rotated refresh token again is treated as a leaked-token signal — the entire token family it descended from (every access/refresh token minted since the original code exchange) is revoked immediately, not just the reused token. |

### Not yet shipped

**Client ID Metadata Documents (CIMD)** — letting a client identify itself by a `https://` URL instead of going through `/register` — are deferred to a follow-up PR; landing CIMD safely requires an SSRF-safe fetch of that URL (the AS would otherwise follow a client-supplied URL from inside the gateway process), which is its own piece of design work. Every client today registers via `/register` (RFC 7591) instead.

### Loopback + no remote exposure yet

The bind address is still hard-coded to `127.0.0.1` — there is no `--bind` flag, no `$ONEBRAIN_BIND`-style escape hatch (unlike [`onebrain serve`](serve.md#containers--self-host--onebrain_bind)), and no config key to change it. OAuth authenticates *who* may call `/mcp`; it does not by itself make exposing this port beyond the local machine safe — do not put it behind a plain reverse proxy or port-forward it to another host. A remote tunnel (so a phone or another machine can reach your gateway through the same pairing flow) is planned for a later PR in the v3.5 epic — until then, `onebrain gateway run` is a localhost-only tool: a local MCP client that wants Streamable HTTP instead of stdio, or a testing/development target.
