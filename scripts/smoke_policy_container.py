"""Opt-in disposable Linux/cgroup-v2 container acceptance. Never deploys production."""

import argparse
import json
import os
from pathlib import Path
import socket
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid

PROBE = r"""
import errno,json,os,pathlib,subprocess,sys,tempfile
cg=pathlib.Path('/sys/fs/cgroup')
status={line.split(':',1)[0]:line.split(':',1)[1].strip() for line in pathlib.Path('/proc/self/status').read_text().splitlines() if ':' in line}
assert os.getuid()!=0 and int(status['CapEff'],16)==0 and status['NoNewPrivs']=='1'
assert int((cg/'memory.max').read_text())==512*1048576
assert int((cg/'memory.swap.max').read_text())==0
assert int((cg/'pids.max').read_text())==64
quota,period=map(int,(cg/'cpu.max').read_text().split());assert quota/period==1
try:
 pathlib.Path('/var/tmp/should-not-write').write_text('no')
 raise AssertionError('root filesystem writable')
except OSError as e:assert e.errno==errno.EROFS
with tempfile.TemporaryDirectory() as directory:
 path=pathlib.Path(directory)/'exec';path.write_text('#!/usr/local/bin/python\n');path.chmod(0o700)
 try:
  subprocess.run([str(path)],check=True)
  raise AssertionError('tmp executable')
 except OSError as e:assert e.errno==errno.EACCES
children=[]; limited=False
try:
 for _ in range(80):
  try:children.append(subprocess.Popen([sys.executable,'-c','import time;time.sleep(20)'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL))
  except OSError as e:
   assert e.errno==errno.EAGAIN;limited=True;break
 assert limited
finally:
 for child in children:child.kill()
 for child in children:child.wait(timeout=5)
print(json.dumps({'uid':os.getuid(),'capabilities':0,'no_new_privileges':True,'memory_max':512*1048576,'swap_max':0,'pids_max':64,'cpu_quota':1,'root_read_only':True,'tmp_noexec':True,'pid_exhaustion_enforced':limited,'probe_children':len(children)}))
"""
CPU_PROBE = r"""
import json,pathlib,time
p=pathlib.Path('/sys/fs/cgroup')
def stats():return dict(line.split() for line in (p/'cpu.stat').read_text().splitlines())
a=stats();end=time.monotonic()+.7
while time.monotonic()<end:pass
b=stats();n=int(b['nr_throttled'])-int(a['nr_throttled']);assert n>0
q,period=map(int,(p/'cpu.max').read_text().split());assert q/period==.25
print(json.dumps({'cpu_quota':.25,'throttled_periods':n}))
"""


