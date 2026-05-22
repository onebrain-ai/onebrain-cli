#!/usr/bin/env node
/**
 * @onebrain-ai/cli postinstall — downloads the matching platform binary
 * from the OneBrain CLI GitHub Release tagged v${pkg.version}, extracts it
 * to ./bin/, and chmods the result.
 *
 * No npm-side caching of binaries — every install pulls fresh from GitHub
 * (matches the rustup / esbuild / swc pattern). For a faster install path,
 * see the optionalDependencies-per-platform layout we may switch to in
 * v3.0.x.
 *
 * Bypass: set ONEBRAIN_CLI_SKIP_POSTINSTALL=1 to skip the download (useful
 * for CI environments that supply their own binary).
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

const TRIPLE_MAP = {
  'darwin-arm64': 'aarch64-apple-darwin',
  'darwin-x64':   'x86_64-apple-darwin',
  'linux-arm64':  'aarch64-unknown-linux-gnu',
  'linux-x64':    'x86_64-unknown-linux-gnu',
  'win32-arm64':  'aarch64-pc-windows-msvc',
  'win32-x64':    'x86_64-pc-windows-msvc',
};

const key = `${process.platform}-${process.arch}`;
const triple = TRIPLE_MAP[key];
if (!triple) {
  console.error(`[@onebrain-ai/cli] Unsupported platform: ${key}`);
  console.error('Supported: ' + Object.keys(TRIPLE_MAP).join(', '));
  console.error('Manual download: https://github.com/onebrain-ai/onebrain-cli/releases/latest');
  process.exit(1);
}

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
    // `tar` ships on macOS, Linux, and Windows 10 1803+ (bsdtar). Spawn it
    // via execFileSync — argv form so the path is never shell-interpolated.
    if (isWin) {
      // Windows zip — bsdtar auto-detects format.
      execFileSync('tar', ['-xf', archivePath, '-C', binDir], { stdio: 'inherit' });
    } else {
      execFileSync('tar', ['-xzf', archivePath, '-C', binDir], { stdio: 'inherit' });
    }
  } finally {
    // Always remove the archive — failed verification or extraction
    // shouldn't leave half-staged files lingering in node_modules/.
    if (fs.existsSync(archivePath)) {
      try { fs.unlinkSync(archivePath); } catch { /* best effort */ }
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
  console.log(`[@onebrain-ai/cli] Installed onebrain v${VERSION} → ${binaryPath}`);
}).catch((err) => {
  console.error('[@onebrain-ai/cli] Install failed:', err.message);
  console.error('Manual download: https://github.com/onebrain-ai/onebrain-cli/releases/latest');
  process.exit(1);
});

function downloadFile(url, dest, redirects = 5) {
  return new Promise((resolve, reject) => {
    https.get(url, { headers: { 'User-Agent': `onebrain-cli-postinstall/${VERSION}` } }, (res) => {
      // GitHub Release downloads always redirect to S3 — follow.
      if ([301, 302, 303, 307, 308].includes(res.statusCode)) {
        if (redirects <= 0) {
          reject(new Error('Too many redirects'));
          return;
        }
        res.resume();
        downloadFile(res.headers.location, dest, redirects - 1).then(resolve, reject);
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

// downloadText follows the same redirect chain as downloadFile but buffers
// the body into a UTF-8 string. Used for small metadata files (currently
// the `.sha256` checksum next to each release archive). Don't use it for
// binaries — the body lives in memory for the duration of the request.
function downloadText(url, redirects = 5) {
  return new Promise((resolve, reject) => {
    https.get(url, { headers: { 'User-Agent': `onebrain-cli-postinstall/${VERSION}` } }, (res) => {
      if ([301, 302, 303, 307, 308].includes(res.statusCode)) {
        if (redirects <= 0) {
          reject(new Error('Too many redirects'));
          return;
        }
        res.resume();
        downloadText(res.headers.location, redirects - 1).then(resolve, reject);
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
