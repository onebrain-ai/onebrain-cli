#!/usr/bin/env node
/**
 * @onebrain-ai/cli shim — execs the platform-native onebrain binary that
 * `postinstall.js` extracted into this directory. Uniform across darwin,
 * linux, and win32 so npm consumers get a single `onebrain` command name.
 *
 * Exit code is propagated from the child; if the binary is missing (e.g.
 * postinstall was skipped via ONEBRAIN_CLI_SKIP_POSTINSTALL or failed),
 * we print a helpful pointer and exit 127 (sysexits "command not found").
 */
'use strict';

const path = require('node:path');
const fs = require('node:fs');
const { spawnSync } = require('node:child_process');

const isWin = process.platform === 'win32';
const binaryName = isWin ? 'onebrain.exe' : 'onebrain';
const binaryPath = path.join(__dirname, binaryName);

if (!fs.existsSync(binaryPath)) {
  console.error(`[@onebrain-ai/cli] Binary not found at ${binaryPath}`);
  console.error('Re-run `npm install` (or `npm install -g @onebrain-ai/cli`) to download it,');
  console.error('or visit https://github.com/onebrain-ai/onebrain-cli/releases/latest');
  process.exit(127);
}

const child = spawnSync(binaryPath, process.argv.slice(2), { stdio: 'inherit' });
if (child.error) {
  console.error('[@onebrain-ai/cli] Failed to exec:', child.error.message);
  process.exit(1);
}
// Signal-terminated child: re-raise on this process so the parent shell sees
// the canonical `128 + signum` exit (Ctrl-C → SIGINT → 130, SIGTERM → 143).
// Without this, CI tooling can't distinguish a user interrupt from a real
// error — both would collapse to exit 1.
if (child.signal) {
  process.kill(process.pid, child.signal);
}
process.exit(child.status ?? 1);
