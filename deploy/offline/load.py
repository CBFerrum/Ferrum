#!/usr/bin/env python3
"""Loads a bundle made by bundle.py at a site without internet (docs/operations.md, "A site
without internet"). Run it from the bundle, as carried:

    deploy/offline/load.py                                  # one host: into its Docker
    deploy/offline/load.py --registry registry.site:5000    # Kubernetes: into the site's registry

It first checks every file against SHA256SUMS and refuses the bundle if anything differs, is
missing or was added. Then:

* one host: the images go into Docker under their own names, and release.env pins the Compose
  files to them (`docker compose --env-file deploy/.env --env-file release.env ...`);
* a registry: each image goes to <registry>/<its registry>/<its path> with its digest kept, and
  containerd/certs.d/ holds a hosts.toml for each registry they came from, which sends the nodes'
  pulls there (copy it to /etc/containerd/certs.d/ on every node); the bundle's
  deploy/k8s/kustomization.yaml already names the release, and its operators' manifests are ready
  for deploy/offline/install.sh.

Needs python3; skopeo for a registry (docker for one host without skopeo).
"""

import argparse
import hashlib
import json
import shutil
import subprocess
import sys
from pathlib import Path

BUNDLE = Path(__file__).resolve().parents[2]


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def verify(root: Path) -> None:
    sums = root / "SHA256SUMS"
    if not sums.is_file():
        sys.exit(f"{root} has no SHA256SUMS: not a bundle made by bundle.py")
    listed = {}
    for line in sums.read_text().splitlines():
        digest, _, name = line.partition("  ")
        listed[name] = digest
    bad = [name for name, digest in listed.items() if not (root / name).is_file() or sha256(root / name) != digest]
    # Python's own caches appear when these tools run from the bundle; nothing else may.
    extra = [
        p.relative_to(root).as_posix()
        for p in root.rglob("*")
        if p.is_file() and p.name != "SHA256SUMS" and "__pycache__" not in p.parts and p.name != "release.env"
        and not p.relative_to(root).as_posix().startswith("containerd/")
        and p.relative_to(root).as_posix() not in listed
    ]
    if bad or extra:
        for name in bad:
            print(f"differs or missing: {name}", file=sys.stderr)
        for name in extra:
            print(f"not in the bundle: {name}", file=sys.stderr)
        sys.exit("refused: the bundle is not as it was made")


def run(*args: str) -> None:
    subprocess.run(args, check=True)


def split(name: str) -> tuple[str, str, str, str]:
    """`docker.io/envoyproxy/envoy:v1@sha256:…` → registry, path, tag, digest."""
    rest, _, digest = name.partition("@")
    registry, _, path = rest.partition("/")
    path, _, tag = path.rpartition(":") if ":" in path.rsplit("/", 1)[-1] else (path, "", "")
    return registry, path, tag, digest


def hosts_toml(registry: str, mirror: str, insecure: bool) -> str:
    upstream = "https://registry-1.docker.io" if registry == "docker.io" else f"https://{registry}"
    scheme = "http" if insecure else "https"
    lines = [
        f"# {registry}, pulled from the site's registry (Ferrum's offline bundle, deploy/offline/load.py).",
        f'server = "{upstream}"',
        "",
        f'[host."{scheme}://{mirror}/v2/{registry}"]',
        '  capabilities = ["pull", "resolve"]',
        "  override_path = true",
    ]
    if insecure:
        lines.append("  skip_verify = true")
    return "\n".join(lines) + "\n"


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("bundle", nargs="?", type=Path, default=BUNDLE)
    ap.add_argument("--registry", help="the site's registry, host[:port], for Kubernetes")
    ap.add_argument("--insecure", action="store_true", help="the registry speaks plain http, or its certificate is not trusted")
    ap.add_argument("--check", action="store_true", help="only check the bundle")
    a = ap.parse_args()

    root = a.bundle.resolve()
    verify(root)
    meta = json.loads((root / "bundle.json").read_text())
    print(f"bundle of Ferrum {meta['ferrum'][:12]} ({meta['profile']}, {len(meta['images'])} images) checks out")
    if a.check:
        return

    if a.registry:
        if not shutil.which("skopeo"):
            sys.exit("loading into a registry needs skopeo, which keeps each image's digest")
        tls = ["--dest-tls-verify=false"] if a.insecure else []
        registries = set()
        for image in meta["images"]:
            registry, path, tag, digest = split(image["name"])
            registries.add(registry)
            # Pushed under its tag when it has one; the digest is kept either way, so pins by digest answer.
            dest = f"docker://{a.registry}/{registry}/{path}" + (f":{tag}" if tag else f"@{digest}")
            source = f"{image['format']}:{root / image['file']}"
            print(f"==> {image['name']}")
            run("skopeo", "copy", "--quiet", "--all", "--preserve-digests", *tls, source, dest)
        for registry in sorted(registries):
            out = root / "containerd" / "certs.d" / registry / "hosts.toml"
            out.parent.mkdir(parents=True, exist_ok=True)
            out.write_text(hosts_toml(registry, a.registry, a.insecure))
        print(
            f"\nNext, on every node: copy {root / 'containerd' / 'certs.d'}/* to /etc/containerd/certs.d/ "
            '(containerd reads it with config_path = "/etc/containerd/certs.d" under its CRI registry settings); '
            "then deploy/offline/install.sh and docs/operations.md, \"A site without internet\"."
        )
        return

    for image in meta["images"]:
        print(f"==> {image['ref']}")
        if image["format"] == "docker-archive":
            run("docker", "load", "--quiet", "-i", str(root / image["file"]))
        else:
            if not shutil.which("skopeo"):
                sys.exit("this bundle was made with skopeo: load it with skopeo, or ask for one made with docker")
            run("skopeo", "copy", "--quiet", f"oci-archive:{root / image['file']}", f"docker-daemon:{image['ref'].split('@')[0]}")
    pins = [f"FERRUM_IMAGE={meta['server']}"]
    if meta.get("editors"):
        pins.append(f"FERRUM_EDITORS_IMAGE={meta['editors']}")
    for image in meta["images"]:
        for var, repo in (("STALWART_TAG", "stalwartlabs/stalwart"), ("COLLABORA_TAG", "collabora/code")):
            if image["ref"].startswith(repo + ":"):
                pins.append(f"{var}={image['ref'].split(':', 1)[1]}")
    (root / "release.env").write_text("# Written by deploy/offline/load.py: the images this bundle loaded.\n" + "\n".join(pins) + "\n")
    print(f"\nNext: docker compose -f deploy/docker-compose.yml --env-file deploy/.env --env-file {root / 'release.env'} up -d")


if __name__ == "__main__":
    main()
