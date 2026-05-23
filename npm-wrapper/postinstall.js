#!/usr/bin/env node
/**
 * @onebrain-ai/cli postinstall — downloads the matching platform binary
 * from the OneBrain CLI GitHub Release tagged v${pkg.version}, extracts it
 * to ./bin/, verifies SHA256, smoke-runs the binary, and chmods the result.
 *
 * No npm-side caching of binaries — every install pulls fresh from GitHub
 * (matches the rustup / esbuild / swc pattern). For a faster install path,
 * see the optionalDependencies-per-platform layout we may switch to in
 * v3.0.x.
 *
 * Knobs:
 *   ONEBRAIN_CLI_SKIP_POSTINSTALL=1  — skip the download entirely (for CI
 *                                       environments that supply their own
 *                                       binary).
 *   ONEBRAIN_CLI_LIBC=glibc|musl     — override Linux libc auto-detection.
 *   ONEBRAIN_CLI_ARM=v6|v7           — override 32-bit ARM version detect.
 *   ONEBRAIN_CLI_DEBUG=1             — verbose logging (cleanup, libc probe).
 *
 * Telemetry: every fetch sends `User-Agent: onebrain-cli-postinstall/$VERSION`
 * to GitHub. This is the only telemetry the postinstall emits; opt out by
 * either setting ONEBRAIN_CLI_SKIP_POSTINSTALL=1 or downloading the binary
 * manually from the GitHub Release.
 */
'use strict';

const fs = require('node:fs');
const path = require('node:path');
const https = require('node:https');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');

if (process.env.ONEBRAIN_CLI_SKIP_POSTINSTALL) {
  console.log('[@onebrain-ai/cli] ONEBRAIN_CLI_SKIP_POSTINSTALL set — skipping binary download.');
  process.exit(0);
}

const pkg = require('./package.json');
const VERSION = pkg.version;
const DEBUG = !!process.env.ONEBRAIN_CLI_DEBUG;

// The fixed-target host map — Linux + arm32 + musl rows live in resolveTriple
// because they need runtime detection. This table is the "no detection needed"
// fast path.
const TRIPLE_MAP = {
  'darwin-arm64': 'aarch64-apple-darwin',
  'darwin-x64':   'x86_64-apple-darwin',
  'linux-arm64':  'aarch64-unknown-linux-gnu',
  'linux-x64':    'x86_64-unknown-linux-gnu',
  'win32-arm64':  'aarch64-pc-windows-msvc',
  'win32-x64':    'x86_64-pc-windows-msvc',
};

const triple = resolveTriple();
const isWin = process.platform === 'win32';
const archiveExt = isWin ? 'zip' : 'tar.gz';
const archiveName = `onebrain-${triple}.${archiveExt}`;
const url = `https://github.com/onebrain-ai/onebrain-cli/releases/download/v${VERSION}/${archiveName}`;

const root = __dirname;
const binDir = path.join(root, 'bin');
fs.mkdirSync(binDir, { recursive: true });
const archivePath = path.join(root, archiveName);

console.log(`[@onebrain-ai/cli] Downloading v${VERSION} for ${triple} ...`);
console.log(`  ${url}`);

