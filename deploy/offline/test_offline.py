#!/usr/bin/env python3
"""bundle.py and load.py against stand-ins for skopeo, docker and kubectl, and operators' manifests
written here: what a bundle carries, that it checks out, what loading does, and that a bundle
changed after it was made is refused. CI runs it (.github/workflows/ci.yml, audit).

    python3 deploy/offline/test_offline.py
"""

import base64
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import bundle  # noqa: E402

COMMIT = "0123456789abcdef0123456789abcdef01234567"
ENVOY = "docker.io/envoyproxy/envoy:distroless-v1.39.5@sha256:" + "9d" * 32
SIDECAR = "ghcr.io/cloudnative-pg/plugin-barman-cloud-sidecar:v0.15.1"

# Each stand-in logs its arguments as a JSON line to $STUB_LOG and does just enough of its job.
STUB = r'''#!/usr/bin/env python3
import hashlib, json, os, sys
args = sys.argv[1:]
with open(os.environ["STUB_LOG"], "a") as log:
    log.write(json.dumps([os.path.basename(sys.argv[0])] + args) + "\n")
tool = os.path.basename(sys.argv[0])
plain = [a for a in args if not a.startswith("-")]
if tool == "skopeo" and args[0] == "copy":
    source, dest = plain[-2], plain[-1]
    if source.startswith("docker://") and dest.startswith("oci-archive:"):
        open(dest.split(":", 1)[1], "w").write("image " + source[len("docker://"):] + "\n")
elif tool == "skopeo" and args[0] == "inspect":
    print("sha256:" + hashlib.sha256(open(plain[-1].split(":", 1)[1], "rb").read()).hexdigest())
elif tool == "docker" and args[0] == "save":
    open(args[args.index("-o") + 1], "w").write("image " + plain[-1] + "\n")
elif tool == "docker" and args[:2] == ["image", "inspect"]:
    print(plain[-1].rsplit(":", 1)[0] + "@sha256:" + hashlib.sha256(plain[-1].encode()).hexdigest())
elif tool == "kubectl" and args[0] == "kustomize":
    print(open(os.environ["STUB_KUSTOMIZE"]).read())
'''

RENDERED = """apiVersion: apps/v1
kind: Deployment
metadata: {name: ferrum}
spec: {template: {spec: {containers: [{name: ferrum, image: ghcr.io/cbferrum/ferrum:latest}]}}}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: pdf}
spec: {template: {spec: {containers: [{name: pdf, image: gotenberg/gotenberg:8}]}}}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: ferrum-office, namespace: ferrum-office}
spec: {template: {spec: {containers: [{name: collabora, image: collabora/code:26.04.5.2.1}]}}}
---
apiVersion: postgresql.cnpg.io/v1
kind: Cluster
metadata: {name: ferrum-db}
spec: {instances: 3, imageName: ghcr.io/cloudnative-pg/postgresql:17}
"""


def deployment(image: str, env: str = "") -> str:
    return f"""apiVersion: apps/v1
kind: Deployment
metadata: {{name: x}}
spec: {{template: {{spec: {{containers: [{{name: x, image: {image}{env}}}]}}}}}}
"""


def manifests(where: Path) -> None:
    """The operators' manifests as their releases name images: in containers, in an operator's
    settings, in a Secret (base64, over several lines) and only in the controller's code."""
    sidecar = base64.b64encode(SIDECAR.encode()).decode()
    bodies = {
        "envoy-gateway.yaml": deployment("envoyproxy/gateway:v1.9.3")
        + "---\napiVersion: v1\nkind: ConfigMap\nmetadata: {name: eg}\ndata:\n  envoy-gateway.yaml: |\n    rateLimitDeployment:\n      container: {image: docker.io/envoyproxy/ratelimit:5ae0682e}\n",
        "cert-manager.yaml": deployment("quay.io/jetstack/cert-manager-controller:v1.21.3")
        + "---\n"
        + deployment("quay.io/jetstack/cert-manager-webhook:v1.21.3"),
        "cloudnative-pg.yaml": deployment("ghcr.io/cloudnative-pg/cloudnative-pg:1.30.1"),
        "barman-cloud.yaml": deployment("ghcr.io/cloudnative-pg/plugin-barman-cloud:v0.15.1", ", env: [{name: SIDECAR_IMAGE, valueFrom: {secretKeyRef: {name: s, key: SIDECAR_IMAGE}}}]")
        + f"---\napiVersion: v1\nkind: Secret\nmetadata: {{name: s}}\ndata:\n  SIDECAR_IMAGE: |\n    {sidecar[:40]}\n    {sidecar[40:]}\n",
        "metrics-server.yaml": deployment("registry.k8s.io/metrics-server/metrics-server:v0.9.0"),
        "servicemonitors.yaml": "apiVersion: apiextensions.k8s.io/v1\nkind: CustomResourceDefinition\nmetadata: {name: servicemonitors.monitoring.coreos.com}\n",
        "podmonitors.yaml": "apiVersion: apiextensions.k8s.io/v1\nkind: CustomResourceDefinition\nmetadata: {name: podmonitors.monitoring.coreos.com}\n",
    }
    named = lambda url: re.sub(r"[^A-Za-z0-9._-]", "_", url.split("://", 1)[-1])  # noqa: E731
    v = bundle.versions()
    for name, url in bundle.operator_manifests(v).items():
        (where / named(url)).write_text(bodies[name])
    go = f"https://raw.githubusercontent.com/envoyproxy/gateway/{v['ENVOY_GATEWAY']}/api/v1alpha1/shared_types.go"
    (where / named(go)).write_text(f'const (\n\tDefaultEnvoyProxyImage = "{ENVOY}"\n)\n')