def run(args, *, check=True, **kwargs):
    return subprocess.run(args, check=check, capture_output=True, timeout=180, **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sandhi", required=True, type=Path)
    parser.add_argument("--image", required=True)
    parser.add_argument("--fixture", type=Path)
    args = parser.parse_args()
    if args.fixture:
        fixture = args.fixture
    else:
        result = run(
            [
                sys.executable,
                str(args.sandhi / "scripts/smoke_policy_evaluator.py"),
                "--sandhi",
                str(args.sandhi),
            ]
        )
        fixture = Path(json.loads(result.stdout)["evidence_directory"])
    root = Path(tempfile.mkdtemp(prefix="sandhi-container-"))
    project = "sandhi-eval-test-" + uuid.uuid4().hex[:12]
    release = root / "release"
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    model = json.loads((fixture / "example/.runtime/http.json").read_text())
    prepare_args = [
        sys.executable,
        str(args.sandhi / "templates/python-evaluator/containerize.py"),
        str(release),
        "--model",
        str(fixture / "example/.runtime/http.json"),
        "--model",
        str(fixture / "other.json"),
        "--image",
        args.image,
        "--port",
        str(port),
        "--network",
        "published",
        "--auth-file",
        model["auth_file"],
        "--cert-file",
        str(fixture / "cert.pem"),
        "--key-file",
        str(fixture / "key.pem"),
        "--ca-file",
        str(fixture / "ca.pem"),
    ]
    run(prepare_args)
    compose = [
        "docker",
        "compose",
        "--project-name",
        project,
        "--file",
        str(release / "compose.json"),
    ]
    parsed = json.loads(run(compose + ["config", "--format", "json"]).stdout)
    assert len(parsed["services"]) == 1
    token = Path(model["auth_file"]).read_text().strip()
    context = ssl.create_default_context(cafile=str(fixture / "ca.pem"))
    context.load_cert_chain(fixture / "cert.pem", fixture / "key.pem")
    opener = urllib.request.build_opener(
        urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=context)
    )

    def request(path, body=None):
        req = urllib.request.Request(
            f"https://127.0.0.1:{port}" + path,
            data=None if body is None else json.dumps(body).encode(),
            headers={
                "Authorization": "Bearer " + token,
                "Content-Type": "application/json",
            },
        )
        with opener.open(req, timeout=2) as response:
            return json.load(response)

    probe_name = project + "-oom"
    started = False
    try:
        started = True
        run(compose + ["up", "-d", "--no-build", "--pull", "never"])
        container = run(compose + ["ps", "-q", "evaluator"]).stdout.decode().strip()
        assert container
        end = time.monotonic() + 25
        while True:
            try:
                if request("/readyz")["ready"]:
                    break
            except (OSError, urllib.error.URLError):
                pass
            if time.monotonic() >= end:
                (root / "service.log").write_bytes(
                    run(compose + ["logs", "--no-color"], check=False).stdout
                )
                raise AssertionError("container readiness failed; " + str(root))
            time.sleep(0.1)
        for path, text in [
            (fixture / "example/.runtime/http.json", "SYNTHETIC_RESTRICTED"),
            (fixture / "other.json", "OTHER_RESTRICTED"),
        ]:
            m = json.loads(path.read_text())
            body = {
                "version": 1,
                "id": "1",
                "text": text,
                "joined": "",
                "timeout_ms": 200,
                "evaluator": m["name"],
                "artifact_sha256": m["artifact_sha256"],
                "code_sha256": m["bundle_sha256"],
            }
            assert (
                request("/v1/evaluators/" + m["name"] + "/evaluate", body)["score"] == 1
            )
        limits = json.loads(
            run(
                ["docker", "exec", "-i", container, "python", "-I", "-"],
                input=PROBE.encode(),
            ).stdout
        )
        assert request("/readyz")["ready"]
        # No server/credentials/ports are attached to the independent limit probes.
        probe = {
            "services": {
                "probe": {
                    "image": args.image,
                    "pull_policy": "never",
                    "user": f"{os.getuid()}:{os.getgid()}",
                    "init": True,
                    "read_only": True,
                    "cap_drop": ["ALL"],
                    "security_opt": ["no-new-privileges:true"],
                    "cpus": 0.25,
                    "mem_limit": 64 * 1048576,
                    "memswap_limit": 64 * 1048576,
                    "pids_limit": 16,
                    "network_mode": "none",
                    "restart": "no",
                }
            }
        }
        (root / "probe.json").write_text(json.dumps(probe))
        probes = [
            "docker",
            "compose",
            "--project-name",
            project,
            "--file",
            str(root / "probe.json"),
        ]
        cpu = json.loads(
            run(
                probes
                + [
                    "run",
                    "--rm",
                    "-T",
                    "--entrypoint",
                    "python",
                    "probe",
                    "-I",
                    "-c",
                    CPU_PROBE,
                ]
            ).stdout
        )
        oom = run(
            probes
            + [
                "run",
                "--name",
                probe_name,
                "-T",
                "--entrypoint",
                "python",
                "probe",
                "-I",
                "-c",
                "x=bytearray(256*1024*1024)",
            ],
            check=False,
        )
        state = json.loads(
            run(["docker", "inspect", probe_name, "--format", "{{json .State}}"]).stdout
        )
        assert oom.returncode == 137 and state["OOMKilled"] and state["ExitCode"] == 137
        assert request("/readyz")["ready"]
        output = {
            "status": "passed",
            "image": args.image,
            "models_per_listener": 2,
            "published_ports": 1,
            "limits": limits,
            "cpu_throttle_probe": cpu,
            "isolated_memory_probe": {
                "limit_bytes": 64 * 1048576,
                "oom_killed": True,
                "exit_code": 137,
            },
            "evidence_directory": str(root),
            "fixture_directory": str(fixture),
        }
    finally:
        # Only this invocation's UUID-named resources are eligible for cleanup.
        run(["docker", "rm", "-f", probe_name], check=False)
        if started:
            run(compose + ["down", "--remove-orphans", "--timeout", "20"], check=False)
    remaining = run(
        [
            "docker",
            "ps",
            "-aq",
            "--filter",
            "label=com.docker.compose.project=" + project,
        ]
    ).stdout.strip()
    assert not remaining
    # Exercise the secure default separately: no host publication, but authenticated
    # TLS is reachable on the private service network/loopback. The synthetic leaf
    # fixture has both serverAuth/clientAuth; production health clients need their own identity.
    internal_args = list(prepare_args)
    internal_args[2] = str(root / "internal-release")
    internal_args[internal_args.index("--network") + 1] = "internal"
    run(internal_args)
    internal_project = project + "-internal"
    internal_compose = [
        "docker",
        "compose",
        "--project-name",
        internal_project,
        "--file",
        str(root / "internal-release/compose.json"),
    ]
    ready_code = """import ssl,urllib.request,json
ctx=ssl.create_default_context(cafile='/run/evaluator-secrets/client-ca.crt')
ctx.load_cert_chain('/run/evaluator-secrets/server.crt','/run/evaluator-secrets/server.key')
opener=urllib.request.build_opener(urllib.request.ProxyHandler({}),urllib.request.HTTPSHandler(context=ctx))
key=open('/run/evaluator-secrets/service.key').read().strip()
r=urllib.request.Request('https://127.0.0.1:9088/readyz',headers={'Authorization':'Bearer '+key})
with opener.open(r,timeout=2) as response: assert json.load(response)['ready']
print('ready')
"""
    try:
        run(internal_compose + ["up", "-d", "--no-build", "--pull", "never"])
        internal_id = (
            run(internal_compose + ["ps", "-q", "evaluator"]).stdout.decode().strip()
        )
        assert internal_id
        metadata = json.loads(run(["docker", "inspect", internal_id]).stdout)[0]
        assert not metadata["HostConfig"]["PortBindings"]
        networks = metadata["NetworkSettings"]["Networks"]
        assert len(networks) == 1
        network = next(iter(networks))
        assert (
            run(
                ["docker", "network", "inspect", network, "--format", "{{.Internal}}"]
            ).stdout.strip()
            == b"true"
        )
        end = time.monotonic() + 20
        while True:
            checked = run(
                ["docker", "exec", internal_id, "python", "-I", "-c", ready_code],
                check=False,
            )
            if checked.returncode == 0:
                break
            assert time.monotonic() < end
            time.sleep(0.1)
        output["internal_profile"] = {
            "published_ports": 0,
            "network_internal": True,
            "mtls_ready": True,
        }
    finally:
        run(
            internal_compose + ["down", "--remove-orphans", "--timeout", "20"],
            check=False,
        )
    assert not run(
        [
            "docker",
            "ps",
            "-aq",
            "--filter",
            "label=com.docker.compose.project=" + internal_project,
        ]
    ).stdout.strip()
    output["cleanup_verified"] = True
    (root / "result.json").write_text(json.dumps(output, indent=2) + "\n")
    print(json.dumps(output, indent=2))


if __name__ == "__main__":
    main()
