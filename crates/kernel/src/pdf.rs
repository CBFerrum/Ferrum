//! PDFs of print formats, made by a Chromium rendering service with Gotenberg's HTTP API
//! (`FERRUM_PDF_URL`, e.g. a `gotenberg/gotenberg:8` container beside the API). The page is the
//! document's print format; the service only lays it out, so API replicas stay small and stateless.

use crate::error::{Error, Result};
use crate::{Ctx, Kernel};
use std::sync::LazyLock;
use std::time::Duration;
use uuid::Uuid;

/// Largest PDF accepted from the renderer.
const MAX_PDF: usize = 50 << 20;

static CLIENT: LazyLock<reqwest::Client> =
    LazyLock::new(|| reqwest::Client::builder().connect_timeout(Duration::from_secs(5)).timeout(Duration::from_secs(60)).build().unwrap_or_default());

/// A `multipart/form-data` body: the page as `index.html` (with `header.html` and `footer.html` when there are some),
/// then form fields.
fn form(boundary: &str, files: &[(&str, &str)], fields: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::with_capacity(files.iter().map(|f| f.1.len()).sum::<usize>() + 1024);
    for (name, html) in files {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{name}\"\r\nContent-Type: text/html; charset=utf-8\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(html.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// The paper a PDF is laid out on, in inches as the PDF service takes it: a format's page (its margins grown by a
/// letter head's header and footer repeated on every page), or today's A4 with the page's own `@page` preferred.
#[derive(Debug, Clone, PartialEq)]
pub struct PdfPage {
    pub width_in: f64,
    pub height_in: f64,
    pub margin_in: [f64; 4],
    /// The page's own `@page` size and margins win (a template with no page of its own).
    pub prefer_css: bool,
}

const MM: f64 = 25.4;

impl PdfPage {
    /// As pages were laid out before formats had paper: A4, 0.4 in margins (0.8 in where a header or footer sits),
    /// the page's `@page` CSS preferred.
    pub fn from_css(header: bool, footer: bool) -> PdfPage {
        PdfPage { width_in: 8.27, height_in: 11.69, margin_in: [if header { 0.8 } else { 0.4 }, 0.4, if footer { 0.8 } else { 0.4 }, 0.4], prefer_css: true }
    }

    /// A format's page: its sheet (landscape turned) and margins in millimetres, the top grown by `header_mm` and the
    /// bottom by `footer_mm` (a letter head repeated by the PDF service sits there).
    pub fn from_setup(p: &ferrum_meta::layout::PageSetup, header_mm: f64, footer_mm: f64) -> PdfPage {
        let (w, h) = p.sheet_mm();
        let m = p.margin_mm;
        PdfPage {
            width_in: w / MM,
            height_in: h / MM,
            margin_in: [(m.top as f64 + header_mm) / MM, m.right as f64 / MM, (m.bottom as f64 + footer_mm) / MM, m.left as f64 / MM],
            prefer_css: false,
        }
    }

    /// The form fields the PDF service takes.
    pub fn fields(&self) -> Vec<(&'static str, String)> {
        let n = |v: f64| format!("{:.2}", v);
        let [t, r, b, l] = self.margin_in.map(|v| format!("{:.3}", v));
        vec![
            ("preferCssPageSize", self.prefer_css.to_string()),
            ("printBackground", "true".into()),
            ("paperWidth", n(self.width_in)),
            ("paperHeight", n(self.height_in)),
            ("marginTop", t),
            ("marginBottom", b),
            ("marginLeft", l),
            ("marginRight", r),
        ]
    }
}

impl Kernel {
    /// Whether this server can make PDFs (a renderer is configured).
    pub fn pdf_enabled(&self) -> bool {
        self.settings.pdf_url.is_some()
    }

    /// A document's print format as a PDF, if the caller may read the document.
    pub async fn print_pdf(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Result<Vec<u8>> {
        let html = self.render_print(ctx, entity, id).await?;
        self.html_to_pdf(&html).await
    }

    /// Lays out a self-contained HTML page as a PDF: A4 unless the page's CSS (`@page`) says
    /// otherwise, backgrounds printed.
    pub async fn html_to_pdf(&self, html: &str) -> Result<Vec<u8>> {
        self.page_to_pdf(html, None, None).await
    }

    /// [`Kernel::html_to_pdf`] with a header and a footer repeated on every page, each a page of its own in which
    /// `<span class="pageNumber">` and `<span class="totalPages">` are filled in; the margins they sit in are made room
    /// for.
    pub async fn page_to_pdf(&self, html: &str, header: Option<&str>, footer: Option<&str>) -> Result<Vec<u8>> {
        self.page_to_pdf_on(html, header, footer, &PdfPage::from_css(header.is_some(), footer.is_some())).await
    }

    /// [`Kernel::page_to_pdf`] on the paper and margins `page` says.
    pub async fn page_to_pdf_on(&self, html: &str, header: Option<&str>, footer: Option<&str>, page: &PdfPage) -> Result<Vec<u8>> {
        let base = self.settings.pdf_url.as_deref().ok_or_else(|| Error::Unavailable("PDF output is not set up on this server (FERRUM_PDF_URL)".into()))?;
        let boundary = format!("ferrum-pdf-{}", Uuid::now_v7().simple());
        let owned = page.fields();
        let fields: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let url = format!("{}/forms/chromium/convert/html", base.trim_end_matches('/'));
        let mut res = CLIENT
            .post(url)
            .header("content-type", format!("multipart/form-data; boundary={boundary}"))
            .body(form(
                &boundary,
                &[Some(("index.html", html)), header.map(|h| ("header.html", h)), footer.map(|f| ("footer.html", f))].into_iter().flatten().collect::<Vec<_>>(),
                &fields,
            ))
            .send()
            .await
            .map_err(|e| Error::Unavailable(format!("PDF renderer unreachable: {e}")))?;
        if !res.status().is_success() {
            return Err(Error::Unavailable(format!("PDF renderer answered {}", res.status())));
        }
        let mut pdf = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|e| Error::Unavailable(format!("PDF renderer: {e}")))? {
            if pdf.len() + chunk.len() > MAX_PDF {
                return Err(Error::Unavailable("the PDF is larger than 50 MB".into()));
            }
            pdf.extend_from_slice(&chunk);
        }
        if !pdf.starts_with(b"%PDF") {
            return Err(Error::Unavailable("the PDF renderer returned something that is not a PDF".into()));
        }
        Ok(pdf)
    }
}

#[cfg(test)]
mod tests {
    use super::{PdfPage, form};

    /// T11: A5 landscape with a 20 mm header repeated: the sheet turned, the top margin grown by the header.
    #[test]
    fn a_formats_paper_reaches_the_pdf_service() {
        let p: ferrum_meta::layout::PageSetup = serde_json::from_value(serde_json::json!({"size": "A5", "orientation": "landscape"})).unwrap();
        let f = PdfPage::from_setup(&p, 20.0, 0.0).fields();
        let get = |k: &str| f.iter().find(|(n, _)| *n == k).unwrap().1.clone();
        assert_eq!((get("paperWidth"), get("paperHeight")), ("8.27".into(), "5.83".into()));
        assert_eq!(get("marginTop"), format!("{:.3}", 35.0 / 25.4));
        assert_eq!((get("marginBottom"), get("marginLeft"), get("preferCssPageSize")), ("0.591".into(), "0.472".into(), "false".into()));
        let old = PdfPage::from_css(false, true).fields();
        assert!(old.contains(&("preferCssPageSize", "true".into())) && old.contains(&("marginBottom", "0.800".into())));
    }

    #[test]
    fn the_page_and_fields_travel_as_one_form() {
        let body = String::from_utf8(form("B", &[("index.html", "<p>Hi</p>")], &[("printBackground", "true")])).unwrap();
        assert_eq!(
            body,
            "--B\r\nContent-Disposition: form-data; name=\"files\"; filename=\"index.html\"\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hi</p>\r\n\
             --B\r\nContent-Disposition: form-data; name=\"printBackground\"\r\n\r\ntrue\r\n--B--\r\n"
        );
    }
}
