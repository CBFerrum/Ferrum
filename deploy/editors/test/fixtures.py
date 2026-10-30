"""The editor server's browser test (relay.test.mjs): a stand-in for Ferrum's web app and the files
it opens. Writes into <dir>:
* parent.html: frames the relay (?relay=<relay.html address>&program=<editor>&file=<name>), connects,
  sends the file and records what comes back, as Drive's editor page does;
* test.xlsx (a sheet "Sales" with a SUM), test.docx (one paragraph) and test.pdf (one page).

    python3 fixtures.py <dir> && python3 -m http.server 8091 --bind 127.0.0.1 -d <dir>
"""

import sys
import zipfile

out = sys.argv[1]
NS = "http://schemas.openxmlformats.org"
REL = f"{NS}/officeDocument/2006/relationships"
XML = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'


def package(name, parts):
    with zipfile.ZipFile(f"{out}/{name}", "w", zipfile.ZIP_DEFLATED) as z:
        for path, xml in parts.items():
            z.writestr(path, XML + xml)


package(
    "test.xlsx",
    {
        "[Content_Types].xml": f'<Types xmlns="{NS}/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/></Types>',
        "_rels/.rels": f'<Relationships xmlns="{NS}/package/2006/relationships"><Relationship Id="rId1" Type="{REL}/officeDocument" Target="xl/workbook.xml"/></Relationships>',
        "xl/workbook.xml": f'<workbook xmlns="{NS}/spreadsheetml/2006/main" xmlns:r="{REL}"><sheets><sheet name="Sales" sheetId="1" r:id="rId1"/></sheets></workbook>',
        "xl/_rels/workbook.xml.rels": f'<Relationships xmlns="{NS}/package/2006/relationships"><Relationship Id="rId1" Type="{REL}/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="{REL}/styles" Target="styles.xml"/></Relationships>',
        "xl/styles.xml": f'<styleSheet xmlns="{NS}/spreadsheetml/2006/main"><fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts><fills count="1"><fill><patternFill patternType="none"/></fill></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf/></cellStyleXfs><cellXfs count="1"><xf xfId="0"/></cellXfs></styleSheet>',
        "xl/worksheets/sheet1.xml": f'<worksheet xmlns="{NS}/spreadsheetml/2006/main"><sheetData>'
        '<row r="1"><c r="A1" t="inlineStr"><is><t>Item</t></is></c><c r="B1" t="inlineStr"><is><t>Amount</t></is></c></row>'
        '<row r="2"><c r="A2" t="inlineStr"><is><t>Bolts</t></is></c><c r="B2"><v>120.5</v></c></row>'
        '<row r="3"><c r="A3" t="inlineStr"><is><t>Nuts</t></is></c><c r="B3"><v>79.5</v></c></row>'
        '<row r="4"><c r="A4" t="inlineStr"><is><t>Total</t></is></c><c r="B4"><f>SUM(B2:B3)</f><v>200</v></c></row>'
        "</sheetData></worksheet>",
    },
)

W = f"{NS}/wordprocessingml/2006/main"
package(
    "test.docx",
    {
        "[Content_Types].xml": f'<Types xmlns="{NS}/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>',
        "_rels/.rels": f'<Relationships xmlns="{NS}/package/2006/relationships"><Relationship Id="rId1" Type="{REL}/officeDocument" Target="word/document.xml"/></Relationships>',
        "word/document.xml": f'<w:document xmlns:w="{W}"><w:body><w:p><w:r><w:t>Quarterly plan</w:t></w:r></w:p><w:sectPr/></w:body></w:document>',
    },
)

# One A4 page with a line of text, its cross-reference table at the offsets it states.
objects = [
    "<< /Type /Catalog /Pages 2 0 R >>",
    "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>",
    None,
    "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
]
stream = b"BT /F1 24 Tf 72 760 Td (Quarterly report) Tj ET"
pdf = bytearray(b"%PDF-1.4\n")
offsets = []
for i, body in enumerate(objects, start=1):
    offsets.append(len(pdf))
    if body is None:
        pdf += f"{i} 0 obj\n<< /Length {len(stream)} >>\nstream\n".encode() + stream + b"\nendstream\nendobj\n"
    else:
        pdf += f"{i} 0 obj\n{body}\nendobj\n".encode()
xref = len(pdf)
pdf += f"xref\n0 {len(objects) + 1}\n0000000000 65535 f \n".encode()
pdf += "".join(f"{o:010d} 00000 n \n" for o in offsets).encode()
pdf += f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
open(f"{out}/test.pdf", "wb").write(pdf)

open(f"{out}/parent.html", "w").write(
    """<!doctype html><meta charset=utf-8><title>Ferrum stand-in</title>
<style>body{margin:0} iframe{display:block;width:1200px;height:760px;border:0}</style>
<iframe id=f sandbox="allow-scripts allow-same-origin allow-downloads allow-popups allow-popups-to-escape-sandbox"
  allow="clipboard-read; clipboard-write; fullscreen"></iframe>
<script>
const q = new URLSearchParams(location.search);
const relay = q.get("relay"), program = q.get("program"), file = q.get("file");
window.events = []; window.saved = null;
const f = document.getElementById("f");
f.onload = async () => {
  const ch = new MessageChannel();
  ch.port1.onmessage = (e) => {
    const d = e.data || {};
    window.events.push({type: d.type, version: d.version, dirty: d.dirty, error: d.error, name: d.name});
    if (d.type === "save") {
      const b = new Uint8Array(d.bytes); let s = "";
      for (const x of b) s += String.fromCharCode(x);
      window.saved = {name: d.name, b64: btoa(s), size: b.length};
    }
  };
  f.contentWindow.postMessage({type: "connect", author: "Test Person"}, new URL(relay).origin, [ch.port2]);
  if (file) {
    const bytes = await (await fetch(file)).arrayBuffer();
    ch.port1.postMessage({type: "open", name: file, bytes}, [bytes]);
  }
};
f.src = `${relay}?program=${encodeURIComponent(program)}`;
</script>
""")
