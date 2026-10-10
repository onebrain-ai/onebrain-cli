# 0043 — Plain-English approval prompts from structured fields

- **Status:** accepted
- **Date:** 2026-10
- **Follows:** [0038](0038-gateway-policy-engine-and-human-approvals.md).
- **PRs:** [#420](https://github.com/onebrain-ai/onebrain-cli/pull/420).

## Context

The first approval prompts echoed the audit summary, a machine-oriented line such as `capture: title="..." vault=default text_chars=42`. During the phone check the person approving could not tell what was being asked, who asked, or what Allow would commit them to. In particular, under the default `ask_once` mode the second `brain_capture` ran **with no prompt at all** (a 30-minute grant, by design), and nothing on screen had said that Allow does that.

The text is also adversarial input. A client chooses its own name at registration, and a note title is chosen by the model.

## Decision

- **Build the wording from structured fields, never by parsing the summary string.** A pending approval carries the tool, a capped subject (note title, file, or query), the vault, the body length, the client name and id, and the grant window. `approval_view` renders them for all three surfaces: the Telegram prompt and its edits, the macOS dialog, and the `description` on `GET /approvals`. The audit `summary` stays an audit line.
- **Say what is being asked, in a sentence.** For example "Claude wants to save a new note", then the title, vault and length.
- **Say who is asking, honestly.** The client's name is self-declared and is labelled that way, shown with a short client id; an app with no name is shown as "an unnamed app" with its id. Names are capped at 64 characters.
- **Show the `ask_once` window.** When Allow will also cover repeat calls, the prompt says so and for how long, and the outcome shows the time up to which more notes are allowed.
- **The outcome states the decision only.** After resolution the message says "Allowed", "Denied", "Timed out" or "Answered on the Mac" together with the effect (for example "nothing was written to the vault"), and the buttons are removed.
- **Neutralise untrusted text.** Control and bidi characters become visible escapes, the subject is capped, and the note body is never an input; only its length is. A hostile title cannot spoof the layout or push a Telegram message past its length limit.
- **Show the time left** to answer, so a prompt that will be denied automatically says so.

## Consequences

- Approvers see the consequence of Allow, not just the request, which removes the surprise of an unprompted second write.
- Wording changes are a view-layer change; the audit format and the machine fields are untouched.
- The client name is still only a label. The short id (the first 8 and last 3 characters of the client id) is the stable part, and `gateway clients list` shows the full ids to match it against.
- The prompt is English-only. Titles and queries in other scripts (Thai, for example) are shown as written.

## Alternatives considered

- **Keep the audit summary as the prompt.** Rejected: it was the original problem.
- **Show the note text in the prompt.** Rejected: the body is large, can carry private content into a chat history, and would be an unbounded attacker-controlled field.
- **Hide the client name because it is unverified.** Rejected: showing it labelled is more useful than hiding it, and the id still tells two clients with the same name apart.
