"""Browser editors' fidelity note (docs/plan/browser-editors-fidelity.md): compares each original
file of the corpus with the copy its editor saved unchanged (fidelity_run.mjs), and prints a
Markdown table of what survived.

    python3 -I fidelity_compare.py <corpus dir> <saved dir>
"""

import pathlib
import re
import sys
import xml.etree.ElementTree as ET
import zipfile

S = "{http://schemas.openxmlformats.org/spreadsheetml/2006/main}"
W = "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}"


def parts(path):
    with zipfile.ZipFile(path) as z:
        return {n: z.read(n) for n in z.namelist()}


def count(files, pattern, names=r".*"):
    return sum(len(re.findall(pattern, b.decode("utf8", "replace"))) for n, b in files.items() if re.fullmatch(names, n))


def cells(ref):
    """How many cells a reference such as A3:A5 covers."""
    def rc(a):
        col, row = re.fullmatch(rb"([A-Z]+)(\d+)", a).groups()
        return int(row), sum((ord(ch) - 64) * 26**i for i, ch in enumerate(reversed(col.decode())))
    a, _, b = ref.partition(b":")
    (r1, c1), (r2, c2) = rc(a), rc(b or a)
    return (abs(r2 - r1) + 1) * (abs(c2 - c1) + 1)


def xlsx(path):
    f = parts(path)
    strings = []
    if "xl/sharedStrings.xml" in f:
        for si in ET.fromstring(f["xl/sharedStrings.xml"]).iter(f"{S}si"):
            strings.append("".join(t.text or "" for t in si.iter(f"{S}t")))
    wb = ET.fromstring(f["xl/workbook.xml"])
    sheets = [s.get("name") for s in wb.iter(f"{S}sheet")]
    values, formulas = {}, {}
    for n, b in f.items():
        if not re.fullmatch(r"xl/worksheets/sheet\d+\.xml", n):
            continue
        for c in ET.fromstring(b).iter(f"{S}c"):
            ref, t = c.get("r"), c.get("t")
            v, fm, inline = c.find(f"{S}v"), c.find(f"{S}f"), c.find(f"{S}is")
            text = "".join(x.text or "" for x in inline.iter(f"{S}t")) if inline is not None else (v.text if v is not None else None)
            if t == "s" and text is not None:
                text = strings[int(text)] if int(text) < len(strings) else f"#{text}"
            if text not in (None, ""):
                values[(n, ref)] = text.strip()
            # A shared formula may be written out cell by cell: what counts is that the cell has one.
            if fm is not None:
                formulas[(n, ref)] = True
    return {
        "sheets": sheets,
        "values": values,
        "formulas": formulas,
        "merges": count(f, r"<mergeCell ", r"xl/worksheets/.*"),
        "conditional formats": count(f, r"<conditionalFormatting[ >]", r"xl/worksheets/.*"),
        "validations": count(f, r"<dataValidation[ >]", r"xl/worksheets/.*"),
        # A link over a range may be written as one link per cell: count the cells linked.
        "linked cells": sum(cells(m) for n, b in f.items() if n.startswith("xl/worksheets/") for m in re.findall(rb'<hyperlink [^>]*ref="([A-Z0-9:]+)"', b)),
        "comments": count(f, r"<comment ", r"xl/comments.*"),
        "charts": len([n for n in f if re.fullmatch(r"xl/charts/chart\d+\.xml", n)]),
        "pictures": len([n for n in f if n.startswith("xl/media/")]),
        "pivot tables": len([n for n in f if re.fullmatch(r"xl/pivotTables/pivotTable\d+\.xml", n)]),
        "tables": len([n for n in f if re.fullmatch(r"xl/tables/table\d+\.xml", n)]),
        "defined names": count(f, r"<definedName ", r"xl/workbook\.xml"),
        "headers and footers": count(f, r"<(odd|even|first)(Header|Footer)>", r"xl/worksheets/.*"),
    }


def docx(path):
    f = parts(path)
    body = ET.fromstring(f["word/document.xml"])
    paragraphs = ["".join(t.text or "" for t in p.iter(f"{W}t")).strip() for p in body.iter(f"{W}p")]
    return {
        "text": [p for p in paragraphs if p],
        "tables": count(f, r"<w:tbl>", r"word/document\.xml"),
        "pictures": len([n for n in f if n.startswith("word/media/")]),
        "comments": count(f, r"<w:comment ", r"word/comments\.xml"),
        "tracked changes": count(f, r"<w:(ins|del) ", r"word/document\.xml"),
        "footnotes": max(0, count(f, r"<w:footnote ", r"word/footnotes\.xml") - 2),
        "headers and footers": len([n for n in f if re.fullmatch(r"word/(header|footer)\d+\.xml", n)]),
        "bookmarks": count(f, r"<w:bookmarkStart ", r"word/document\.xml"),
        "hyperlinks": count(f, r"<w:hyperlink ", r"word/document\.xml"),
        "fields": count(f, r"<w:(fldSimple|instrText)[ >]", r"word/document\.xml"),
        "lists": count(f, r"<w:numPr>", r"word/document\.xml"),
        "charts": len([n for n in f if re.fullmatch(r"word/charts/chart\d+\.xml", n)]),
    }


def pdf(path):
    b = path.read_bytes()
    return {"bytes": b, "pages": len(re.findall(rb"/Type\s*/Page(?![s\w])", b)), "annotations": len(re.findall(rb"/Annot\b", b)), "form": b"/AcroForm" in b}


def kept(a, b):
    """What changed between the original's facts `a` and the copy's `b`, for people."""
    out = []
    for k, va in a.items():
        vb = b.get(k)
        if k == "bytes":
            continue
        if isinstance(va, dict):
            same = sum(1 for key, v in va.items() if vb.get(key) == v)
            if same < len(va):
                out.append(f"{k}: {same} of {len(va)} unchanged")
        elif isinstance(va, list):
            if va != vb:
                same = sum(1 for x in va if x in vb)
                out.append(f"{k}: {same} of {len(va)} kept" + (f" (now {len(vb)})" if len(vb) != len(va) else ""))
        elif va != vb:
            out.append(f"{k}: {va} → {vb}")
    return out


def main():
    corpus, saved = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
    print("| File | Kind | What it holds | Saved unchanged by its editor |")
    print("|---|---|---|---|")
    for original in sorted(corpus.iterdir()):
        ext = original.suffix[1:]
        read = {"xlsx": xlsx, "docx": docx, "pdf": pdf}.get(ext)
        if not read:
            continue
        a = read(original)
        holds = ", ".join(f"{k} {len(v) if isinstance(v, (list, dict)) else v}" for k, v in a.items() if k != "bytes" and (v if not isinstance(v, (list, dict)) else len(v)))
        copy = saved / original.name
        if not copy.exists():
            verdict = "**not saved**"
        else:
            b = read(copy)
            if ext == "pdf" and a["bytes"] == b["bytes"]:
                verdict = "identical"
            else:
                diff = kept(a, b)
                verdict = "everything above kept" if not diff else "; ".join(diff)
        print(f"| {original.name} | {ext} | {holds} | {verdict} |")


if __name__ == "__main__":
    main()
