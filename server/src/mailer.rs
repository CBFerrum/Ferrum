//! Bridges the kernel's `Mailer` trait to Loco's SMTP sender.

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