class Env:
    def __init__(self, tmp: Path, tools: tuple[str, ...]):
        self.bin = tmp / "bin"
        self.bin.mkdir()
        for tool in tools:
            (self.bin / tool).write_text(STUB)
            (self.bin / tool).chmod(0o755)
        self.log = tmp / "calls.jsonl"
        self.fetch = tmp / "fetch"
        self.fetch.mkdir()
        manifests(self.fetch)
        (tmp / "rendered.yaml").write_text(RENDERED)
        self.env = {
            **os.environ,
            "PATH": f"{self.bin}{os.pathsep}{os.environ['PATH']}",
            "STUB_LOG": str(self.log),
            "STUB_KUSTOMIZE": str(tmp / "rendered.yaml"),
            "FERRUM_OFFLINE_FETCH_FROM": str(self.fetch),
            "PYTHONDONTWRITEBYTECODE": "1",
        }

    def run(self, *args: str, ok: bool = True, **env: str) -> subprocess.CompletedProcess:
        p = subprocess.run([sys.executable, *args], env={**self.env, **env}, capture_output=True, text=True)
        assert (p.returncode == 0) == ok, f"{args}: exit {p.returncode}\n{p.stdout}\n{p.stderr}"
        return p

    def calls(self, tool: str) -> list[list[str]]:
        if not self.log.exists():
            return []
        return [c[1:] for c in map(json.loads, self.log.read_text().splitlines()) if c[0] == tool]


def kubernetes(tmp: Path) -> None:
    e = Env(tmp, ("skopeo", "kubectl"))
    out = tmp / "k8s-bundle"
    e.run(str(HERE / "bundle.py"), COMMIT, "--kubernetes", "--with", "editors", "--out", str(out))
    meta = json.loads((out / "bundle.json").read_text())
    refs = {i["ref"] for i in meta["images"]}
    expected = {
        f"ghcr.io/cbferrum/ferrum:{COMMIT}",
        f"ghcr.io/cbferrum/ferrum-editors:{COMMIT}",
        "gotenberg/gotenberg:8",
        "collabora/code:26.04.5.2.1",
        "ghcr.io/cloudnative-pg/postgresql:17",
        "envoyproxy/gateway:v1.9.3",
        "docker.io/envoyproxy/ratelimit:5ae0682e",
        ENVOY,
        "quay.io/jetstack/cert-manager-controller:v1.21.3",
        "quay.io/jetstack/cert-manager-webhook:v1.21.3",
        "ghcr.io/cloudnative-pg/cloudnative-pg:1.30.1",
        "ghcr.io/cloudnative-pg/plugin-barman-cloud:v0.15.1",
        SIDECAR,
        "registry.k8s.io/metrics-server/metrics-server:v0.9.0",
    }
    assert refs == expected, f"carried {sorted(refs ^ expected)} differently"
    assert not any(r.startswith("ghcr.io/cbferrum/ferrum:latest") for r in refs), "the release, not latest"
    names = {i["ref"]: i["name"] for i in meta["images"]}
    assert names["gotenberg/gotenberg:8"] == "docker.io/gotenberg/gotenberg:8" and names[ENVOY] == ENVOY
    assert all(i["format"] == "oci-archive" and re.fullmatch(r"sha256:[0-9a-f]{64}", i["digest"]) for i in meta["images"])
    assert {m["file"] for m in meta["manifests"]} == {f"manifests/{n}" for n in bundle.operator_manifests(bundle.versions())}
    # Pinned by digest: copied whole, so the digest still answers in the site's registry.
    envoy_copy = next(c for c in e.calls("skopeo") if c[0] == "copy" and f"docker://{ENVOY}" in c)
    assert "--all" in envoy_copy and "--preserve-digests" in envoy_copy, envoy_copy
    tagged = next(c for c in e.calls("skopeo") if c[0] == "copy" and "docker://gotenberg/gotenberg:8" in c)
    assert tagged[tagged.index("--override-arch") + 1] == "amd64", tagged
    kustomization = (out / "deploy" / "k8s" / "kustomization.yaml").read_text()
    assert kustomization.count(f"newTag: {COMMIT}") == 2 and "newTag: latest" not in kustomization
    assert (out / "deploy" / "offline" / "load.py").is_file(), "the tools travel with the bundle"
    for line in (out / "SHA256SUMS").read_text().splitlines():
        digest, name = line.split("  ", 1)
        assert hashlib.sha256((out / name).read_bytes()).hexdigest() == digest, name

    load = str(out / "deploy" / "offline" / "load.py")
    e.run(load, "--check")
    e.run(load, "--registry", "registry.site:5000", "--insecure")
    pushed = {c[-1] for c in e.calls("skopeo") if c[0] == "copy" and c[-1].startswith("docker://registry.site:5000/")}
    assert f"docker://registry.site:5000/docker.io/envoyproxy/envoy:distroless-v1.39.5" in pushed, pushed
    assert "docker://registry.site:5000/docker.io/library/postgres:17" not in pushed, "no postgres image on Kubernetes"
    assert "docker://registry.site:5000/ghcr.io/cloudnative-pg/postgresql:17" in pushed
    assert len(pushed) == len(expected)
    assert all("--dest-tls-verify=false" in c for c in e.calls("skopeo") if c[0] == "copy" and c[-1].startswith("docker://registry.site"))
    certs = out / "containerd" / "certs.d"
    assert {p.name for p in certs.iterdir()} == {"docker.io", "ghcr.io", "quay.io", "registry.k8s.io"}
    docker_io = (certs / "docker.io" / "hosts.toml").read_text()
    assert 'server = "https://registry-1.docker.io"' in docker_io and '[host."http://registry.site:5000/v2/docker.io"]' in docker_io
    assert "override_path = true" in docker_io and "skip_verify = true" in docker_io
    e.run(load, "--check")  # what loading wrote is not part of the bundle

    # Changed, or added to, after it was made: refused.
    image = out / meta["images"][0]["file"]
    image.write_text(image.read_text() + "tampered\n")
    assert "differs or missing" in e.run(load, "--check", ok=False).stderr
    image.write_text(image.read_text().replace("tampered\n", ""))
    e.run(load, "--check")
    (out / "images" / "99-extra.tar").write_text("smuggled\n")
    assert "not in the bundle: images/99-extra.tar" in e.run(load, "--check", ok=False).stderr


