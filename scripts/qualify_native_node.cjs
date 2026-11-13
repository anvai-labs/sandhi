// Nonpublishing qualification of the advertised macOS ARM64 release addon.
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const { execFileSync, spawnSync } = require('node:child_process');

assert.equal(process.platform, 'darwin', 'qualification requires native macOS');
assert.equal(process.arch, 'arm64', 'qualification requires a native ARM64 Node runtime');
const compiler = execFileSync('rustc', ['-vV'], { encoding: 'utf8' });
assert.deepEqual(compiler.split('\n').filter(line => line.startsWith('host: ')), ['host: aarch64-apple-darwin']);
if (process.argv.includes('--check-host')) {
  console.log(JSON.stringify({ platform: process.platform, arch: process.arch, node: process.version, rustc: compiler.trim() }));
} else {
  const root = path.resolve(__dirname, '..');
  const binding = path.join(root, 'bindings/node');
  const expected = fs.realpathSync(path.join(binding, 'sandhi.darwin-arm64.node'));
  const loaded = [];
  const original = process.dlopen;
  process.dlopen = function(module, filename, ...args) {
    const actual = fs.realpathSync(filename);
    assert.equal(actual, expected, 'unexpected native addon shadows the built artifact');
    loaded.push(actual);
    return original.call(this, module, filename, ...args);
  };
  try {
    require(path.join(binding, 'sandhi.js'));
  } finally {
    process.dlopen = original;
  }
  assert.deepEqual(loaded, [expected], 'built native artifact must actually load');
  const hash = () => crypto.createHash('sha256').update(fs.readFileSync(expected)).digest('hex');
  const report = { schema_version: 1, platform: process.platform, arch: process.arch, node: process.version, rustc: compiler.trim(), addon: expected, addon_sha256: hash(), result: 'loaded' };
  const reportFile = path.join(root, 'native-node-report.json');
  fs.writeFileSync(reportFile, JSON.stringify(report, null, 2) + '\n');
  // Exact package test entry, all tests; no name filtering or skipped native suite.
  const child = spawnSync(process.execPath, ['--test', '__test__/gateway.test.mjs'], { cwd: binding, stdio: 'inherit', env: { ...process.env, NODE_PATH: '', NODE_OPTIONS: '' } });
  assert.equal(child.error, undefined, 'binding test process failed to start');
  assert.equal(child.signal, null, 'binding test process was signalled');
  assert.equal(child.status, 0, 'binding tests failed');
  assert.equal(hash(), report.addon_sha256, 'native artifact changed during qualification');
  report.result = 'passed';
  fs.writeFileSync(reportFile, JSON.stringify(report, null, 2) + '\n');
}
