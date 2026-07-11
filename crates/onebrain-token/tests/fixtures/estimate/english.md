---
tags: [project, notes]
created: 2026-07-11
---

# Weekly Project Status

The onebrain-token crate ships pure transform functions with no I/O in the
hot path. Every lossy transform emits a machine-readable signal so an agent
always knows what was omitted and how to recover it. The funnel applies
registered transforms in order, runs the never-worse backstop, and records
exactly one gain event per call.

## Next steps

- Wire the config block and the `token gain` command.
- Add the generation counter and the two cache layers.
- Route MCP, CLI, and daemon HTTP surfaces through the shared funnel.
