# 0009 — Full Raspberry Pi / ARM matrix, ARMv6-first by default

- **Status:** accepted
- **Date:** 2026-05-23

## Context

The v3.0.0 GA shipped 7 platforms but no 32-bit ARM, leaving every pre-ARMv8 Raspberry Pi (Pi 1, Pi Zero, Pi Zero W) and any 32-bit Pi OS install uncovered. ARM is unforgiving: Pi 1/Zero are ARMv6, Pi 2+/Zero 2 are ARMv7/ARMv8, and the wrong binary doesn't degrade gracefully — an ARMv7 binary traps with SIGILL on an ARMv6 core. The kernel also lies: `/proc/cpuinfo` reports `CPU architecture: 7` even on ARMv6 Pis (the chip supports VMSAv7 memory addressing despite an ARMv6 instruction set).

## Decision

Add `armv7-unknown-linux-gnueabihf` (Pi 2 v1.1+ · Pi 3/4/5 on a 32-bit OS) and `arm-unknown-linux-gnueabihf` (ARMv6 · Pi 1 · Pi Zero) to the release matrix, cross-compiled on Ubuntu — taking it to **9 platforms**, every Pi from 1 to 5 covered. When 32-bit ARM detection is inconclusive, **default to ARMv6**: an ARMv6 binary runs (slower) on an ARMv7 host, but an ARMv7 binary crashes on ARMv6. Correctness over speed. The npm wrapper detects via `/proc/cpuinfo`, checking the reliable `model name` line *before* the misleading `CPU architecture` line; `ONEBRAIN_CLI_ARM=v7` forces the faster build.

## Consequences

- Every Raspberry Pi has a published binary out of the box.
- A Pi 4 on a 32-bit OS gets the slower ARMv6 binary unless the user overrides — an accepted, safe default (a SIGILL crash is far worse than a perf hit).
- Two more build targets per release; CI cross-compiles via `gcc-arm-linux-gnueabihf` (the matrix-driven `cross_prefix` derives the apt package, linker, and strip binary).
