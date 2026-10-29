//! Bridges the kernel's `Mailer` and `FileStore` traits to Loco's SMTP sender and object storage.

use async_trait::async_trait;
use ferrum_kernel::{Error, MailAttachment, Mailer, Result};
use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::extension::ClientId;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use loco_rs::config::{MailerTls, SmtpMailer};
use loco_rs::mailer::{Email, EmailSender};

pub struct LocoMailer {
    pub sender: EmailSender,
    pub from: Option<String>,
    /// The same SMTP server, for mail with attachments (Loco's mailer has none); `None` without SMTP.
    pub smtp: Option<AsyncSmtpTransport<Tokio1Executor>>,
}

/// An SMTP transport for `config`, built as Loco builds its own.
pub fn smtp_transport(config: &SmtpMailer) -> std::result::Result<AsyncSmtpTransport<Tokio1Executor>, String> {
    let host = config.host.as_str();
    let mut b = match config.tls_mode() {
        MailerTls::Starttls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host).map_err(|e| e.to_string())?,
        MailerTls::Implicit => AsyncSmtpTransport::<Tokio1Executor>::relay(host).map_err(|e| e.to_string())?,
        MailerTls::None => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host),
    }
    .port(config.port);
    if let Some(auth) = &config.auth {
        b = b.credentials(Credentials::new(auth.user.clone(), auth.password.clone()));
    }
    if let Some(hello) = &config.hello_name {
        b = b.hello_name(ClientId::Domain(hello.clone()));
    }
    Ok(b.build())
}

#[async_trait]
impl Mailer for LocoMailer {
    async fn send(&self, to: &str, subject: &str, html: &str) -> Result<()> {
        if !to.contains('@') || to.len() > 254 {
            return Err(Error::invalid(format!("invalid recipient '{to}'")));
        }
        let email = Email {
            from: self.from.clone(),
            to: to.to_string(),
            subject: subject.chars().take(250).collect(),
            text: html.to_string(),
            html: html.to_string(),
            ..Default::default()
        };
        self.sender.mail(&email).await.map_err(|e| Error::Unavailable(format!("mail delivery failed: {e}")))
    }

    fn can_attach(&self) -> bool {
        self.smtp.is_some()
    }

    async fn send_with(&self, to: &str, subject: &str, html: &str, attachments: &[MailAttachment]) -> Result<()> {
        if attachments.is_empty() {
            return self.send(to, subject, html).await;
        }
        let smtp = self.smtp.as_ref().ok_or_else(|| Error::Unavailable("mail with attachments needs an SMTP server in the mailer settings".into()))?;
        let bad = |what: &str, e: &dyn std::fmt::Display| Error::invalid(format!("{what}: {e}"));
        let from: Mailbox = self.from.as_deref().unwrap_or(loco_rs::mailer::DEFAULT_FROM_SENDER).parse().map_err(|e| bad("sender", &e))?;
        let to: Mailbox = to.parse().map_err(|e| bad("recipient", &e))?;
        let mut body = MultiPart::mixed().multipart(MultiPart::alternative_plain_html(html.to_string(), html.to_string()));
        for a in attachments {
            let ctype = ContentType::parse(&a.content_type).unwrap_or(ContentType::parse("application/octet-stream").map_err(|e| bad("content type", &e))?);
            body = body.singlepart(Attachment::new(a.file_name.clone()).body(a.data.clone(), ctype));
        }
        let message =
            Message::builder().from(from).to(to).subject(subject.chars().take(250).collect::<String>()).multipart(body).map_err(|e| bad("message", &e))?;
        smtp.send(message).await.map(|_| ()).map_err(|e| Error::Unavailable(format!("mail delivery failed: {e}")))
    }
}

/// The server's object storage (S3-compatible or disk) for files packages store.
pub struct StorageFiles(pub std::sync::Arc<loco_rs::storage::Storage>);

#[async_trait]
impl ferrum_kernel::files::FileStore for StorageFiles {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        self.0.upload(std::path::Path::new(key), &bytes::Bytes::copy_from_slice(data)).await.map_err(|e| Error::Unavailable(format!("storage: {e}")))
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        self.0.download(std::path::Path::new(key)).await.map_err(|e| Error::Unavailable(format!("storage: {e}")))
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.0.delete(std::path::Path::new(key)).await.map_err(|e| Error::Unavailable(format!("storage: {e}")))
    }
}
