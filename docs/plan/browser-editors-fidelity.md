# Browser editors: fidelity on real files

30 October 2026. Item 6 of [browser-editors.md](browser-editors.md) §9: does each editor keep
what real files hold when it opens one and saves it unchanged?

**Setup.**
* Editors: GridCraft 0.13.0, WordCraft 0.12.0 and PdfCraft 0.15.0, from the editor server
  (`deploy/editors`) and its relay.
* Browser: Chrome through Playwright 1.64.1.
* Each file was opened as Drive's editor page opens it, saved unchanged with Ctrl/⌘+S, and the
  copy compared with the original.

**To repeat.** `spikes/fidelity_run.mjs`, then `spikes/fidelity_compare.py`. The compare script
reads the parts of each file with the standard library and counts what is listed below.

**Corpus.** 25 public files, not kept in the repository:
* 10 workbooks and 10 documents from Apache POI's test data
  (https://github.com/apache/poi/tree/trunk/test-data, `spreadsheet/` and `document/`);
* 5 PDFs from Mozilla pdf.js's tests (https://github.com/mozilla/pdf.js/tree/master/test/pdfs).

They were chosen for what they hold: charts, pivot tables, tables, comments, conditional
formatting, shared formulas, pictures, headers and footers, hyperlinks, footnotes, tracked
changes, bookmarks, nested tables, annotations and form fields.

## Results

| File | Kind | What it holds | Saved unchanged by its editor |
|---|---|---|---|
| 123233_charts.xlsx | xlsx | sheets 5, merges 7, charts 4 | everything above kept |
| 50867_with_table.xlsx | xlsx | sheets 3, values 9, tables 1 | everything above kept |
| ExcelPivotTableSample.xlsx | xlsx | sheets 3, values 50, pivot tables 2, tables 1 | everything above kept |
| SampleSS.xlsx | xlsx | sheets 3, values 15, formulas 1 | everything above kept |
| commentTest.xlsx | xlsx | sheets 1, values 2, comments 14 | everything above kept |
| conditional_formatting_cell_is.xlsx | xlsx | sheets 2, values 11, formulas 1, conditional formats 4, defined names 2 | everything above kept |
| headerFooterTest.xlsx | xlsx | sheets 1, values 2, headers and footers 2 | everything above kept |
| picture-and-shape-same-size.xlsx | xlsx | sheets 1, values 1, pictures 1, headers and footers 2 | everything above kept |
| shared_formulas.xlsx | xlsx | sheets 1, values 81, formulas 40, defined names 63 | everything above kept (a) |
| sharedhyperlink.xlsx | xlsx | sheets 1, values 4, linked cells 4 | everything above kept (b) |
| SampleDoc.docx | docx | text 6 | everything above kept |
| bookmarks.docx | docx | text 2, bookmarks 2 | everything above kept |
| bug56075-changeTracking_on.docx | docx | text 1, bookmarks 1 | everything above kept |
| chartex.docx | docx | text 1, pictures 2, bookmarks 1, lists 3, charts 6 | everything above kept |
| comment.docx | docx | comments 1 | everything above kept |
| footnotes.docx | docx | text 1, footnotes 1 | everything above kept |
| headerFooter.docx | docx | headers and footers 6 | everything above kept |
| headerPic.docx | docx | pictures 1, headers and footers 1 | everything above kept |
| table-alignment.docx | docx | text 54, tables 6 | everything above kept |
| deep-table-cell.docx | docx | text 5000, tables 5000 | **not opened** (c) |
| alphatrans.pdf | pdf | pages 1, annotations 1 | identical |
| annotation-freetext.pdf | pdf | pages 1, form | identical |
| annotation-highlight.pdf | pdf | pages 1, form | identical |
| annotation-link-text-popup.pdf | pdf | pages 1, form | identical |
| annotation-button-widget.pdf | pdf | pages 1, form | original kept, appearances added (d) |

Notes:
* (a) GridCraft writes a shared formula out cell by cell (`A2:A41 = B2…` becomes `A3 = B3`, and so
  on). The values are the same.
* (b) A link over a range (`A3:A5`) becomes one link per cell, to the same address. Its optional
  display text is dropped.
* (c) A stress file from a bug report: 5 000 tables nested in each other.
  * WordCraft does not open it. It shows its start page and saves nothing.
  * It does not report the failure to its host either (see Findings).
* (d) PdfCraft saved an incremental update. The original's bytes are kept unchanged at the start,
  followed by appearance streams for the form's two buttons.

The edit tests (deploy/editors/test, and the check through Drive) cover saving with changes:
* a typed cell value, with the sheet's names, strings and formulas kept;
* a typed paragraph;
* a PDF saved back.

## Findings

* **Real files survive.** On 24 of the 25 files, an unchanged save kept every feature
  counted: charts, pivot tables, tables, comments, conditional formats, pictures, headers and
  footers, footnotes, bookmarks and the values.
  * PDFs come back byte for byte, or as an incremental update over the untouched original.
  * The two rewrites (a, b) are equivalent forms.
* **WordCraft and PdfCraft ignore `?chrome=none`.** GridCraft hides its title bar and File tab.
  The other two show their whole frame: File, Open, New, AutoSave, Share, and links to their
  community.
  * Every file they hand back still goes to Drive through the bridge.
  * Drive's editor page therefore saves only a file under the opened name as a new version of it.
    A save under another name (Save As, an export, a file opened inside the editor) becomes a
    new file beside it.
* **A file an editor cannot open is not reported.** The editor stays on its start page. The page
  says "ready", and a person sees the editor's home instead of their file.

Not compared:
* **Collabora's results on the same files.** No Collabora server ran on this machine.
  Collabora stays the default wherever the operator runs it, so its results decide nothing here.
* **How the files look.** Checked by eye on the screenshots of the files above, not measured.

## What changes

* **Defaults stay as built.**
  * "Open" uses the online editor where the operator runs one.
  * Browser editors are the default only where no online editor runs, and always in "Open with".
* **Two more requests upstream** (browser-editors.md §8):
  * WordCraft and PdfCraft should honour `?chrome=none` as GridCraft does;
  * every editor should report a file it could not open (`<app>:failed`).
