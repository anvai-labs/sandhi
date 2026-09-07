"""Offline release metadata/pack checks; no native code loading or registry publication."""
import json
import os
from pathlib import Path
import shutil
import subprocess

import pytest

ROOT = Path(__file__).resolve().parents[2]
HELPER = ROOT / 'scripts/check-npm-release.mjs'
VERSION = '1.2.3'
PLATFORMS = [('linux-x64-gnu', 'linux', 'x64'), ('darwin-arm64', 'darwin', 'arm64')]
FILES = ['index.js', 'sandhi.js', 'sandhi.d.ts', 'index.d.ts', 'contracts.d.ts', 'README.md']


def write_json(path, data):
    path.write_text(json.dumps(data))


def change(path, **fields):
    data = json.loads(path.read_text())
    data.update(fields)
    write_json(path, data)


@pytest.fixture
def package(tmp_path):
    root = tmp_path / 'npm-package'
    root.mkdir()
    manifest = {'name': '@anvailabs/sandhi', 'version': '0.0.0', 'main': 'sandhi.js',
                'types': 'sandhi.d.ts', 'files': FILES}
    write_json(root / 'package.json', manifest)
    for name in FILES:
        (root / name).write_text('// disposable loader/types/readme\n')
    for platform, operating_system, cpu in PLATFORMS:
        target = root / 'npm' / platform
        target.mkdir(parents=True)
        native = f'sandhi.{platform}.node'
        data = {'name': f'@anvailabs/sandhi-{platform}', 'version': '0.0.0', 'main': native,
                'os': [operating_system], 'cpu': [cpu], 'files': [native]}
        if operating_system == 'linux':
            data['libc'] = ['glibc']
        write_json(target / 'package.json', data)
        (target / 'README.md').write_text('Disposable platform fixture\n')
        header = bytearray(64)
        if operating_system == 'linux':
            header[:6] = b'\x7fELF\x02\x01'
            header[18:20] = (62).to_bytes(2, 'little')
        else:
            header[:4] = (0xfeedfacf).to_bytes(4, 'little')
            header[4:8] = (0x0100000c).to_bytes(4, 'little')
        (target / native).write_bytes(header)
    return root


def run(package, command='prepare', version=VERSION, output=None):
    env = {key: value for key, value in os.environ.items() if not key.lower().startswith('npm_config_')}
    env.update(npm_config_cache=str(package.parent / 'npm-cache'), npm_config_userconfig=os.devnull)
    args = ['node', str(HELPER), command, '--package-dir', str(package), '--version', version]
    if command == 'pack':
        args += ['--output-dir', str(output or package.parent / 'npm-packed')]
    return subprocess.run(args, env=env, capture_output=True, text=True, timeout=90)


def prepared(package):
    result = run(package)
    assert result.returncode == 0, result.stderr


def test_prepare_preserves_complete_dependency_set_on_repair(package):
    prepared(package)
    expected = {f'@anvailabs/sandhi-{platform}': VERSION for platform, _, _ in PLATFORMS}
    assert json.loads((package / 'package.json').read_text())['optionalDependencies'] == expected
    before = (package / 'package.json').read_bytes()
    prepared(package)
    assert (package / 'package.json').read_bytes() == before
    assert len(list((package / 'npm').iterdir())) == 2


@pytest.mark.parametrize('version', ['0.0.0', '01.2.3', '1.2', '1.2.3-beta.1', '1.2.3+build', 'v1.2.3', '../1.2.3',
                                   '1.2.3\n', '99999999999999999999999.1.2'])
def test_unstable_or_unsafe_version_fails_before_mutation(package, version):
    before = (package / 'package.json').read_bytes()
    assert run(package, version=version).returncode != 0
    assert (package / 'package.json').read_bytes() == before


@pytest.mark.parametrize('file', FILES)
def test_missing_root_loader_type_or_doc_rejected(package, file):
    (package / file).unlink()
    assert run(package).returncode != 0


@pytest.mark.parametrize('fault', ['missing_platform', 'extra_platform', 'missing_binary', 'extra_binary',
                                  'wrong_arch', 'empty_binary', 'symlink_binary', 'symlink_manifest'])
