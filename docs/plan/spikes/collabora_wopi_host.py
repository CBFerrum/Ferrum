"""Spike S1: the smallest WOPI host, standing in for Ferrum's Office app over Drive. One file, one
user; it logs every call the editor makes and keeps each saved version.

    python3 wopi_host.py <dir> [port]     # serves <dir>/test.docx; also /editor.html

CheckFileInfo, GetFile, PutFile, Lock, Unlock, RefreshLock and GetLock, as Collabora uses them."""

import io
import json
import sys
import time
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

DIR = sys.argv[1]
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 18900
TOKEN = "spike-token"
state = {"lock": "", "versions": 0}
log = open(f"{DIR}/wopi.log", "a", buffering=1)


def docx(text: str) -> bytes:
    """A minimal Word document with one paragraph."""
    w = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
    parts = {
        "[Content_Types].xml": '<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>',
        "_rels/.rels": '<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>',
        "word/document.xml": f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="{w}"><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>',
    }
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        for name, xml in parts.items():
            z.writestr(name, xml)
    return buf.getvalue()


def current() -> bytes:
    with open(f"{DIR}/test.docx", "rb") as f:
        return f.read()


class Wopi(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def answer(self, status, body=b"", ctype="application/json", headers=None):
        self.send_response(status)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(body)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def record(self, what):
        h = self.headers
        log.write(json.dumps({"t": round(time.time(), 2), "call": what, "path": urlparse(self.path).path,
                              "proof": bool(h.get("X-WOPI-Proof")), "proof_old": bool(h.get("X-WOPI-ProofOld")),
                              "timestamp": h.get("X-WOPI-TimeStamp"), "lock": h.get("X-WOPI-Lock"),
                              "ua": (h.get("User-Agent") or "")[:40]}) + "\n")

    def authorized(self):
        return parse_qs(urlparse(self.path).query).get("access_token", [""])[0] == TOKEN

    def do_GET(self):
        path = urlparse(self.path).path
        if path == "/editor.html":
            return self.answer(200, open(f"{DIR}/editor.html", "rb").read(), "text/html; charset=utf-8")
        if not path.startswith("/wopi/files/1") or not self.authorized():
            return self.answer(401 if path.startswith("/wopi/") else 404)
        if path.endswith("/contents"):
            self.record("GetFile")
            return self.answer(200, current(), "application/octet-stream")
        self.record("CheckFileInfo")
        body = current()
        info = {"BaseFileName": "test.docx", "Size": len(body), "OwnerId": "ferrum", "UserId": "ann",
                "UserFriendlyName": "Ann", "Version": str(state["versions"]), "UserCanWrite": True,
                "SupportsLocks": True, "SupportsUpdate": True, "SupportsGetLock": True}
        return self.answer(200, json.dumps(info).encode())

    def do_POST(self):
        path = urlparse(self.path).path
        if not path.startswith("/wopi/files/1") or not self.authorized():
            return self.answer(401)
        override = self.headers.get("X-WOPI-Override", "")
        body = self.rfile.read(int(self.headers.get("content-length") or 0))
        lock = self.headers.get("X-WOPI-Lock", "")
        if path.endswith("/contents") and override == "PUT":
            self.record("PutFile")
            if state["lock"] and lock != state["lock"]:
                return self.answer(409, headers={"X-WOPI-Lock": state["lock"]})
            state["versions"] += 1
            with open(f"{DIR}/test.docx", "wb") as f:
                f.write(body)
            with open(f"{DIR}/saved-{state['versions']}.docx", "wb") as f:
                f.write(body)
            return self.answer(200, json.dumps({"LastModifiedTime": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}).encode())
        self.record(override or "POST")
        if override == "LOCK":
            if state["lock"] and state["lock"] != lock:
                return self.answer(409, headers={"X-WOPI-Lock": state["lock"]})
            state["lock"] = lock
        elif override == "UNLOCK":
            if state["lock"] != lock:
                return self.answer(409, headers={"X-WOPI-Lock": state["lock"]})
            state["lock"] = ""
        elif override == "GET_LOCK":
            return self.answer(200, headers={"X-WOPI-Lock": state["lock"]})
        return self.answer(200)


if __name__ == "__main__":
    import os
    if not os.path.exists(f"{DIR}/test.docx"):
        with open(f"{DIR}/test.docx", "wb") as f:
            f.write(docx("Hello from Ferrum Drive."))
    ThreadingHTTPServer(("127.0.0.1", PORT), Wopi).serve_forever()
