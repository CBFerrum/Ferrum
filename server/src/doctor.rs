//! `ferrum task doctor`: a redacted report on this deployment, blockers apart from warnings, each
//! with what to do. The kernel looks at the database and its queues; this adds what only the server
//! knows: settings, object storage, mail and the services it depends on.

use crate::state::AppState;
use ferrum_kernel::doctor::{Finding, Level};
use loco_rs::app::AppContext;
use loco_rs::environment::Environment;

/// The sort of value that means nobody set this one.
fn placeholder(secret: &str) -> bool {
    let s = secret.to_lowercase();
    s.is_empty() || ["change-me", "changeme", "change_me", "example", "replace", "secret", "password"].iter().any(|p| s.contains(p))
}

fn is_local(url: &str) -> bool {
    ["localhost", "127.0.0.1", "[::1]"].iter().any(|h| url.contains(h))
}

/// What can be said from settings alone.
pub fn settings_findings(production: bool, jwt_secret: &str, st: &crate::state::Settings, public_url: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    let blocker_in_production = |check, detail: &str, fix| if production { Finding::blocker(check, detail, fix) } else { Finding::warning(check, detail, fix) };
    out.push(if jwt_secret.len() < 32 || placeholder(jwt_secret) {
        blocker_in_production("secrets.jwt", "JWT_SECRET is short, or a placeholder", "Set it to `openssl rand -base64 48`. Changing it signs everyone out.")
    } else {
        Finding::ok("secrets.jwt", "JWT_SECRET is set")
    });
    out.push(if st.secret_key.trim().is_empty() {
        blocker_in_production(
            "secrets.seal",
            "FERRUM_SECRET_KEY is not set",
            "Set it to `openssl rand -base64 32`, and keep it: secret fields cannot be read without it.",
        )
    } else {
        Finding::ok("secrets.seal", "FERRUM_SECRET_KEY is set")
    });
    out.push(if st.metrics_token.trim().is_empty() {
        Finding::warning("secrets.metrics", "/metrics needs no token", "Set FERRUM_METRICS_TOKEN.")
    } else {
        Finding::ok("secrets.metrics", "/metrics needs a token")
    });
    if !st.bootstrap_token.trim().is_empty() {
        out.push(Finding::warning(
            "secrets.bootstrap",
            "anyone with FERRUM_BOOTSTRAP_TOKEN can create workspaces",
            "Unset it unless you are creating workspaces over HTTP.",
        ));
    }
    out.push(if public_url.starts_with("https://") {
        Finding::ok("network.public_url", "PUBLIC_URL is https")
    } else if is_local(public_url) {
        Finding::warning(
            "network.public_url",
            "PUBLIC_URL is a local address",
            "Set it to the address people open: links in mail and sign-in redirects use it.",
        )
    } else {
        blocker_in_production(
            "network.public_url",
            "PUBLIC_URL is not https",
            "Serve Ferrum over TLS and set PUBLIC_URL to the https address: session cookies and passkeys need it.",
        )
    });
    if st.webhooks_allow_private {
        out.push(blocker_in_production(
            "network.webhooks",
            "webhooks may reach private addresses",
            "Unset FERRUM_WEBHOOKS_ALLOW_PRIVATE: it lets a workspace's administrator reach your internal network.",
        ));
    }
    if st.sso_allow_private {
        out.push(blocker_in_production(
            "network.sso",
            "single sign-on may reach private addresses",
            "Unset FERRUM_SSO_ALLOW_PRIVATE unless your identity provider is on your own network.",
        ));
    }
    out
}

