#!/usr/bin/env python3
"""Gathers what a site without internet needs to run one release of Ferrum.

Run on a connected machine, from a checkout at the release's commit (docs/operations.md, "A site
without internet"). It writes a directory to carry across:

    images/      every image, copied with its digest kept (skopeo; `docker save` without it)
    manifests/   with --kubernetes, the operators' manifests at the versions CI tests
    deploy/      this checkout's deploy/ directory, these tools included
    bundle.json  the release, each image with its digest, each manifest with where it came from
    SHA256SUMS   every file's checksum; load.py refuses a bundle that does not match it

    deploy/offline/bundle.py <commit> [--kubernetes] [--with editors,mail,meet,office,clamav]
                             [--platform linux/amd64] [--out ferrum-offline-<commit>]

Needs python3 with PyYAML and skopeo (or docker), signed in to ghcr.io while its packages are
private; --kubernetes needs kubectl too (kustomize is built in). Images named by a moving tag
(`latest`, `stable`) are copied as they are that day; pin them with the variables the Compose
files read (COLLABORA_TAG, STALWART_TAG, CLAMAV_TAG).
"""

import argparse
import base64
import binascii
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

try:
    import yaml
except ImportError:
    sys.exit("bundle.py needs PyYAML: python3 -m pip install pyyaml")

HERE = Path(__file__).resolve().parent
DEPLOY = HERE.parent
REPO = DEPLOY.parent

# The Compose files of each part a site may run beside the server (docs/operations.md).
COMPOSE = {"editors": "editors.yml", "mail": "mail.yml", "meet": "meet.yml", "office": "office.yml"}
EXTRAS = set(COMPOSE) | {"clamav"}

# Registries an image is named on in full inside another string (an operator's settings, a
# Secret's value): what is found there is copied too.
REGISTRIES = ("docker.io", "ghcr.io", "quay.io", "registry.k8s.io", "gcr.io", "mcr.microsoft.com", "public.ecr.aws")
COMPONENT = r"[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*"
TAG = r"[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}"
DIGEST = r"sha256:[0-9a-f]{64}"
REF = re.compile(rf"^(?:[a-z0-9.-]+\.[a-z]+(?::\d+)?/|localhost(?::\d+)?/)?{COMPONENT}(?:/{COMPONENT})*(?::{TAG})?(?:@{DIGEST})?$")
EMBEDDED = re.compile(
    rf"(?<![\w./-])(?:{'|'.join(map(re.escape, REGISTRIES))})/{COMPONENT}(?:/{COMPONENT})*(?::{TAG})?(?:@{DIGEST})?"
)


def normalize(ref: str) -> str:
    """`postgres:17` → `docker.io/library/postgres:17`: the name containerd pulls by."""
    name, _, digest = ref.partition("@")
    first = name.split("/", 1)[0]
    if "/" not in name or not ("." in first or ":" in first or first == "localhost"):
        name = f"docker.io/{name}" if "/" in name else f"docker.io/library/{name}"
    last = name.rsplit("/", 1)[1]
    if ":" not in last and not digest:
        name += ":latest"
    return f"{name}@{digest}" if digest else name


def images_in(text: str) -> set[str]:
    """Every image a set of Kubernetes manifests names: in `image` fields and `imageName`, in
    variables named like `..._IMAGE` (in containers and in Secrets, which hold them base64), and
    anywhere a registry-qualified name appears in a string (operators' own settings)."""
    found: set[str] = set()

    def consider(value: object, named: bool) -> None:
        if not isinstance(value, str):
            return
        value = value.strip()
        if named and REF.match(value) and ("/" in value or ":" in value or "@" in value):
            found.add(value)
        found.update(m.group(0) for m in EMBEDDED.finditer(value))

    def walk(node: object, key: str = "") -> None:
        if isinstance(node, dict):
            if isinstance(node.get("name"), str) and "value" in node and node["name"].upper().endswith("IMAGE"):
                consider(node["value"], True)
            for k, v in node.items():
                walk(v, str(k))
        elif isinstance(node, list):
            for v in node:
                walk(v, key)
        else:
            consider(node, key in ("image", "imageName") or key.upper().endswith("IMAGE"))

    for doc in yaml.safe_load_all(text):
        if not isinstance(doc, dict):
            continue
        walk(doc)
        if doc.get("kind") == "Secret":
            for k, v in (doc.get("data") or {}).items():
                try:
                    decoded = base64.b64decode("".join(str(v).split()), validate=True).decode()
                except (binascii.Error, UnicodeDecodeError):
                    continue
                consider(decoded, str(k).upper().endswith("IMAGE"))
    return found