def test_artifact_topology_and_binary_fail_closed(package, fault):
    platform = package / 'npm/linux-x64-gnu'
    native = platform / 'sandhi.linux-x64-gnu.node'
    if fault == 'missing_platform':
        shutil.rmtree(platform)
    elif fault == 'extra_platform':
        (package / 'npm/win32-x64-msvc').mkdir()
    elif fault == 'missing_binary':
        native.unlink()
    elif fault == 'extra_binary':
        (platform / 'unexpected.node').write_bytes(b'x')
    elif fault == 'wrong_arch':
        shutil.copyfile(package / 'npm/darwin-arm64/sandhi.darwin-arm64.node', native)
    elif fault == 'empty_binary':
        native.write_bytes(b'')
    elif fault == 'symlink_binary':
        native.unlink()
        native.symlink_to(package / 'index.js')
    else:
        source = platform / 'package.json'
        held = package.parent / 'manifest-original.json'
        source.rename(held)
        source.symlink_to(held)
    before = (package / 'package.json').read_bytes()
    assert run(package).returncode != 0
    assert (package / 'package.json').read_bytes() == before


@pytest.mark.parametrize('field,value', [('optionalDependencies', {}), ('optionalDependencies', {
    '@anvailabs/sandhi-linux-x64-gnu': '^1.2.3', '@anvailabs/sandhi-darwin-arm64': VERSION}),
    ('files', FILES + ['src/**']), ('main', 'src/lib.rs'), ('version', '2.0.0')])
def test_pack_rechecks_prepared_root_contract(package, field, value):
    prepared(package)
    change(package / 'package.json', **{field: value})
    assert run(package, 'pack').returncode != 0
    assert not (package.parent / 'npm-packed').exists()


def test_pack_exact_files(package):
    (package / 'src').mkdir()
    (package / 'src/private.rs').write_text('private source\n')
    (package / 'stray.node').write_bytes(b'not shipped')
    prepared(package)
    result = run(package, 'pack')
    assert result.returncode == 0, result.stderr
    packet = json.loads(result.stdout)
    assert packet['validated'] is True
    assert len(packet['packages']) == 3
    assert packet['packages'][-1]['files'] == sorted(FILES + ['package.json'])
    assert all(Path(item['tarball']).is_file() for item in packet['packages'])
    assert run(package, 'pack').returncode != 0, 'existing packet must not be overwritten'


@pytest.mark.parametrize('hook', ['prepare', 'prepack', 'postpack', 'prepublish', 'prepublishOnly',
                                 'publish', 'postpublish', 'preinstall', 'install', 'postinstall'])
def test_lifecycle_scripts_refused_before_npm_execution(package, hook):
    prepared(package)
    marker = package.parent / 'must-not-exist'
    script = f'node -e "require(\'fs\').writeFileSync(\'{marker}\',\'bad\')"'
    change(package / 'package.json', scripts={hook: script})
    before = (package / 'package.json').read_bytes()
    for command in ('prepare', 'pack'):
        result = run(package, command)
        assert result.returncode != 0
        assert 'lifecycle scripts prohibited' in result.stderr
    assert (package / 'package.json').read_bytes() == before
    assert not marker.exists()
    assert not (package.parent / 'npm-packed').exists()


@pytest.mark.parametrize('config', [{'registry': 'https://example.invalid'}, {'tag': 'evil'},
                                  {'access': 'restricted'}, {'ignore-scripts': False}])
def test_publish_config_cannot_override_release_destination_or_policy(package, config):
    change(package / 'package.json', publishConfig=config)
    result = run(package)
    assert result.returncode != 0
    assert 'publishConfig override' in result.stderr


@pytest.mark.parametrize('field,value', [('type', 'module'), ('exports', './missing.js')])
def test_package_metadata_cannot_override_checked_loader(package, field, value):
    change(package / 'package.json', **{field: value})
    assert run(package).returncode != 0


def test_npm_forced_file_inclusion_is_rejected_by_actual_pack_listing(package):
    (package / 'src').mkdir()
    (package / 'src/leak.js').write_text('not permitted in the root tarball\n')
    change(package / 'package.json', bin={'unexpected': 'src/leak.js'})
    prepared(package)
    result = run(package, 'pack')
    assert result.returncode != 0
    assert 'packed files differ from release allowlist' in result.stderr