downloadFile(url, archivePath).then(async () => {
  try {
    // Supply-chain integrity: verify the downloaded archive against the
    // `.sha256` published alongside it in the same GitHub Release. The
    // release workflow generates both files in the same job, so a mismatch
    // indicates either archive corruption or tampering between GitHub's
    // S3 CDN and this host. This closes the gap between the OIDC-attested
    // wrapper publish and the binary the postinstall actually executes.
    console.log('[@onebrain-ai/cli] Verifying SHA256 ...');
    const sha256Body = await downloadText(`${url}.sha256`);
    const expectedHash = sha256Body.trim().split(/\s+/)[0].toLowerCase();
    const actualHash = crypto.createHash('sha256')
      .update(fs.readFileSync(archivePath))
      .digest('hex');
    if (!expectedHash || expectedHash !== actualHash) {
      throw new Error(`SHA256 mismatch — expected ${expectedHash || '<empty>'}, got ${actualHash}. Archive may be corrupt or tampered.`);
    }

    console.log('[@onebrain-ai/cli] Extracting ...');
    extractArchive(archivePath, binDir);
  } finally {
    // Always remove the archive — failed verification or extraction
    // shouldn't leave half-staged files lingering in node_modules/.
    if (fs.existsSync(archivePath)) {
      try { fs.unlinkSync(archivePath); }
      catch (err) {
        if (DEBUG) console.warn(`[@onebrain-ai/cli] archive cleanup failed: ${err.message}`);
      }
    }
  }

  const binaryName = isWin ? 'onebrain.exe' : 'onebrain';
  const binaryPath = path.join(binDir, binaryName);
  if (!fs.existsSync(binaryPath)) {
    console.error(`[@onebrain-ai/cli] Extraction succeeded but ${binaryName} not found at ${binDir}`);
    process.exit(1);
  }
  if (!isWin) {
    fs.chmodSync(binaryPath, 0o755);
  }

  // Smoke-run the binary so a wrong-libc / wrong-arch download fails at
  // install time with an actionable error instead of segfaulting at first
  // user invocation. ~30-80 ms cost, catches every silent install bug from
  // the detector layer above.
  try {
    const out = execFileSync(binaryPath, ['--version'], {
      stdio: ['ignore', 'pipe', 'pipe'],
      timeout: 10_000,
    }).toString().trim();
    console.log(`[@onebrain-ai/cli] Installed ${out} → ${binaryPath}`);
  } catch (err) {
    console.error(`[@onebrain-ai/cli] Binary installed but failed to run: ${err.message}`);
    console.error('This usually means the wrong libc/arch variant was selected for your host.');
    console.error('Override with one of:');
    console.error('  ONEBRAIN_CLI_LIBC=musl npm install @onebrain-ai/cli');
    console.error('  ONEBRAIN_CLI_LIBC=glibc npm install @onebrain-ai/cli');
    console.error('  ONEBRAIN_CLI_ARM=v6 npm install @onebrain-ai/cli   # for older ARM (Pi 1 / Pi Zero)');
    console.error('  ONEBRAIN_CLI_ARM=v7 npm install @onebrain-ai/cli   # for 32-bit Pi 2/3/4');
    process.exit(1);
  }
}).catch((err) => {
  console.error('[@onebrain-ai/cli] Install failed:', err.message);
  console.error('Possible causes: package.json version mismatches a published release, release was yanked,');
  console.error('CDN propagation lag, or this platform variant is not yet built.');
  console.error('Manual download: https://github.com/onebrain-ai/onebrain-cli/releases/latest');
  process.exit(1);
});

// resolveTriple picks the Rust target triple for the current host. On Linux
// the choice between glibc/musl and ARMv6/ARMv7 is dynamic — Alpine and
// other musl-based distros need the musl-linked binary or they'll fail at
// runtime when the dynamic loader can't resolve glibc symbols. Raspberry Pi
// devices span ARMv6 (Pi 1, Pi Zero) through ARMv8 (Pi 5) and the wrong
// binary segfaults with an illegal-instruction trap.
function resolveTriple() {
  if (process.platform === 'linux') {
    if (process.arch === 'arm') {
      return resolveArmTriple();
    }
    const libc = resolveLinuxLibc();
    if (libc === 'musl') {
      if (process.arch === 'x64') return 'x86_64-unknown-linux-musl';
      console.error(`[@onebrain-ai/cli] Unsupported platform: linux-${process.arch}-musl`);
      console.error('Only x86_64 musl is published. Build from source or use a glibc-based distro:');
      console.error('  https://github.com/onebrain-ai/onebrain-cli');
      process.exit(1);
    }
  }

  const key = `${process.platform}-${process.arch}`;
  const triple = TRIPLE_MAP[key];
  if (!triple) {
    console.error(`[@onebrain-ai/cli] Unsupported platform: ${key}`);
    console.error('Supported: ' + Object.keys(TRIPLE_MAP).join(', '));
    console.error('Manual download: https://github.com/onebrain-ai/onebrain-cli/releases/latest');
    process.exit(1);
  }
  return triple;
}

