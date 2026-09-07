#!/usr/bin/env node
// Prepared-artifact release guard. Never loads a native module or publishes a package.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { closeSync, lstatSync, mkdirSync, openSync, readFileSync, readSync, readdirSync, realpathSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const NAME = '@anvailabs/sandhi';
const REPOSITORY = 'https://github.com/anvai-labs/sandhi';
const ROOT_FILES = ['index.js', 'sandhi.js', 'sandhi.d.ts', 'index.d.ts', 'contracts.d.ts', 'README.md'];
const PLATFORMS = [
  { id: 'linux-x64-gnu', os: 'linux', cpu: 'x64', libc: ['glibc'] },
  { id: 'darwin-arm64', os: 'darwin', cpu: 'arm64' },
];
const exact = (actual, expected, message) => assert.deepEqual(actual, expected, message);
const sorted = (values) => [...values].sort();
const deps = (version) => Object.fromEntries(PLATFORMS.map(({ id }) => [`${NAME}-${id}`, version]));

function regular(path, limit = 512 * 1024 * 1024) {
  const stat = lstatSync(path);
  assert.ok(stat.isFile() && stat.size > 0 && stat.size <= limit, `missing/unsafe/empty file: ${path}`);
  return stat;
}

function directory(path) {
  assert.ok(lstatSync(path).isDirectory(), `not a real directory: ${path}`);
  assert.equal(realpathSync(path), resolve(path), `symlink directory: ${path}`);
}

function manifest(path) {
  regular(path, 1024 * 1024);
  const value = JSON.parse(readFileSync(path, 'utf8'));
  assert.ok(value && typeof value === 'object' && !Array.isArray(value), 'invalid manifest');
  return value;
}

function binary(path, platform) {
  regular(path);
  const bytes = Buffer.alloc(64);
  const fd = openSync(path, 'r');
  try { assert.equal(readSync(fd, bytes, 0, bytes.length, 0), 64, 'truncated native header'); }
  finally { closeSync(fd); }
  if (platform.os === 'linux') {
    assert.ok(bytes.subarray(0, 4).equals(Buffer.from([0x7f, 0x45, 0x4c, 0x46]))
      && bytes[4] === 2 && bytes[5] === 1 && bytes.readUInt16LE(18) === 62,
    'expected Linux x64 little-endian ELF artifact');
  } else {
    assert.ok(bytes.readUInt32LE(0) === 0xfeedfacf && bytes.readUInt32LE(4) === 0x0100000c,
      'expected Darwin arm64 Mach-O artifact');
  }
}

function inspect(packageDir, version, prepared) {
  assert.ok(version.length <= 128 && version === version.trim(), 'invalid version whitespace/length');
  assert.match(version, /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/, 'stable X.Y.Z version required');
  assert.ok(version.split('.').every((part) => Number.isSafeInteger(Number(part))), 'version component too large');
  assert.notEqual(version, '0.0.0', 'development placeholder is not a release');
  directory(packageDir);
  const npmDir = join(packageDir, 'npm');
  directory(npmDir);
  exact(sorted(readdirSync(npmDir)), sorted(PLATFORMS.map(({ id }) => id)), 'wrong platform directory set');
  const root = manifest(join(packageDir, 'package.json'));
  assert.equal(root.name, NAME, 'wrong root package name');
  assert.equal(root.main, 'sandhi.js', 'wrong root loader');
  assert.equal(root.types, 'sandhi.d.ts', 'wrong root types');
  assert.ok(root.type === undefined || root.type === 'commonjs', 'root loader requires CommonJS');
  assert.equal(root.exports, undefined, 'unsupported exports override of the root loader');
  exact(sorted(root.files), sorted(ROOT_FILES), 'root files whitelist must be exact');
  for (const file of ROOT_FILES) regular(join(packageDir, file));
  const packages = PLATFORMS.map((platform) => {
    const path = join(npmDir, platform.id);
    directory(path);
    const data = manifest(join(path, 'package.json'));
    const native = `sandhi.${platform.id}.node`;
    assert.equal(data.name, `${NAME}-${platform.id}`, 'wrong platform package name');
    assert.equal(data.main, native, 'wrong platform loader');
    exact(data.os, [platform.os], 'wrong platform OS');
    exact(data.cpu, [platform.cpu], 'wrong platform CPU');
    exact(data.libc, platform.libc, 'wrong platform libc');
    exact(data.files, [native], 'platform files whitelist must be exact');
    exact(readdirSync(path).filter((file) => file.endsWith('.node')), [native], 'wrong platform binary set');
    binary(join(path, native), platform);
    regular(join(path, 'README.md'));
    return { path, data, files: [native, 'README.md', 'package.json'] };
  });
  packages.push({ path: packageDir, data: root, files: [...ROOT_FILES, 'package.json'] });
  for (const pkg of packages) {
    // OIDC/provenance identifies the workflow repository, independently for each package.
    // Accept npm's string shorthand and canonical git-object form, not arbitrary URLs.
    const repository = pkg.data.repository;
    assert.ok(repository === REPOSITORY || (repository && typeof repository === 'object'
      && repository.type === 'git' && repository.url === `git+${REPOSITORY}.git`),
    'package repository must identify the trusted publishing repository');
    assert.ok(!pkg.data.private, 'private package is not publishable');
    assert.ok(!pkg.data.bundleDependencies && !pkg.data.bundledDependencies, 'bundled dependencies prohibited');
    // Some npm releases still run prepare during pack with --ignore-scripts.
    // Reject publish/install hooks before invoking npm; build/test commands are fine.
    const hooks = ['prepare', 'prepack', 'postpack', 'prepublish', 'prepublishOnly',
      'publish', 'postpublish', 'preinstall', 'install', 'postinstall'];
    assert.ok(!hooks.some((hook) => Object.hasOwn(pkg.data.scripts || {}, hook)),
      'release/install lifecycle scripts prohibited');
    const publish = pkg.data.publishConfig || {};
    const allowedPublish = { access: 'public', registry: 'https://registry.npmjs.org', tag: 'latest' };
    assert.ok(Object.entries(publish).every(([key, value]) => allowedPublish[key] === value),
      'publishConfig override outside public npm/latest prohibited');
    if (prepared) assert.equal(pkg.data.version, version, 'package version mismatch');
    // npm automatically includes LICENSE even when files excludes it.
    if (readdirSync(pkg.path).includes('LICENSE')) {
      regular(join(pkg.path, 'LICENSE'));
      pkg.files.push('LICENSE');
    }
  }
  if (prepared) exact(root.optionalDependencies, deps(version), 'root platform dependencies must remain complete and exact');
  return packages;
}

