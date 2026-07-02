# Rust patterns in this codebase

A guided tour of the idiomatic Rust OneBrain CLI leans on, each anchored to a real file you can open alongside. Aimed at readers learning Rust through a production codebase — the *why*, not just the *what*.

Line numbers drift as the code changes; the function/type names are the stable anchor.

---

## 1. Trait objects for a plug-in set — `Box<dyn Check>`

**Where:** `crates/onebrain-fs/src/doctor/`

`doctor` runs a list of independent health checks (config valid? folders present? hooks wired?). Each check implements a common `Check` trait, and the runner holds them as `Vec<Box<dyn Check>>` — a heterogeneous list of "things that can be checked," resolved at runtime (dynamic dispatch).

**Why this over an enum:** an enum would force every check into one `match` arm and one file; the trait-object list lets each check live in its own module and be added without touching the runner. The cost is a vtable indirection per call — irrelevant for ~10 checks run once.

**Learn:** this is the Rust answer to "I want a list of polymorphic handlers." Reach for `Box<dyn Trait>` when the set is open-ended and the per-call cost doesn't matter; reach for an `enum` when the set is closed and you want exhaustiveness checking.

---

## 2. Compile-time platform selection — `cfg!`

**Where:** `crates/onebrain-fs/src/update/install.rs` → `AssetInfo::for_running_target`

The self-update path must download the asset matching the host triple. Instead of detecting the platform at runtime, it uses `cfg!(all(target_arch = "aarch64", target_os = "macos"))` and friends — the compiler bakes in the branch for the platform being built, so a macOS-arm64 binary *only* ever asks for `aarch64-apple-darwin`.

```rust
let info = if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
    AssetInfo { triple: "aarch64-apple-darwin", extension: "tar.gz", binary_name: "onebrain" }
} else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
    // …
} else {
    return Err(UpdateError::Install(/* helpful arch/os/env hint */));
};
```

**Learn:** `cfg!(...)` is a boolean macro evaluated at compile time — distinct from the `#[cfg(...)]` *attribute* that includes/excludes whole items. Use `cfg!` when you want one function body that the compiler specializes; use `#[cfg]` when you want entirely different items per platform (see `set_executable`, which has a `#[cfg(unix)]` body and a `#[cfg(not(unix))]` no-op).

---

## 3. Errors as data — domain `enum` + a result alias

**Where:** `crates/onebrain-fs/src/update/` → `UpdateError`

Failure modes are modeled as an `enum` (`Network`, `GithubStatus(u16)`, `Decode`, `Install`, …) rather than stringly-typed errors. Callers `match` on the variant when they need to react (e.g. distinguish an HTTP status from a decode failure), and the variants carry just enough payload to render a useful message.

**Learn:** the Rust convention is one error enum per crate/module boundary, with `?` propagating up and `map_err` adding context at each layer (`.map_err(|e| UpdateError::Network(format!("GET {url}: {e}")))`). This keeps the happy path linear while every failure stays typed and attributable.

---

## 4. Naming a closure type — `type InstallFn = Box<dyn Fn(...) + Send + Sync>`

**Where:** `crates/onebrain-cli/src/commands/update.rs`

The install step is injectable so the TTY layer can wrap it with a spinner. Its type — `Box<dyn Fn(&str) -> Result<(), UpdateError> + Send + Sync>` — is noisy inline, so it gets a `type` alias. The source even notes clippy flags the raw shape; the alias is the readability fix.

**Learn:** function-as-value is everyday Rust. `Box<dyn Fn>` is a heap-allocated closure; the `+ Send + Sync` bounds let it cross threads. When a boxed-closure type repeats or gets long, alias it — the alias documents intent (`InstallFn`) better than the raw signature.

---

## 5. Shared mutable UI state across closures — `Arc<Mutex<Option<T>>>`

**Where:** `crates/onebrain-cli/src/commands/update.rs` → `build_tty_options`

Three closures (stdout sink, stderr sink, install wrapper) all need the *same* progress bar: the install wrapper creates it, the sinks route their output through it so lines don't trample the spinner. It's shared as `Arc<Mutex<Option<ProgressBar>>>`:

- `Arc` — multiple owners (each closure holds a clone of the handle).
- `Mutex` — they mutate it from different call sites; the lock serializes access.
- `Option` — the bar doesn't exist until the install phase starts, and is set back to `None` when it ends.

Note the lock is taken with `.lock().unwrap_or_else(|e| e.into_inner())` — if a closure panicked while holding the lock, this recovers the guard instead of poisoning every later call.

**Learn:** `Arc<Mutex<T>>` is the standard "shared mutable state" tool. The `Option` inside is a common refinement for "shared slot that's empty until later." Cloning an `Arc` is cheap (a refcount bump), not a deep copy.

---

## 6. Atomic file replacement — temp write + rename, with rollback

**Where:** `crates/onebrain-fs/src/update/install.rs` → `swap_binary`

Replacing the running binary can't be a naive truncate-and-write (a crash mid-write leaves a corrupt executable). The pattern: write the new bytes to a sibling `*.new`, `fsync`, `chmod 0755`, then **atomically `rename`** it over the target. On Unix `rename` over a running binary is legal (the old inode stays open). On Windows the live `.exe` is locked, so it's a two-step (`live → .old`, then `.new → live`) with an explicit rollback if the second step fails.

**Learn:** "write-temp-then-rename" is the canonical way to get atomic file updates on POSIX — `rename(2)` is atomic within a filesystem, so a reader sees either the whole old file or the whole new one, never a half-written mix. The Windows divergence (and the rollback path that surfaces its own failure to stderr) is a good study in handling platform reality without leaving the user with a broken install.

---

## 7. Options struct + `..Default::default()`

**Where:** `crates/onebrain-fs/src/update/` → `UpdateOptions`, used throughout `commands/update.rs`

`run_update` takes a single `UpdateOptions` struct instead of a long positional argument list. Callers set only the fields they care about and fill the rest with `..Default::default()`:

```rust
UpdateOptions { check: dry_run, fresh, ..Default::default() }
```

**Learn:** Rust has no default/named arguments, so the idiom is a struct with `#[derive(Default)]` and struct-update syntax. It keeps call sites readable, makes adding a new option backward-compatible (existing callers don't change), and pairs naturally with the builder-like wiring you see in `build_tty_options`.

---

## To expand

Patterns worth documenting next (open a PR): `serialize_for_mode` output dispatch + the `OutputMode` type · the `Envelope<T>` generic + `skip_serializing_if` · `resolve_vault` walk-up resolution · the `#[non_exhaustive]` structs that let v3.x add fields without breaking out-of-tree consumers.