// resolveLinuxLibc returns 'musl' or 'glibc'. Override via ONEBRAIN_CLI_LIBC.
// Detection layers (positive-verification — never silently fall through):
//   1. Env override — always wins.
//   2. /etc/alpine-release — definitive Alpine signal.
//   3. process.report.header.glibcVersionRuntime — empty string on musl,
//      a version like "2.39" on glibc (stable in Node 14+).
// On a fully unknown Node build (process.report missing, no /etc/alpine-release)
// we WARN and default to 'glibc' — the user can override via env if the guess
// is wrong instead of the postinstall installing the wrong binary silently.
function resolveLinuxLibc() {
  const override = process.env.ONEBRAIN_CLI_LIBC;
  if (override === 'musl' || override === 'glibc') {
    if (DEBUG) console.log(`[@onebrain-ai/cli] libc override: ${override}`);
    return override;
  }

  try {
    if (fs.existsSync('/etc/alpine-release')) return 'musl';
  } catch (err) {
    if (DEBUG) console.warn(`[@onebrain-ai/cli] libc probe (alpine-release): ${err.message}`);
  }

  try {
    if (typeof process.report?.getReport === 'function') {
      const header = process.report.getReport()?.header;
      if (header && typeof header.glibcVersionRuntime === 'string') {
        return header.glibcVersionRuntime === '' ? 'musl' : 'glibc';
      }
      // Report exists but the field shape is unfamiliar — likely a future
      // Node version restructured it. Don't guess; warn and let the smoke
      // test catch a wrong choice.
      console.warn('[@onebrain-ai/cli] libc detector: process.report shape unrecognised, defaulting to glibc.');
      console.warn('  Override with ONEBRAIN_CLI_LIBC=musl if this is an Alpine/musl host.');
      return 'glibc';
    }
  } catch (err) {
    if (DEBUG) console.warn(`[@onebrain-ai/cli] libc probe (process.report): ${err.message}`);
  }

  console.warn('[@onebrain-ai/cli] libc detector: no probe succeeded, defaulting to glibc.');
  console.warn('  Override with ONEBRAIN_CLI_LIBC=musl if this is an Alpine/musl host.');
  return 'glibc';
}

// resolveArmTriple picks between armv6 (Pi 1, Pi Zero) and armv7
// (Pi 2/3/4 in 32-bit OS, Pi Zero 2 W in 32-bit OS) for Linux 32-bit ARM.
// Override via ONEBRAIN_CLI_ARM=v6 or v7.
//
// Conservative default = ARMv6 because an ARMv6 binary runs on ARMv7 hosts
// but an ARMv7 binary crashes with SIGILL on ARMv6 hosts. We sacrifice some
// performance on Pi 4 32-bit for correctness on Pi Zero.
function resolveArmTriple() {
  const override = process.env.ONEBRAIN_CLI_ARM;
  if (override === 'v7') return 'armv7-unknown-linux-gnueabihf';
  if (override === 'v6') return 'arm-unknown-linux-gnueabihf';

  // process.config.variables.arm_version is set at Node build time:
  // '6' on Node's armv6l build, '7' on armv7l, missing on builds without
  // ARM-specific flags.
  const armVersion = process.config?.variables?.arm_version;
  if (armVersion === '7') return 'armv7-unknown-linux-gnueabihf';
  if (armVersion === '6') return 'arm-unknown-linux-gnueabihf';

  // Fall back to /proc/cpuinfo — "CPU architecture: 7" / "CPU architecture: 6"
  // or model strings like "ARMv7 Processor".
  try {
    const cpuinfo = fs.readFileSync('/proc/cpuinfo', 'utf8');
    if (/CPU architecture:\s*7/m.test(cpuinfo) || /ARMv7/.test(cpuinfo)) {
      return 'armv7-unknown-linux-gnueabihf';
    }
    if (/CPU architecture:\s*[56]/m.test(cpuinfo) || /ARMv6/.test(cpuinfo)) {
      return 'arm-unknown-linux-gnueabihf';
    }
  } catch (err) {
    if (DEBUG) console.warn(`[@onebrain-ai/cli] arm probe (cpuinfo): ${err.message}`);
  }

  console.warn('[@onebrain-ai/cli] ARM version detection inconclusive — defaulting to ARMv6 (compatible everywhere, slower on Pi 4).');
  console.warn('  Override with ONEBRAIN_CLI_ARM=v7 to force the ARMv7 binary.');
  return 'arm-unknown-linux-gnueabihf';
}