def compose_images(files: list[Path], env: dict[str, str]) -> set[str]:
    """The images Compose files name, `${VAR:-default}` read as Compose would with `env`."""

    def expand(value: str) -> str:
        def one(m: re.Match) -> str:
            var, default = m.group(1), m.group(3) or ""
            return env.get(var) or default

        return re.sub(r"\$\{(\w+)(:?-([^}]*))?\}", one, value)

    found = set()
    for f in files:
        for service in (yaml.safe_load(f.read_text()) or {}).get("services", {}).values():
            if isinstance(service, dict) and service.get("image"):
                found.add(expand(str(service["image"])))
    return found


def versions() -> dict[str, str]:
    """The operators' versions CI tests, from deploy/kind/install.sh: one place to change them."""
    text = (DEPLOY / "kind" / "install.sh").read_text()
    out = dict(re.findall(r"^([A-Z_]+)=(\S+)$", text, re.M))
    for name in ("ENVOY_GATEWAY", "CERT_MANAGER", "CNPG", "BARMAN_PLUGIN", "PROMETHEUS_OPERATOR", "METRICS_SERVER"):
        if name not in out:
            sys.exit(f"deploy/kind/install.sh no longer sets {name}")
    return out


def operator_manifests(v: dict[str, str]) -> dict[str, str]:
    cnpg_minor = v["CNPG"].rsplit(".", 1)[0]
    prom = f"https://raw.githubusercontent.com/prometheus-operator/prometheus-operator/{v['PROMETHEUS_OPERATOR']}/example/prometheus-operator-crd"
    return {
        "envoy-gateway.yaml": f"https://github.com/envoyproxy/gateway/releases/download/{v['ENVOY_GATEWAY']}/install.yaml",
        "cert-manager.yaml": f"https://github.com/cert-manager/cert-manager/releases/download/{v['CERT_MANAGER']}/cert-manager.yaml",
        "cloudnative-pg.yaml": f"https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/release-{cnpg_minor}/releases/cnpg-{v['CNPG']}.yaml",
        "barman-cloud.yaml": f"https://github.com/cloudnative-pg/plugin-barman-cloud/releases/download/{v['BARMAN_PLUGIN']}/manifest.yaml",
        "metrics-server.yaml": f"https://github.com/kubernetes-sigs/metrics-server/releases/download/{v['METRICS_SERVER']}/components.yaml",
        "servicemonitors.yaml": f"{prom}/monitoring.coreos.com_servicemonitors.yaml",
        "podmonitors.yaml": f"{prom}/monitoring.coreos.com_podmonitors.yaml",
    }


def fetch(url: str) -> bytes:
    """What `url` serves. FERRUM_OFFLINE_FETCH_FROM names a directory to read it from instead
    (tests, or a machine that downloaded the files apart), its file named like the URL."""
    local = os.environ.get("FERRUM_OFFLINE_FETCH_FROM")
    if local:
        return (Path(local) / re.sub(r"[^A-Za-z0-9._-]", "_", url.split("://", 1)[-1])).read_bytes()
    with urllib.request.urlopen(url, timeout=120) as r:
        return r.read()


