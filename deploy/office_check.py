#!/usr/bin/env python3
"""Checks online editing end to end, at the protocol level (docs/operations.md, "Online editing").

For a docx, an xlsx and a pptx made in Drive it opens a session the way Drive's page does, then does
what the editor does over WOPI: CheckFileInfo, LOCK, GetFile, PutFile (a changed file) and UNLOCK, and
checks that the save became a new version with the new bytes. It also has the real editor load one of
the files over its WebSocket, which makes the editor itself call Ferrum back (its allow-list, its
address for Ferrum, the proof signature when it has a key).

    deploy/office_check.py --url http://127.0.0.1:8080 --tenant smoke --token <bearer token> \\
                           --office http://127.0.0.1:9980

--url is Ferrum as you reach it; --office is the editor as you reach it (its public address when you
run this from outside: the editor refuses a WebSocket whose origin is not its own address).
When the editor publishes a proof key, only the editor may call WOPI: the direct calls are then
checked to be refused, and the editor's own load is the proof that the rest works. Exit status 1 on
the first failure. Needs python3 only. The files it makes are left in Drive's root, named smoke-*.
"""

import argparse
import base64
import io
import json
import os
import struct
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile

TYPES = ("docx", "xlsx", "pptx")


def fail(msg):
    print(f"FAILED: {msg}", file=sys.stderr)
    sys.exit(1)