// extractArchive unpacks .tar.gz on Unix and .zip on Windows. The Windows
// path tries bsdtar first (ships on Windows 10 1803+) and falls back to
// PowerShell Expand-Archive for older hosts. PowerShell paths use
// -LiteralPath to avoid wildcard expansion on '[' or ']' and are escaped
// for single-quote literals — '' is PowerShell's only single-quote escape
// inside a '...'-string, so `escapePsSingleQuoted` closes off injection.
function extractArchive(archive, destDir) {
  if (!isWin) {
    execFileSync('tar', ['-xzf', archive, '-C', destDir], { stdio: 'inherit' });
    return;
  }

  try {
    execFileSync('tar', ['-xf', archive, '-C', destDir], { stdio: 'inherit' });
  } catch (err) {
    // ANY tar failure on Windows falls back to PowerShell — the archive
    // already passed SHA256, so a tar error means either tar.exe is missing
    // (pre-1803 Windows 10, ENOENT) or PATH points at GNU tar from MSYS2 /
    // Git-for-Windows / Cygwin (which doesn't grok .zip and exits non-zero).
    // Expand-Archive ships with PowerShell 5.1 on every Windows 10+ host.
    const reason = err.code || err.message;
    console.log(`[@onebrain-ai/cli] tar failed (${reason}), falling back to PowerShell Expand-Archive ...`);
    const escapedArchive = escapePsSingleQuoted(archive);
    const escapedDest = escapePsSingleQuoted(destDir);
    const command = `$ErrorActionPreference='Stop'; Expand-Archive -LiteralPath '${escapedArchive}' -DestinationPath '${escapedDest}' -Force`;
    execFileSync(
      'powershell.exe',
      ['-NoProfile', '-NonInteractive', '-Command', command],
      { stdio: 'inherit' }
    );
  }
}

function escapePsSingleQuoted(s) {
  return s.replace(/'/g, "''");
}

// downloadFile streams a binary to disk. Follows GitHub's redirect chain
// (S3 CDN) and retries on 404 with exponential backoff — after `npm publish`
// the wrapper can be live before the release CDN has fanned out, so a fresh
// `npm install` can race the binary. Three retries at 2s/4s/8s = 14s total
// covers the worst observed lag. The same 404 also fires for permanent
// failures (wrong version, yanked release, missing platform variant) — the
// final reject path elaborates on the possible causes.
function downloadFile(url, dest, redirects = 5, retries = 3, backoffMs = 2000) {
  return new Promise((resolve, reject) => {
    https.get(url, { headers: { 'User-Agent': `onebrain-cli-postinstall/${VERSION}` } }, (res) => {
      if ([301, 302, 303, 307, 308].includes(res.statusCode)) {
        if (redirects <= 0) {
          reject(new Error('Too many redirects'));
          return;
        }
        res.resume();
        downloadFile(res.headers.location, dest, redirects - 1, retries, backoffMs).then(resolve, reject);
        return;
      }
      if (res.statusCode === 404 && retries > 0) {
        res.resume();
        console.log(`[@onebrain-ai/cli]   binary not yet on CDN, retrying in ${backoffMs}ms ...`);
        setTimeout(() => {
          downloadFile(url, dest, redirects, retries - 1, backoffMs * 2).then(resolve, reject);
        }, backoffMs);
        return;
      }
      if (res.statusCode !== 200) {
        reject(new Error(`HTTP ${res.statusCode} fetching ${url}`));
        return;
      }
      const file = fs.createWriteStream(dest);
      res.pipe(file);
      file.on('finish', () => file.close(() => resolve()));
      file.on('error', reject);
    }).on('error', reject);
  });
}

// downloadText follows the same redirect + retry chain as downloadFile but
// buffers the body into a UTF-8 string. Used for small metadata files
// (currently the `.sha256` checksum next to each release archive). Don't
// use it for binaries — the body lives in memory for the duration of the
// request. Inherits the same 404 retry policy since the .sha256 is
// published in the same Release as the archive and CDN-lags together.
function downloadText(url, redirects = 5, retries = 3, backoffMs = 2000) {
  return new Promise((resolve, reject) => {
    https.get(url, { headers: { 'User-Agent': `onebrain-cli-postinstall/${VERSION}` } }, (res) => {
      if ([301, 302, 303, 307, 308].includes(res.statusCode)) {
        if (redirects <= 0) {
          reject(new Error('Too many redirects'));
          return;
        }
        res.resume();
        downloadText(res.headers.location, redirects - 1, retries, backoffMs).then(resolve, reject);
        return;
      }
      if (res.statusCode === 404 && retries > 0) {
        res.resume();
        console.log(`[@onebrain-ai/cli]   sha256 not yet on CDN, retrying in ${backoffMs}ms ...`);
        setTimeout(() => {
          downloadText(url, redirects, retries - 1, backoffMs * 2).then(resolve, reject);
        }, backoffMs);
        return;
      }
      if (res.statusCode !== 200) {
        reject(new Error(`HTTP ${res.statusCode} fetching ${url}`));
        return;
      }
      let body = '';
      res.setEncoding('utf8');
      res.on('data', (chunk) => { body += chunk; });
      res.on('end', () => resolve(body));
      res.on('error', reject);
    }).on('error', reject);
  });
}
