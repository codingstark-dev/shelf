const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { downloadUrl, parseSha, verifySha } = require('./bin/shelf.js');
const { version } = require('./package.json');

test('download url from version, with override', () => {
  assert.strictEqual(
    downloadUrl('1.2.3', {}),
    'https://github.com/codingstark-dev/shelf/releases/download/v1.2.3/Shelf-macos-universal.zip',
  );
  assert.ok(downloadUrl(version, {}).includes(`/v${version}/`));
  assert.strictEqual(downloadUrl('1.2.3', { SHELF_DOWNLOAD_URL: 'file:///x.zip' }), 'file:///x.zip');
});

test('sha parsing and verification', async () => {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), 'shelf-test-'));
  const f = path.join(d, 'a');
  fs.writeFileSync(f, 'hello');
  const hex = crypto.createHash('sha256').update('hello').digest('hex');
  try {
    assert.strictEqual(parseSha(`${hex.toUpperCase()}  a\n`), hex);
    assert.strictEqual(parseSha('nope'), null);
    await verifySha(f, `${hex}  a\n`);
    await assert.rejects(verifySha(f, '0'.repeat(64)), /mismatch/);
    await assert.rejects(verifySha(f, 'garbage'), /not a SHA-256/);
  } finally {
    fs.rmSync(d, { recursive: true, force: true });
  }
});