def call(method, url, body=None, headers=None, token=None):
    req = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    if token:
        req.add_header("authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, dict(r.headers), r.read()
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read()


def ws_frame(text):
    b = text.encode()
    mask = os.urandom(4)
    n = len(b)
    head = bytes([0x81, 0x80 | n]) if n < 126 else bytes([0x81, 0x80 | 126]) + struct.pack(">H", n)
    return head + mask + bytes(c ^ mask[i % 4] for i, c in enumerate(b))


def ws_messages(data):
    out, i = [], 0
    while i + 2 <= len(data):
        n = data[i + 1] & 0x7F
        j = i + 2
        if n == 126:
            n = struct.unpack(">H", data[j : j + 2])[0]
            j += 2
        elif n == 127:
            n = struct.unpack(">Q", data[j : j + 8])[0]
            j += 8
        out.append(data[j : j + n].decode(errors="replace"))
        i = j + n
    return out


def editor_loads(office, wopi_src, token):
    """The editor itself opens the file: it calls CheckFileInfo and GetFile on Ferrum. True when it loads."""
    import socket

    u = urllib.parse.urlparse(office)
    # As the editor's own page does: the token travels in the document's address.
    wopi_src = f"{wopi_src}?access_token={urllib.parse.quote(token)}&access_token_ttl=0"
    quoted = urllib.parse.quote(wopi_src, safe="")
    s = socket.create_connection((u.hostname, u.port or (443 if u.scheme == "https" else 80)), timeout=20)
    s.send(
        (
            f"GET /cool/{quoted}/ws HTTP/1.1\r\nHost: {u.netloc}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {base64.b64encode(os.urandom(16)).decode()}\r\nOrigin: {u.scheme}://{u.netloc}\r\n\r\n"
        ).encode()
    )
    s.recv(4096)
    s.send(ws_frame("coolclient 0.1"))
    s.send(ws_frame(f"load url={wopi_src} access_token={token} access_token_ttl=0 lang=en-US"))
    seen, deadline = [], time.time() + 60
    s.settimeout(10)
    while time.time() < deadline:
        try:
            data = s.recv(65536)
        except OSError:
            break
        if not data:
            break
        for m in ws_messages(data):
            seen.append(m[:120])
            if m.startswith("error:"):
                s.close()
                return False, seen
            if m.startswith(("status:", "perm:")) or ('"command": "load"' in m and '"success": true' in m):
                s.close()
                return True, seen
    s.close()
    return False, seen


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--url", required=True)
    ap.add_argument("--tenant", required=True)
    ap.add_argument("--token", required=True, help="a bearer token of a person who may use Drive")
    ap.add_argument("--office", required=True)
    ap.add_argument("--no-editor-load", action="store_true", help="skip having the editor load a file itself")
    a = ap.parse_args()
    url, office = a.url.rstrip("/"), a.office.rstrip("/")

    print("==> the editor's discovery document")
    status, _, body = call("GET", f"{office}/hosting/discovery")
    if status != 200:
        fail(f"{office}/hosting/discovery answered {status}")
    xml = body.decode(errors="replace")
    for ext in TYPES:
        if f'ext="{ext}" name="edit"' not in xml:
            fail(f"the editor offers no editor for .{ext}")
    proof = "<proof-key " in xml
    print(f"    docx, xlsx and pptx can be edited; proof key {'published' if proof else 'not published'}")

    print("==> a server whose editor works says so")
    status, _, body = call("GET", f"{url}/api/app/drive/editors", token=a.token)
    if status != 200 or not json.loads(body).get("online"):
        fail(f"Drive does not report an online editor: {status} {body[:200]!r}")

    stamp = str(int(time.time()))
    first = None
    for ext in TYPES:
        print(f"==> .{ext}: a new file, a session, and the editor's calls")
        status, _, body = call("POST", f"{url}/api/app/drive/new?kind={ext}&name=smoke-{stamp}", token=a.token)
        if status not in (200, 201):
            fail(f"a new .{ext} file: {status} {body[:200]!r}")
        file_id = json.loads(body)["id"]
        status, _, body = call("POST", f"{url}/api/app/drive/editor/{file_id}", token=a.token)
        if status != 200:
            fail(f"opening .{ext} in the editor: {status} {body[:300]!r}")
        launch = json.loads(body)
        if "WOPISrc=" not in launch["url"] or not launch["url"].startswith(("http://", "https://")) or launch["action"] != "edit":
            fail(f"the editor page for .{ext}: {launch}")
        src = urllib.parse.unquote(urllib.parse.parse_qs(urllib.parse.urlparse(launch["url"]).query)["WOPISrc"][0])
        token = launch["access_token"]
        first = first or (src, token)
        # Drive's address for the editor's calls may be an internal one: call it as the editor would, on --url.
        path = urllib.parse.urlparse(src).path
        base = f"{url}{path}"
        q = f"?access_token={urllib.parse.quote(token)}"
        if proof:
            status, _, _ = call("GET", base + q)
            if status == 200:
                fail("a WOPI call without the editor's signature was answered although the editor publishes a proof key")
            print("    unsigned calls are refused (the editor publishes a proof key)")
            continue
        status, _, body = call("GET", base + "?access_token=wrong")
        if status != 401:
            fail(f"a wrong token answered {status}, not 401")
        status, _, body = call("GET", base + q)
        info = json.loads(body) if status == 200 else {}
        for key, want in (("UserCanWrite", True), ("SupportsLocks", True), ("SupportsUpdate", True), ("ReadOnly", False)):
            if info.get(key) != want:
                fail(f"CheckFileInfo for .{ext}: {key} is {info.get(key)!r} ({status})")
        if not info.get("BaseFileName", "").endswith(f".{ext}"):
            fail(f"CheckFileInfo's file name: {info.get('BaseFileName')!r}")
        lock = f"smoke-{ext}-{stamp}"
        status, _, _ = call("POST", base + q, headers={"X-WOPI-Override": "LOCK", "X-WOPI-Lock": lock})
        if status != 200:
            fail(f"LOCK answered {status}")
        status, _, original = call("GET", f"{base}/contents{q}")
        # A file made by Drive's New menu is empty until the editor first saves it; save a minimal package then.
        if status != 200 or (original and not zipfile.is_zipfile(io.BytesIO(original))):
            fail(f"GetFile for .{ext} did not return an Office file ({status})")
        out = io.BytesIO(original)
        with zipfile.ZipFile(out, "a" if original else "w") as z:
            if not original:
                z.writestr("[Content_Types].xml", '<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/></Types>')
                z.writestr("_rels/.rels", '<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>')
            z.writestr("customXml/ferrum-smoke.txt", f"saved by office_check {stamp}")
        changed = out.getvalue()
        status, headers, _ = call("POST", f"{base}/contents{q}", body=changed, headers={"X-WOPI-Override": "PUT", "X-WOPI-Lock": lock})
        if status != 200:
            fail(f"PutFile for .{ext} answered {status}")
        status, _, _ = call("POST", f"{base}/contents{q}", body=changed, headers={"X-WOPI-Override": "PUT", "X-WOPI-Lock": "another"})
        if status != 409:
            fail(f"PutFile with another lock answered {status}, not 409")
        status, _, _ = call("POST", base + q, headers={"X-WOPI-Override": "UNLOCK", "X-WOPI-Lock": lock})
        if status != 200:
            fail(f"UNLOCK answered {status}")
        status, _, body = call("GET", f"{url}/api/app/drive/files/{file_id}/content", token=a.token)
        if status != 200 or body != changed:
            fail(f"the saved .{ext} is not what the editor sent ({status}, {len(body)} bytes, wanted {len(changed)})")
        status, _, body = call("GET", base + q)
        if json.loads(body)["Version"] == info["Version"]:
            fail(f"the version of .{ext} did not change after the save")
        print(f"    CheckFileInfo, LOCK, GetFile ({len(original)} bytes), PutFile ({len(changed)} bytes), UNLOCK; version {info['Version']} -> {json.loads(body)['Version']}")

    if not a.no_editor_load:
        print("==> the editor serves only Ferrum's WOPI host")
        ok, seen = editor_loads(office, "http://evil.example:80/wopi/files/1", "x")
        if ok:
            fail("the editor loaded a document from a host that is not Ferrum")
        print("    a document from another host is refused")
        print("==> the editor loads a file itself (its allow-list and its calls to Ferrum)")
        ok, seen = editor_loads(office, *first)
        if not ok:
            fail(f"the editor could not load the file: {seen}")
        print("    loaded")
    print("online editing works")


if __name__ == "__main__":
    main()