def envoy_proxy_image(version: str) -> str:
    """The proxy Envoy Gateway starts for each Gateway is named in its code, not its manifests."""
    url = f"https://raw.githubusercontent.com/envoyproxy/gateway/{version}/api/v1alpha1/shared_types.go"
    m = re.search(r'DefaultEnvoyProxyImage\s*=\s*"([^"]+)"', fetch(url).decode())
    if not m:
        sys.exit(f"could not read Envoy Gateway {version}'s proxy image from {url}")
    return m.group(1)


def run(*args: str) -> str:
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout.strip()


def copy_image(ref: str, dest: Path, platform: str, tool: str) -> tuple[str, str]:
    """Copies `ref` into an archive at `dest`; returns the archive's format and the image's digest.
    A reference pinned by digest is copied whole (every platform), so the digest still answers."""
    os_, arch = platform.split("/", 1)
    if tool == "skopeo":
        source = f"docker://{ref}"
        if "@" in ref:
            run("skopeo", "copy", "--quiet", "--all", "--preserve-digests", source, f"oci-archive:{dest}")
        else:
            run("skopeo", "copy", "--quiet", "--override-os", os_, "--override-arch", arch, source, f"oci-archive:{dest}")
        return "oci-archive", run("skopeo", "inspect", "--format", "{{.Digest}}", f"oci-archive:{dest}")
    run("docker", "pull", "--quiet", "--platform", platform, ref)
    run("docker", "save", "-o", str(dest), ref)
    pinned = run("docker", "image", "inspect", "--format", "{{index .RepoDigests 0}}", ref)
    return "docker-archive", pinned.rpartition("@")[2]