export function validatePackReport(report, pkg, version) {
  assert.ok(Array.isArray(report) && report.length === 1, 'one npm pack result required');
  const item = report[0];
  assert.equal(item.name, pkg.data.name, 'packed name mismatch');
  assert.equal(item.version, version, 'packed version mismatch');
  assert.equal(item.filename, `${item.name.replace('@', '').replace('/', '-')}-${version}.tgz`, 'unsafe/unexpected tarball name');
  assert.ok(Array.isArray(item.files), 'packed file listing missing');
  const names = item.files.map((file) => {
    assert.ok(typeof file.path === 'string' && !file.path.includes('\\')
      && !file.path.startsWith('/') && !file.path.split('/').includes('..'), 'unsafe packed path');
    assert.ok(Number.isSafeInteger(file.size) && file.size > 0, 'empty/invalid packed file');
    return file.path;
  });
  exact(sorted(names), sorted(pkg.files), 'packed files differ from release allowlist');
  assert.ok(!item.bundled || item.bundled.length === 0, 'unexpected bundled package');
  return item;
}

function main(args) {
  const command = args.shift();
  assert.ok(['prepare', 'pack'].includes(command), 'usage: prepare|pack --package-dir DIR --version X.Y.Z [--output-dir NEW_DIR]');
  const options = {};
  while (args.length) {
    const key = args.shift();
    assert.ok(['--package-dir', '--version', '--output-dir'].includes(key) && !options[key] && args.length, 'invalid/duplicate option');
    options[key] = args.shift();
  }
  assert.ok(options['--package-dir'] && options['--version'], 'package-dir and version required');
  const packageDir = resolve(options['--package-dir']);
  const version = options['--version'];
  const packages = inspect(packageDir, version, command === 'pack');
  if (command === 'prepare') {
    assert.ok(!options['--output-dir'], 'prepare does not accept output-dir');
    // All artifacts/manifests are preflighted before changing generated release metadata.
    for (const pkg of packages) {
      pkg.data.version = version;
      if (pkg.path === packageDir) pkg.data.optionalDependencies = deps(version);
      writeFileSync(join(pkg.path, 'package.json'), JSON.stringify(pkg.data, null, 2) + '\n');
    }
    console.log(JSON.stringify({ prepared: true, version, packages: packages.map(({ data }) => data.name) }));
    return;
  }
  assert.ok(options['--output-dir'], 'pack requires output-dir');
  const output = resolve(options['--output-dir']);
  assert.ok(output !== packageDir && !output.startsWith(packageDir + '/'), 'pack output must be outside package tree');
  directory(dirname(output));
  mkdirSync(output); // Never overwrite/reuse an earlier release packet.
  const results = [];
  for (const pkg of packages) {
    const stdout = execFileSync('npm', ['pack', '--json', '--ignore-scripts', '--offline', '--pack-destination', output], {
      cwd: pkg.path, encoding: 'utf8', timeout: 60000, maxBuffer: 4 * 1024 * 1024,
      env: { ...process.env, npm_config_ignore_scripts: 'true' },
    });
    const item = validatePackReport(JSON.parse(stdout), pkg, version);
    const tarball = join(output, item.filename);
    regular(tarball);
    const bytes = readFileSync(tarball);
    assert.equal(createHash('sha1').update(bytes).digest('hex'), item.shasum, 'tarball digest mismatch');
    assert.equal('sha512-' + createHash('sha512').update(bytes).digest('base64'), item.integrity, 'tarball integrity mismatch');
    results.push({ name: item.name, version, tarball, files: item.files.map(({ path }) => path),
      sha256: createHash('sha256').update(bytes).digest('hex'), integrity: item.integrity });
  }
  console.log(JSON.stringify({ validated: true, version, packages: results }));
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try { main(process.argv.slice(2)); }
  catch (error) {
    console.error(`npm release guard: ${error.message}`);
    process.exitCode = 1;
  }
}
