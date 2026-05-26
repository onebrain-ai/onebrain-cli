# OneBrain CLI — Design & Internals

Reference docs for people reading the source: why the code is shaped the way it is, and the Rust patterns it leans on. This complements the top-level [`README.md`](../README.md) (what the tool does) and [`CONTRIBUTING.md`](../CONTRIBUTING.md) (how to build + submit).

## What's here

| Doc | Read it when you want to… |
|---|---|
| [`architecture.md`](architecture.md) | Understand the 4-crate workspace, how a command flows from `main` to the filesystem, and why the crate boundaries fall where they do. |
| [`decisions/`](decisions/) | Understand *why* a choice was made (Rust over Bun, direct-GitHub self-update, the canonical `Envelope`, …). One Architecture Decision Record (ADR) per choice. |
| [`rust-patterns.md`](rust-patterns.md) | Learn the idiomatic Rust this codebase uses — trait objects, compile-time target detection, error enums, atomic file swaps — each anchored to a real file. |

## Who this is for

- **Contributors** orienting before a change — start with `architecture.md`, then the relevant ADR.
- **People studying the codebase** as a worked example of a production Rust CLI.
- **Rust learners** — `rust-patterns.md` is a guided tour of the patterns, with file references you can open alongside.

> These docs describe intent and rationale. The source is the source of truth; if a doc and the code disagree, the code wins — please open a PR fixing the doc.