def pin_kustomization(path: Path, commit: str) -> None:
    """The bundle's deploy/k8s names the release's images, whatever tag the checkout named."""
    text = path.read_text()
    pinned, n = re.subn(r"(- name: (?:ferrum|ghcr\.io/cbferrum/ferrum-editors)\n(?:\s+newName: \S+\n)?\s+newTag: )\S+", rf"\g<1>{commit}", text)
    if n != 2:
        sys.exit("deploy/k8s/kustomization.yaml no longer names the server's and the editors' images as bundle.py expects")
    path.write_text(pinned)


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def write_sums(out: Path) -> None:
    lines = [f"{sha256(p)}  {p.relative_to(out).as_posix()}" for p in sorted(out.rglob("*")) if p.is_file() and p.name != "SHA256SUMS"]
    (out / "SHA256SUMS").write_text("\n".join(lines) + "\n")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("commit", help="the release: a commit CI published images for (docs/operations.md, Images)")
    ap.add_argument("--kubernetes", action="store_true", help="for deploy/k8s, with the operators it needs")
    ap.add_argument("--with", dest="extras", default="", help=f"also: {', '.join(sorted(EXTRAS))}")
    ap.add_argument("--platform", default="linux/amd64", help="the site's machines (default linux/amd64)")
    ap.add_argument("--image", action="append", default=[], help="another image to carry (repeatable)")
    ap.add_argument("--tool", choices=["skopeo", "docker"], help="how to copy images (default: skopeo when installed)")
    ap.add_argument("--out", type=Path)
    a = ap.parse_args()

    commit = a.commit.strip()
    if not re.fullmatch(r"[0-9a-f]{7,40}", commit):
        sys.exit("the release is a commit's hash, as CI tags its images (a moving tag would not say what was carried)")
    extras = {e.strip() for e in a.extras.split(",") if e.strip()}
    if extras - EXTRAS:
        sys.exit(f"unknown: {', '.join(sorted(extras - EXTRAS))} (choose from {', '.join(sorted(EXTRAS))})")
    tool = a.tool or ("skopeo" if shutil.which("skopeo") else "docker" if shutil.which("docker") else None)
    if not tool or not shutil.which(tool):
        sys.exit("needs skopeo (keeps digests) or docker")
    if tool == "docker" and a.kubernetes:
        print("warning: without skopeo, images pinned by digest get new digests in the site's registry; install skopeo", file=sys.stderr)

    head = ""
    try:
        head = run("git", "-C", str(REPO), "rev-parse", "HEAD")
    except (subprocess.CalledProcessError, FileNotFoundError):
        pass
    if head and not head.startswith(commit) and not commit.startswith(head):
        print(f"warning: deploy/ is this checkout's ({head[:12]}), not the release's ({commit[:12]}); check out the release first", file=sys.stderr)

    out = (a.out or Path(f"ferrum-offline-{commit[:12]}")).resolve()
    if out.exists() and any(out.iterdir()):
        sys.exit(f"{out} is not empty")
    (out / "images").mkdir(parents=True)

    server = f"ghcr.io/cbferrum/ferrum:{commit}"
    editors = f"ghcr.io/cbferrum/ferrum-editors:{commit}"
    env = {**os.environ, "FERRUM_IMAGE": server, "FERRUM_EDITORS_IMAGE": editors}
    wanted: set[str] = set(a.image)
    manifests: list[dict] = []
    if a.kubernetes:
        rendered = run("kubectl", "kustomize", str(DEPLOY / "k8s"))
        # The server's images are the release's, whatever tag the kustomization names today.
        wanted |= {i for i in images_in(rendered) if not i.startswith(("ghcr.io/cbferrum/ferrum:", "ghcr.io/cbferrum/ferrum-editors:"))}
        wanted.add(server)
        if "editors" in extras:
            wanted.add(editors)
        v = versions()
        (out / "manifests").mkdir()
        for name, url in operator_manifests(v).items():
            body = fetch(url)
            (out / "manifests" / name).write_bytes(body)
            manifests.append({"file": f"manifests/{name}", "url": url})
            wanted |= images_in(body.decode())
        wanted.add(envoy_proxy_image(v["ENVOY_GATEWAY"]))
        for e in sorted(extras - {"editors", "clamav"}):
            print(f"note: {e} runs beside the cluster from {COMPOSE[e]}; its image is carried for that host", file=sys.stderr)
            wanted |= compose_images([DEPLOY / COMPOSE[e]], env)
    else:
        wanted |= compose_images([DEPLOY / "docker-compose.yml"] + [DEPLOY / COMPOSE[e] for e in sorted(extras - {"clamav"})], env)
    if "clamav" in extras:
        wanted.add(f"clamav/clamav:{os.environ.get('CLAMAV_TAG', 'stable')}")

    images = []
    for n, ref in enumerate(sorted(wanted), 1):
        file = Path("images") / f"{n:02d}-{re.sub(r'[^a-z0-9.-]+', '-', normalize(ref).split('@')[0].lower())}.tar"
        print(f"==> {ref}", file=sys.stderr)
        fmt, digest = copy_image(ref, out / file, a.platform, tool)
        images.append({"ref": ref, "name": normalize(ref), "digest": digest, "file": file.as_posix(), "format": fmt})

    shutil.copytree(DEPLOY, out / "deploy", ignore=shutil.ignore_patterns("__pycache__", ".env", "test"))
    if a.kubernetes:
        pin_kustomization(out / "deploy" / "k8s" / "kustomization.yaml", commit)
    (out / "bundle.json").write_text(
        json.dumps(
            {
                "ferrum": commit,
                "deploy_commit": head or None,
                "profile": "kubernetes" if a.kubernetes else "compose",
                "with": sorted(extras),
                "platform": a.platform,
                "created": datetime.now(timezone.utc).isoformat(timespec="seconds"),
                "server": server,
                "editors": editors if "editors" in extras else None,
                "images": images,
                "manifests": manifests,
            },
            indent=2,
        )
        + "\n"
    )
    write_sums(out)
    print(f"{out}: {len(images)} images, {len(manifests)} manifests; carry it whole and run deploy/offline/load.py there")


if __name__ == "__main__":
    main()
