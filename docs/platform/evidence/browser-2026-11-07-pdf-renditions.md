# A browser on a running server, 7 November 2026: a PDF's first page as its renditions

Digital Assets made no renditions of a PDF (FERRUM-ECOSYSTEM-012: "the server carries no decoders
for them"). Digital Assets 0.7.0 draws a PDF's first page with hayro, in pure Rust. This walk took
that on the walk server, this PR's build (platform/pins-rtl at 7bc6f32, the shell from
Ferrum-Frontend 63280d3), in workspace walk, signed in as its administrator, at 00:20–00:26 UTC on 7 November.
Digital Assets was installed there earlier and followed the server to 0.7.0, as a built-in app does.

## What was walked

| Step | What it showed |
|---|---|
| A PDF in Drive | Drive → Upload: `walk-flyer.pdf`, one A4 page (595 by 842 points) with a blue block and the line "Ferrum walk, 7 November" set in Helvetica, a standard font the file does not embed. The pane has no file picker, so the file was built in the page and handed to Drive's own upload box |
| An asset of it | Digital Assets → New Asset: "Walk flyer", its file the PDF. Its kind (PDF), content type, size (642 bytes) and digest came from the file. Saved as a draft |
| Renditions | Within a minute, the upkeep round stored a thumbnail and a preview, with no note. Width and height stay empty, since a page's picture says nothing of the file's size |
| The preview | A JPEG of 904 by 1280 pixels. The block reads rgb(51, 102, 230), the page's 0.2 0.4 0.9 blue, and the margin white. The text's row has 225 dark pixels: the Helvetica line drawn, from the fonts hayro carries |
| The thumbnail | A JPEG of 226 by 320, showing the blue block and the line of text |

## What it found

| Finding | Now |
|---|---|
| The asset's form, reopened, shows its thumbnail and preview as "Download file" links, not as pictures: the kit's attachment field shows no image. Its list has no picture column. The renditions were looked at by fetching them | Not changed in this PR: a kit change, for the attachment field to show an image it holds |

## What it did not cover

- An encrypted PDF: not tried. One hayro cannot read and one with no pages: the suite gives each
  its note.
- A PDF of many pages: only the first is drawn, by design.
