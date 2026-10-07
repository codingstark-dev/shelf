#!/usr/bin/env node
// Downloads Shelf.app for this package's version (once), verifies it, and opens it.
'use strict';
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const https = require('node:https');
const crypto = require('node:crypto');
const { spawnSync } = require('node:child_process');

const { version } = require('../package.json');
const ASSET = 'Shelf-macos-universal.zip';

const downloadUrl = (v = version, env = process.env) =>
  env.SHELF_DOWNLOAD_URL ||
  `https://github.com/codingstark-dev/shelf/releases/download/v${v}/${ASSET}`;

const cacheRoot = (env = process.env) =>
  env.SHELF_CACHE_DIR || path.join(os.homedir(), 'Library', 'Caches', 'shelf-drive');

// First hex token of a `.sha256` file (accepts "<hex>" or "<hex>  filename").
const parseSha = (text) => {
  const m = /^[0-9a-f]{64}\b/i.exec(String(text).trim());
  return m ? m[0].toLowerCase() : null;
};

const sha256File = (file) =>
  new Promise((resolve, reject) => {
    const h = crypto.createHash('sha256');
    fs.createReadStream(file).on('data', (d) => h.update(d)).on('error', reject).on('end', () => resolve(h.digest('hex')));
  });

// Throws unless `file` hashes to `expected`.
async function verifySha(file, expected) {
  const want = parseSha(expected);
  if (!want) throw new Error('checksum file is not a SHA-256 hex digest');
  const got = await sha256File(file);
  if (got !== want) throw new Error(`SHA-256 mismatch (expected ${want}, got ${got})`);
}

// Fetch `url` into `dest`; follows redirects, supports file://, prints progress to stderr.
function fetchTo(url, dest, hops = 0) {
  return new Promise((resolve, reject) => {
    if (url.startsWith('file://')) {
      return fs.copyFile(new URL(url), dest, (e) => (e ? reject(e) : resolve()));
    }
    if (hops > 8) return reject(new Error('too many redirects'));
    https.get(url, { headers: { 'user-agent': 'shelf-drive' } }, (res) => {
      const { statusCode: code, headers } = res;
      if (code >= 300 && code < 400 && headers.location) {
        res.resume();
        return fetchTo(new URL(headers.location, url).href, dest, hops + 1).then(resolve, reject);
      }
      if (code !== 200) {
        res.resume();
        return reject(new Error(`HTTP ${code} for ${url}`));
      }
      const total = Number(headers['content-length']) || 0;
      let got = 0;
      res.on('data', (c) => {
        got += c.length;
        if (total && process.stderr.isTTY) process.stderr.write(`\rDownloading Shelf... ${Math.floor((got / total) * 100)}%`);
      });
      const out = fs.createWriteStream(dest);
      res.pipe(out);
      out.on('finish', () => {
        if (total && process.stderr.isTTY) process.stderr.write('\n');
        resolve();
      });
      out.on('error', reject);
      res.on('error', reject);
    }).on('error', reject);
  });
}

const sh = (cmd, args, opts) => spawnSync(cmd, args, { stdio: 'inherit', ...opts });

async function install(dir) {
  const url = downloadUrl();
  fs.mkdirSync(cacheRoot(), { recursive: true });
  const tmp = fs.mkdtempSync(path.join(cacheRoot(), '.tmp-'));
  try {
    const zip = path.join(tmp, ASSET);
    console.error(`Downloading ${url}`);
    await fetchTo(url, zip);

    const shaFile = path.join(tmp, `${ASSET}.sha256`);
    try {
      await fetchTo(`${url}.sha256`, shaFile);
      await verifySha(zip, fs.readFileSync(shaFile, 'utf8'));
      console.error('SHA-256 verified.');
    } catch (e) {
      if (!fs.existsSync(shaFile)) console.error(`warning: no .sha256 available, skipping verification (${e.message})`);
      else throw e;
    }

    const out = path.join(tmp, 'out');
    if (sh('/usr/bin/ditto', ['-x', '-k', zip, out]).status !== 0) throw new Error('unzip failed');
    const app = path.join(out, 'Shelf.app');
    if (!fs.existsSync(app)) throw new Error('archive does not contain Shelf.app');
    // Unsigned app: best-effort, ignore failure.
    spawnSync('/usr/bin/xattr', ['-dr', 'com.apple.quarantine', app], { stdio: 'ignore' });

    // Only now (new copy downloaded and verified) is the old one replaced, so a
    // failed --update leaves the working install, and any LaunchAgent pointing at it, intact.
    fs.mkdirSync(dir, { recursive: true });
    const dest = path.join(dir, 'Shelf.app');
    const old = path.join(tmp, 'old');
    if (fs.existsSync(dest)) fs.renameSync(dest, old);
    try {
      fs.renameSync(app, dest);
    } catch (e) {
      if (fs.existsSync(old)) fs.renameSync(old, dest);
      else if (!fs.existsSync(dest)) throw e; // else lost a race to another run: fine
    }
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

const HELP = `shelf-drive ${version}
Usage: npx shelf-drive [--update | --uninstall | --version | --help]
  (none)       download Shelf.app on first run, then open it
  --update     re-download this version
  --uninstall  delete the cached app (${cacheRoot()}) and the start-at-login item
`;

async function main(argv = process.argv.slice(2)) {
  const flag = argv[0];
  if (flag === '--version' || flag === '-v') return console.log(version);
  if (flag === '--help' || flag === '-h') return console.log(HELP);
  if (process.platform !== 'darwin') {
    console.error('Shelf only runs on macOS.');
    process.exit(1);
  }
  if (flag === '--uninstall') {
    // Best effort: remove the "start at login" LaunchAgent that points into the cache.
    const label = 'dev.shelf.app';
    spawnSync('/bin/launchctl', ['bootout', `gui/${process.getuid()}/${label}`], { stdio: 'ignore' });
    fs.rmSync(path.join(os.homedir(), 'Library', 'LaunchAgents', `${label}.plist`), { force: true });
    fs.rmSync(cacheRoot(), { recursive: true, force: true });
    return console.log('Removed Shelf cache and login item.');
  }
  if (flag && flag !== '--update') {
    console.error(`Unknown option ${flag}\n\n${HELP}`);
    process.exit(2);
  }
  const dir = path.join(cacheRoot(), version);
  const app = path.join(dir, 'Shelf.app');
  if (flag === '--update' || !fs.existsSync(app)) await install(dir);
  if (sh('/usr/bin/open', [app]).status !== 0) {
    console.error(`Could not open ${app}`);
    process.exit(1);
  }
}

module.exports = { downloadUrl, cacheRoot, parseSha, verifySha };

if (require.main === module) {
  main().catch((e) => {
    console.error(`shelf-drive: ${e.message}`);
    process.exit(1);
  });
}
