---
tags: [onebrain, cli, token-optimization]
created: 2026-07-11
---

# v3.4.10 Token Optimization — Notes

Line one: the funnel applies registered transforms in order, gated by level.
Line two: never_worse compares byte length and returns the original when the
transformed payload would be larger.
Line three: every optimized call records exactly one gain event.
Line four: the ladder has four rungs — off, conservative, balanced, aggressive.
Line five: conservative is the default and is strictly lossless.
Line six: balanced turns the already-sent ledger on.
Line seven: aggressive adds progressive disclosure and head-only multi_get.
Line eight: estimate_tokens is char-aware, not bytes/4.
Line nine: the JSONL gain writer rotates monthly and tolerates a partial last line.
Line ten: this fixture exists purely to be long enough to trigger a cap.