/// Everything, worst first.
pub async fn report(st: &AppState, ctx: &AppContext) -> Result<Vec<Finding>, ferrum_kernel::Error> {
    let production = ctx.environment == Environment::Production;
    let mut out = settings_findings(production, &st.jwt_secret, &st.settings, &ctx.config.server.host);
    out.extend(st.kernel.doctor(st.settings.db_max_connections).await?);

    out.push(match st.objects.probe().await {
        Ok(()) => Finding::ok("storage", "object storage takes a write, a read and a delete"),
        Err(e) => Finding::blocker("storage", format!("object storage refused a probe: {e}"), "Check FERRUM_S3_BUCKET, its region, endpoint and credentials."),
    });
    if production && st.settings.s3_bucket.trim().is_empty() {
        out.push(Finding::warning(
            "storage.replicas",
            "files are on this machine's disk",
            "Set FERRUM_S3_BUCKET before running a second replica: each would have its own files.",
        ));
    }
    out.push(match st.kernel.scanner_ok().await {
        None => Finding::warning("uploads.scanning", "uploads are not scanned for malware", "Set FERRUM_CLAMAV_ADDR (docs/operations.md, Malware scanning)."),
        Some(Ok(())) => Finding::ok("uploads.scanning", "the malware scanner answers"),
        Some(Err(e)) => {
            Finding::blocker("uploads.scanning", format!("the malware scanner does not answer, so uploads fail: {e}"), "Start it, or unset FERRUM_CLAMAV_ADDR.")
        }
    });
    out.push(if ctx.mailer.is_some() {
        Finding::ok("mail", "a mailer is configured (not tested: that would send mail)")
    } else {
        Finding::warning("mail", "no mailer is configured: invitations and password resets do not go out", "Set MAILER_* (docs/operations.md).")
    });
    let pdf = st.settings.pdf_url.trim();
    out.push(if pdf.is_empty() {
        Finding::warning("pdf", "no PDF renderer: documents print as HTML only", "Set FERRUM_PDF_URL.")
    } else {
        match reqwest::Client::new().get(format!("{}/health", pdf.trim_end_matches('/'))).timeout(std::time::Duration::from_secs(5)).send().await {
            Ok(r) if r.status().is_success() => Finding::ok("pdf", "the PDF renderer answers"),
            Ok(r) => Finding::warning("pdf", format!("the PDF renderer answered {}", r.status().as_u16()), "Check it: printing to PDF fails until it answers."),
            Err(_) => Finding::warning("pdf", "the PDF renderer cannot be reached", "Check FERRUM_PDF_URL: printing to PDF fails until it answers."),
        }
    });
    out.sort_by_key(|f| match f.level {
        Level::Blocker => 0,
        Level::Warning => 1,
        Level::Ok => 2,
    });
    Ok(out)
}

/// The report as a person reads it.
pub fn render(findings: &[Finding]) -> String {
    let mut out = String::new();
    for f in findings {
        let level = match f.level {
            Level::Blocker => "BLOCKER",
            Level::Warning => "warning",
            Level::Ok => "ok",
        };
        out.push_str(&format!("{level:8} {:24} {}\n", f.check, f.detail));
        if let Some(fix) = f.fix {
            out.push_str(&format!("{:33} -> {fix}\n", ""));
        }
    }
    let count = |l: Level| findings.iter().filter(|f| f.level == l).count();
    out.push_str(&format!("\n{} blockers, {} warnings, {} fine\n", count(Level::Blocker), count(Level::Warning), count(Level::Ok)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> crate::state::Settings {
        serde_json::from_value(serde_json::json!({"secret_key": "c2VjcmV0", "metrics_token": "t"})).unwrap()
    }

    fn level(findings: &[Finding], check: &str) -> Option<Level> {
        findings.iter().find(|f| f.check == check).map(|f| f.level)
    }

    #[test]
    fn unsafe_defaults_are_blockers_in_production_and_warnings_elsewhere() {
        let good = "x".repeat(40);
        let fine = settings_findings(true, &good, &settings(), "https://erp.example.com");
        assert!(fine.iter().all(|f| f.level == Level::Ok), "{fine:?}");
        let mut open = settings();
        open.webhooks_allow_private = true;
        open.secret_key = String::new();
        for (production, want) in [(true, Level::Blocker), (false, Level::Warning)] {
            let f = settings_findings(production, "change-me-change-me-change-me-change-me", &open, "http://erp.example.com");
            for check in ["secrets.jwt", "secrets.seal", "network.public_url", "network.webhooks"] {
                assert_eq!(level(&f, check), Some(want), "{check}");
            }
        }
        assert_eq!(level(&settings_findings(false, &good, &settings(), "http://localhost:5150"), "network.public_url"), Some(Level::Warning));
    }

    #[test]
    fn a_report_holds_no_secret() {
        let secret = "s3cr3t-value-that-must-not-print-anywhere";
        let mut s = settings();
        s.secret_key = secret.into();
        s.metrics_token = secret.into();
        s.bootstrap_token = secret.into();
        let text = render(&settings_findings(true, secret, &s, "https://erp.example.com"));
        assert!(!text.contains(secret), "{text}");
    }

    #[test]
    fn it_counts_what_stands_in_the_way() {
        let text = render(&[Finding::blocker("a", "b", "c"), Finding::ok("d", "e")]);
        assert!(text.contains("BLOCKER") && text.contains("-> c") && text.contains("1 blockers, 0 warnings, 1 fine"), "{text}");
    }
}
