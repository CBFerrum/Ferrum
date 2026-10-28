//! Bridges the kernel's `Mailer` and `FileStore` traits to Loco's SMTP sender and object storage.

use async_trait::async_trait;
use ferrum_kernel::{Error, Mailer, Result};
use loco_rs::mailer::{Email, EmailSender};

pub struct LocoMailer {
    pub sender: EmailSender,
    pub from: Option<String>,
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