def test_missing_platform_after_prepare_cannot_pass_idempotent_repair(package):
    prepared(package)
    shutil.rmtree(package / 'npm/darwin-arm64')
    assert run(package, 'pack').returncode != 0


@pytest.mark.parametrize('field,value', [('name', '@other/platform'), ('version', '1.2.4'),
                                      ('cpu', ['arm64']), ('os', ['darwin']), ('libc', ['musl']),
                                      ('files', ['*.node'])])
def test_pack_rechecks_platform_manifest(package, field, value):
    prepared(package)
    change(package / 'npm/linux-x64-gnu/package.json', **{field: value})
    assert run(package, 'pack').returncode != 0
    assert not (package.parent / 'npm-packed').exists()


@pytest.mark.parametrize('fault', ['missing_listing', 'wrong_name', 'wrong_version', 'unsafe_filename',
                                  'traversal', 'nested_native', 'duplicate', 'empty_file', 'bundled'])
def test_pack_report_rejects_malformed_or_leaking_listing(tmp_path, fault):
    item = {'name': '@anvailabs/sandhi', 'version': VERSION, 'filename': 'anvailabs-sandhi-1.2.3.tgz',
            'files': [{'path': 'package.json', 'size': 20}]}
    if fault == 'missing_listing':
        del item['files']
    elif fault == 'wrong_name':
        item['name'] = '@other/pkg'
    elif fault == 'wrong_version':
        item['version'] = '2.0.0'
    elif fault == 'unsafe_filename':
        item['filename'] = '../escaped.tgz'
    elif fault == 'traversal':
        item['files'][0]['path'] = '../secret'
    elif fault == 'nested_native':
        item['files'].append({'path': 'npm/private.node', 'size': 1})
    elif fault == 'duplicate':
        item['files'] *= 2
    elif fault == 'empty_file':
        item['files'][0]['size'] = 0
    else:
        item['bundled'] = ['unexpected']
    code = '''import {validatePackReport} from %s;
      const report = JSON.parse(process.argv[1]);
      try { validatePackReport(report, {data:{name:'@anvailabs/sandhi'},files:['package.json']}, '1.2.3'); }
      catch { process.exit(7); }
    ''' % json.dumps(HELPER.as_uri())
    result = subprocess.run(['node', '--input-type=module', '-e', code, json.dumps([item])],
                            capture_output=True, text=True, timeout=5)
    assert result.returncode == 7, result.stderr


def test_pinned_napi_generated_manifests_pack_without_prepublication(package):
    """Required pinned-generator compatibility, not cross-platform native execution."""
    cli_root = ROOT / 'bindings/node/node_modules/@napi-rs/cli'
    assert cli_root.is_dir(), 'run npm ci --ignore-scripts in bindings/node before release tests'
    assert json.loads((cli_root / 'package.json').read_text())['version'] == '2.18.4'
    # Copy only public source inputs; the fixture's 64-byte native headers are not
    # real loadable addons and must never be described as native ABI certification.
    shutil.copyfile(ROOT / 'bindings/node/package.json', package / 'package.json')
    for file in FILES:
        source = ROOT / 'bindings/node' / file
        if source.is_file():
            shutil.copyfile(source, package / file)
    natives = {platform: (package / f'npm/{platform}/sandhi.{platform}.node').read_bytes()
               for platform, _, _ in PLATFORMS}
    shutil.rmtree(package / 'npm')
    generated = subprocess.run(['node', str(cli_root / 'scripts/index.js'), 'create-npm-dir', '-t', '.'],
                               cwd=package, capture_output=True, text=True, timeout=30)
    assert generated.returncode == 0, generated.stderr
    for platform, content in natives.items():
        (package / f'npm/{platform}/sandhi.{platform}.node').write_bytes(content)
    prepared(package)
    result = run(package, 'pack')
    assert result.returncode == 0, result.stderr
    packet = json.loads(result.stdout)
    assert [item['name'] for item in packet['packages']] == [
        '@anvailabs/sandhi-linux-x64-gnu', '@anvailabs/sandhi-darwin-arm64', '@anvailabs/sandhi']
