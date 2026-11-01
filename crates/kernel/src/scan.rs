//! Malware scanning of what people upload, through a ClamAV daemon (`FERRUM_CLAMAV_ADDR`).
//!
//! Every file passes here before it is stored: [`Kernel::store_file`] for what reaches the server,
//! [`Kernel::scan_stream`] for what browsers send straight to the bucket. What the scanner finds is
//! refused and recorded on the audit trail, and is never stored. With a scanner configured, one that
//! cannot answer refuses the upload too: a scanner being down must not be a way in. A file past the
//! scanner's size limit is stored and marked `unscanned`. That limit is told to us
//! (`FERRUM_CLAMAV_MAX_MB`) rather than found out: a scanner that hits it mid-stream closes the
//! connection, and the reset can destroy its answer, leaving an outage to tell it from.

use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::{Kernel, audit};
use futures::{Stream, StreamExt};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use uuid::Uuid;

/// How long one conversation with the scanner may take.
const SCAN_TIMEOUT: Duration = Duration::from_secs(120);
/// The most sent to the scanner in one frame of its protocol.
const FRAME: usize = 1 << 20;

/// What the scanner says of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Clean,
    Infected(String),
    /// Past the scanner's size limit: it did not look.
    TooLarge,
}

fn read_verdict(answer: &str) -> Result<Verdict> {
    let answer = answer.trim_end_matches(['\0', '\n', ' ']);
    if answer.ends_with("size limit exceeded. ERROR") {
        return Ok(Verdict::TooLarge);
    }
    let body = answer.strip_prefix("stream: ").unwrap_or(answer);
    if body == "OK" {
        return Ok(Verdict::Clean);
    }
    match body.strip_suffix(" FOUND") {
        Some(signature) => Ok(Verdict::Infected(signature.to_string())),
        None => Err(Error::Unavailable(format!("the malware scanner answered: {answer}"))),
    }
}

async fn send_to_scanner<S, B>(addr: &str, mut chunks: S) -> Result<Verdict>
where
    S: Stream<Item = Result<B>> + Unpin,
    B: AsRef<[u8]>,
{
    let down = |e: std::io::Error| Error::Unavailable(format!("the malware scanner cannot be reached: {e}"));
    let mut conn = TcpStream::connect(addr).await.map_err(down)?;
    conn.write_all(b"zINSTREAM\0").await.map_err(down)?;
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        for frame in chunk.as_ref().chunks(FRAME) {
            // A scanner past its limit answers and closes: that answer is the verdict.
            if conn.write_all(&(frame.len() as u32).to_be_bytes()).await.is_err() || conn.write_all(frame).await.is_err() {
                return answer(&mut conn).await;
            }
        }
    }
    let _ = conn.write_all(&[0u8; 4]).await;
    answer(&mut conn).await
}

async fn answer(conn: &mut TcpStream) -> Result<Verdict> {
    let mut out = Vec::new();
    conn.take(4096).read_to_end(&mut out).await.map_err(|e| Error::Unavailable(format!("the malware scanner did not answer: {e}")))?;
    read_verdict(&String::from_utf8_lossy(&out))
}

impl Kernel {
    /// Passes what `chunks` yields through the scanner, if there is one. What it finds is refused
    /// here, and put on the audit trail. Answers the file's `scan_status`.
    pub async fn scan_stream<S, B>(&self, ctx: &Ctx, file_name: &str, size: u64, chunks: S) -> Result<&'static str>
    where
        S: Stream<Item = Result<B>> + Unpin,
        B: AsRef<[u8]>,
    {
        let Some(addr) = &self.settings.clamd else { return Ok("unscanned") };
        if size > self.settings.clamd_max_bytes {
            tracing::warn!(file = file_name, size, "stored unscanned: past the malware scanner's size limit");
            return Ok("unscanned");
        }
        let verdict = tokio::time::timeout(SCAN_TIMEOUT, send_to_scanner(addr, chunks))
            .await
            .map_err(|_| Error::Unavailable("the malware scanner did not answer in time".into()))??;
        match verdict {
            Verdict::Clean => Ok("clean"),
            Verdict::TooLarge => {
                tracing::warn!(file = file_name, "stored unscanned: past the malware scanner's size limit");
                Ok("unscanned")
            }
            Verdict::Infected(signature) => {
                tracing::warn!(file = file_name, %signature, "upload refused: malware");
                let mut tx = self.db.tenant_tx(ctx.tenant).await?;
                audit::record(&mut tx, ctx, "File", Uuid::now_v7(), "malware_refused", None, json!({"file_name": file_name, "signature": signature})).await?;
                tx.commit().await?;
                Err(Error::Invalid(format!("{file_name} was not stored: the scanner found {signature} in it")))
            }
        }
    }

    /// Whether the scanner is there, if one is configured (`ferrum task doctor`).
    pub async fn scanner_ok(&self) -> Option<Result<()>> {
        let addr = self.settings.clamd.as_deref()?;
        Some(
            async {
                let mut conn = TcpStream::connect(addr).await.map_err(|e| Error::Unavailable(e.to_string()))?;
                conn.write_all(b"zPING\0").await.map_err(|e| Error::Unavailable(e.to_string()))?;
                let mut out = [0u8; 8];
                let n = conn.read(&mut out).await.map_err(|e| Error::Unavailable(e.to_string()))?;
                if out[..n].starts_with(b"PONG") { Ok(()) } else { Err(Error::Unavailable("it answered something other than PONG".into())) }
            }
            .await,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scanners_answers() {
        assert_eq!(read_verdict("stream: OK\0").unwrap(), Verdict::Clean);
        assert_eq!(read_verdict("stream: Win.Test.EICAR_HDB-1 FOUND\0").unwrap(), Verdict::Infected("Win.Test.EICAR_HDB-1".into()));
        assert_eq!(read_verdict("INSTREAM size limit exceeded. ERROR\0").unwrap(), Verdict::TooLarge);
        assert!(matches!(read_verdict("stream: lstat() failed. ERROR\0"), Err(Error::Unavailable(_))), "anything else is not a pass");
        assert!(matches!(read_verdict(""), Err(Error::Unavailable(_))), "nor is silence");
    }
}
