# Scheduler artifact corpus

Candidate scheduler artifacts, checked against the **real** OS scheduler in CI.

## Why this exists

The v3.4.20 design went through two review rounds, and both times the errors
clustered in the same place: XML and unit-file semantics we could not exercise
locally. Reading the schema and eyeballing rendered output is not a gate — it is
what let two blockers reach a spec.

A throwaway spike settled three open questions in fifteen minutes by feeding
candidate documents to a real Task Scheduler. This directory makes that
permanent, so the scheduler validates our documents instead of a human reading
them.

## Layout

```
windows/   Task Scheduler XML, round-tripped through `schtasks /Create /XML`
linux/     systemd units, checked with `systemd-analyze verify`
```

**Filename encodes the expectation** — the runner needs no manifest:

- `accept-*` — the OS must accept it. A rejection fails the build.
- `reject-*` — the OS must reject it. **An acceptance also fails the build**, because a
  `reject-` case exists to pin a limitation we are designing around. If the OS
  starts accepting it, our design is working around a constraint that no longer
  exists and the corpus should be revisited.
- `accept-*.expect` — optional companion holding one regex, matched against
  `schtasks /Query /V /FO LIST`. **Installing cleanly and meaning what we meant
  are different claims**, and only the second one matters. Use it wherever the
  schema would happily accept a document that does the wrong thing.

## Measured facts these cases pin

Each was established by running against a real scheduler on 2026-07-27, not by
reading documentation.

| case | fact |
|---|---|
| `accept-monthly-with-months.xml` + `.expect` | `<Months>` present → fires only in the named month. The `.expect` pins `Next Run Time` to a **March** date. Omitting `<Months>` is *accepted* by Windows, which silently fills in "Every month" — so a cron meaning *1 March* would fire 12×/year. That case cannot be a `reject-` entry (the OS takes it); the `.expect` is what catches it, and the renderer must additionally be unit-tested never to emit it. |
| `reject-weekly-with-months.xml` | `ScheduleByWeek` **cannot carry `<Months>`** — `ERROR: The task XML contains an unexpected node`. So *weekday + month* (e.g. `0 17 * 3 5`) has no single-trigger form at all. |
| `reject-calendar-without-start-boundary.xml` | `<StartBoundary>` is required and is where time-of-day lives. Pinned so a renderer regression that drops it fails here rather than shipping a task that never runs. |
| `accept-repetition-every-minute.xml` | `<Repetition><Interval>PT1M</Interval>` is the only form for a wildcard minute (`* 9 * * *`); no `ScheduleBy*` subtree expresses it. |
| `accept-one-shot-self-deleting.xml` | `<EndBoundary>` + `<DeleteExpiredTaskAfter>` under `<Settings>`. Measured: the task disappears ~180 s after registration with a 60 s `EndBoundary` and `PT1M` (the duration runs from the **EndBoundary**, not the fire). Without `EndBoundary` the trigger never expires and the task is **never** deleted. |
| `accept-daily.service` / `.timer` | The baseline recurring pair, including `StandardOutput=append:` — how a systemd job writes to a file rather than the journal — and `Persistent=false`, which means a fire missed while asleep is **not** caught up (launchd *does* catch up; a deliberate divergence). |
| `accept-one-shot.service` | `ExecStopPost=` self-removal with **`--no-block`**. Blocking here deadlocks: `systemctl` waits on the manager's job queue while the manager waits for `ExecStopPost` to exit. |
| `reject-unknown-directive.service` | A canary. A corpus whose negative cases never fail is indistinguishable from a checker that does nothing. |

## Adding a case

Add the file; the runner picks it up by prefix. Put the *reason* in an XML
comment or a unit-file comment inside the artifact itself — a corpus whose
entries do not say why they exist decays into noise within one release.

## What this does NOT prove

Acceptance is not execution. `systemd-analyze verify` is a static check and
`schtasks /Create` only proves the document is well-formed and installable.
That a job actually **fires** is proven separately — by the Windows fire-proof
job, and on Linux by a manual VM run (see the waiver in the v3.4.20 dashboard).
