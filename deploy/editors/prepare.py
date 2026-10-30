"""Builds the editor server's site (the Dockerfile's first stage).

    python3 prepare.py <site dir> <nginx conf dir> [<dir of downloaded zips>]

For each program in builds.txt: its official web build, from the GitHub release (or the given
directory of already downloaded zips), checked against the pinned SHA-256 and unpacked unmodified
under <site>/<program>/. Then, for nginx:
* <conf>/programs.conf.in: one location per program, whose Content Security Policy names the
  SHA-256 of each inline script in the program's index.html (Trunk starts the app from one), so
  no other inline script can run. Its pages may be framed by the relay and, as frame-ancestors
  covers every ancestor, by the web app above it: 50-editors.sh fills in @ORIGINS@ at start;
* a gzip copy of each text and WebAssembly file, served by gzip_static.
"""

import base64
import gzip
import hashlib
import html.parser
import io
import pathlib
import re
import sys
import urllib.request
import zipfile

HERE = pathlib.Path(__file__).parent
POLICY = (
    "default-src 'self'; script-src 'self' 'wasm-unsafe-eval' {hashes}; style-src 'self' 'unsafe-inline'; "
    "img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self'; worker-src 'self' blob:; "
    "form-action 'none'; base-uri 'none'; object-src 'none'; frame-ancestors 'self' @ORIGINS@"
)
COMPRESS = {".wasm", ".js", ".html", ".css", ".svg", ".json", ".txt", ".md"}


class InlineScripts(html.parser.HTMLParser):
    """The text of each script element without `src`, as the browser hashes it."""

    def __init__(self):
        super().__init__(convert_charrefs=False)
        self.scripts, self.current = [], None

    def handle_starttag(self, tag, attrs):
        if tag == "script" and not dict(attrs).get("src"):
            self.current = []

    def handle_data(self, data):
        if self.current is not None:
            self.current.append(data)

    def handle_endtag(self, tag):
        if tag == "script" and self.current is not None:
            self.scripts.append("".join(self.current))
            self.current = None


def builds():
    for line in (HERE / "builds.txt").read_text().splitlines():
        if line.strip() and not line.startswith("#"):
            program, version, url, sha = line.split()
            if not re.fullmatch(r"[a-z][a-z0-9-]{0,39}", program) or not re.fullmatch(r"[0-9a-f]{64}", sha):
                sys.exit(f"builds.txt: bad line: {line}")
            yield program, version, url, sha


def fetch(url, cached):
    if cached and (cached / url.rsplit("/", 1)[1]).is_file():
        return (cached / url.rsplit("/", 1)[1]).read_bytes()
    with urllib.request.urlopen(url, timeout=600) as r:
        return r.read()


def unpack(data, dest):
    """The archive holds one folder (<program>-web-<version>/): its contents go to `dest`."""
    with zipfile.ZipFile(io.BytesIO(data)) as z:
        tops = {n.split("/", 1)[0] for n in z.namelist()}
        if len(tops) != 1:
            sys.exit(f"expected one folder in the archive, found {sorted(tops)}")
        top = tops.pop() + "/"
        for info in z.infolist():
            rel = info.filename[len(top) :]
            if not rel or info.is_dir():
                continue
            path = (dest / rel).resolve()
            if dest.resolve() not in path.parents:
                sys.exit(f"archive entry outside its folder: {info.filename}")
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(z.read(info))


def main():
    site, conf = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
    cached = pathlib.Path(sys.argv[3]) if len(sys.argv) > 3 else None
    conf.mkdir(parents=True, exist_ok=True)
    locations = []
    for program, version, url, sha in builds():
        data = fetch(url, cached)
        got = hashlib.sha256(data).hexdigest()
        if got != sha:
            sys.exit(f"{program} {version}: SHA-256 {got}, but builds.txt pins {sha}")
        dest = site / program
        unpack(data, dest)
        page = (dest / "index.html").read_text(encoding="utf-8")
        parser = InlineScripts()
        parser.feed(page)
        hashes = " ".join(f"'sha256-{base64.b64encode(hashlib.sha256(s.encode()).digest()).decode()}'" for s in parser.scripts)
        locations.append(
            f"# {program} {version}\n"
            f"location /{program}/ {{\n"
            f'    add_header Content-Security-Policy "{POLICY.format(hashes=hashes)}" always;\n'
            f"    add_header Cache-Control $editors_cache always;\n"
            f"    include /etc/nginx/editors/headers.conf;\n"
            f"}}\n"
        )
        print(f"{program} {version}: {len(parser.scripts)} inline script(s)")
    for path in site.rglob("*"):
        if path.is_file() and path.suffix in COMPRESS:
            path.with_name(path.name + ".gz").write_bytes(gzip.compress(path.read_bytes(), compresslevel=9, mtime=0))
    (conf / "programs.conf.in").write_text("".join(locations))


if __name__ == "__main__":
    main()
