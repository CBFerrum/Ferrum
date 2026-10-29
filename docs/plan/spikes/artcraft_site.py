"""Spike S3: the site artcraft_embed.mjs drives. Unzip a GridCraft web build (its GitHub release's
gridcraft-web-<version>.zip) into <site>/gc, run this, then serve <site> on 127.0.0.1:18771:

    python3 artcraft_site.py <site> && python3 -m http.server 18771 --bind 127.0.0.1 -d <site>

It writes a minimal workbook (a sheet "Sales" with a SUM formula) and two parent pages: host.html
frames GridCraft with ?host=parent&chrome=none on the same origin; host-opaque.html does the same in
an opaque-origin sandbox (sandbox="allow-scripts allow-downloads"), as Ferrum's screens are."""

import sys
import zipfile

site = sys.argv[1]
NS = "http://schemas.openxmlformats.org"
parts = {
    "[Content_Types].xml": f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="{NS}/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>',
    "_rels/.rels": f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="{NS}/package/2006/relationships"><Relationship Id="rId1" Type="{NS}/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>',
    "xl/workbook.xml": f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="{NS}/spreadsheetml/2006/main" xmlns:r="{NS}/officeDocument/2006/relationships"><sheets><sheet name="Sales" sheetId="1" r:id="rId1"/></sheets></workbook>',
    "xl/_rels/workbook.xml.rels": f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="{NS}/package/2006/relationships"><Relationship Id="rId1" Type="{NS}/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>',
    "xl/worksheets/sheet1.xml": f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="{NS}/spreadsheetml/2006/main"><sheetData>'
    '<row r="1"><c r="A1" t="inlineStr"><is><t>Item</t></is></c><c r="B1" t="inlineStr"><is><t>Amount</t></is></c></row>'
    '<row r="2"><c r="A2" t="inlineStr"><is><t>Bolts</t></is></c><c r="B2"><v>120.5</v></c></row>'
    '<row r="3"><c r="A3" t="inlineStr"><is><t>Nuts</t></is></c><c r="B3"><v>79.5</v></c></row>'
    '<row r="4"><c r="A4" t="inlineStr"><is><t>Total</t></is></c><c r="B4"><f>SUM(B2:B3)</f><v>200</v></c></row>'
    "</sheetData></worksheet>",
}
with zipfile.ZipFile(f"{site}/test.xlsx", "w", zipfile.ZIP_DEFLATED) as z:
    for name, xml in parts.items():
        z.writestr(name, xml)
HOST = """<!doctype html><meta charset=utf-8><title>host</title>
<iframe id=f src="gc/index.html?host=parent&chrome=none" style="width:1200px;height:760px;border:0" %s></iframe>
<script>
window.events = []; window.saved = null; const t0 = performance.now();
addEventListener("message", (e) => {
  const d = e.data || {};
  window.events.push({type: d.type, ms: Math.round(performance.now() - t0), origin: e.origin, dirty: d.dirty, error: d.error, name: d.name});
  if (d.type === "gridcraft:save") { const b = new Uint8Array(d.bytes); let s = ""; for (const x of b) s += String.fromCharCode(x); window.saved = {name: d.name, b64: btoa(s), size: b.length}; }
});
window.openTest = async () => { const buf = await (await fetch("test.xlsx")).arrayBuffer(); document.getElementById("f").contentWindow.postMessage({type: "gridcraft:open", name: "test.xlsx", bytes: buf}, location.origin, [buf]); };
</script>"""
open(f"{site}/host.html", "w").write(HOST % "")
open(f"{site}/host-opaque.html", "w").write(HOST % 'sandbox="allow-scripts allow-downloads"')
