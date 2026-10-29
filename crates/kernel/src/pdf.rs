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

/// A `multipart/form-data` body: the page as `index.html`, then form fields.
fn form(boundary: &str, html: &str, fields: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::with_capacity(html.len() + 1024);
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"index.html\"\r\nContent-Type: text/html; charset=utf-8\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(html.as_bytes());
    body.extend_from_slice(b"\r\n");
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
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
        let base = self.settings.pdf_url.as_deref().ok_or_else(|| Error::Unavailable("PDF output is not set up on this server (FERRUM_PDF_URL)".into()))?;
        let boundary = format!("ferrum-pdf-{}", Uuid::now_v7().simple());
        let fields = [
            ("preferCssPageSize", "true"),
            ("printBackground", "true"),
            ("paperWidth", "8.27"),
            ("paperHeight", "11.69"),
            ("marginTop", "0.4"),
            ("marginBottom", "0.4"),
            ("marginLeft", "0.4"),
            ("marginRight", "0.4"),
        ];
        let url = format!("{}/forms/chromium/convert/html", base.trim_end_matches('/'));
        let mut res = CLIENT
            .post(url)
            .header("content-type", format!("multipart/form-data; boundary={boundary}"))
            .body(form(&boundary, html, &fields))
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
    use super::form;

    #[test]
    fn the_page_and_fields_travel_as_one_form() {
        let body = String::from_utf8(form("B", "<p>Hi</p>", &[("printBackground", "true")])).unwrap();
        assert_eq!(
            body,
            "--B\r\nContent-Disposition: form-data; name=\"files\"; filename=\"index.html\"\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hi</p>\r\n\
             --B\r\nContent-Disposition: form-data; name=\"printBackground\"\r\n\r\ntrue\r\n--B--\r\n"
        );
    }
}