def compose(tmp: Path) -> None:
    e = Env(tmp, ("docker",))
    out = tmp / "compose-bundle"
    e.run(str(HERE / "bundle.py"), COMMIT[:12], "--tool", "docker", "--with", "mail,clamav", "--out", str(out), CLAMAV_TAG="1.4")
    meta = json.loads((out / "bundle.json").read_text())
    refs = {i["ref"] for i in meta["images"]}
    assert refs == {f"ghcr.io/cbferrum/ferrum:{COMMIT[:12]}", "postgres:17", "gotenberg/gotenberg:8", "collabora/code:26.04.5.2.1", "stalwartlabs/stalwart:v1.0", "clamav/clamav:1.4"}, refs
    assert meta["profile"] == "compose" and not meta["manifests"] and not (out / "manifests").exists()
    assert all(i["format"] == "docker-archive" for i in meta["images"])
    assert not (out / "deploy" / ".env").exists(), "the operator's secrets stay behind"
    e.run(str(out / "deploy" / "offline" / "load.py"))
    loaded = [c for c in e.calls("docker") if c[0] == "load"]
    assert len(loaded) == len(refs), loaded
    release = (out / "release.env").read_text()
    assert f"FERRUM_IMAGE=ghcr.io/cbferrum/ferrum:{COMMIT[:12]}" in release and "STALWART_TAG=v1.0" in release
    e.run(str(out / "deploy" / "offline" / "load.py"), "--check")


def refusals(tmp: Path) -> None:
    e = Env(tmp, ("skopeo", "kubectl"))
    assert "a commit's hash" in e.run(str(HERE / "bundle.py"), "latest", "--out", str(tmp / "x"), ok=False).stderr
    assert "unknown: mail2" in e.run(str(HERE / "bundle.py"), COMMIT, "--with", "mail2", "--out", str(tmp / "y"), ok=False).stderr
    (tmp / "z").mkdir()
    (tmp / "z" / "something").write_text("")
    assert "is not empty" in e.run(str(HERE / "bundle.py"), COMMIT, "--out", str(tmp / "z"), ok=False).stderr
    assert "no SHA256SUMS" in e.run(str(HERE / "load.py"), str(tmp / "z"), "--check", ok=False).stderr


def main() -> None:
    for case in (kubernetes, compose, refusals):
        with tempfile.TemporaryDirectory() as tmp:
            case(Path(tmp))
        print(f"ok  {case.__name__}")


if __name__ == "__main__":
    main()
