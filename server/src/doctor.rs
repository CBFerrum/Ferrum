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
    out.push(if !st.secret_key_command.trim().is_empty() {
        Finding::ok("secrets.seal", "the secret key comes from FERRUM_SECRET_KEY_COMMAND")
    } else if st.secret_key.trim().is_empty() {
        blocker_in_production(
            "secrets.seal",
            "FERRUM_SECRET_KEY is not set",
            "Set it to `openssl rand -base64 32`, and keep it: secret fields cannot be read without it.",
        )
    } else if ferrum_kernel::kms::is_wrapped(&st.secret_key) {
        Finding::ok("secrets.seal", "FERRUM_SECRET_KEY is wrapped by the key-management service (FERRUM_KMS_TRANSIT)")
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
    out.push(if st.kernel.holding().await? {
        Finding::blocker(
            "restore",
            "this database was restored and nothing that reaches outside is running",
            "Follow docs/operations.md, After restoring a backup, then: ferrum task release_restore note:\"what was checked\"",
        )
    } else {
        Finding::ok("restore", "no restore is waiting for anyone")
    });
    if st.kernel.db.fenced() {
        out.push(Finding::blocker("authority", "this region does not hold the right to write", "See docs/operations.md, Regions."));
    }
    // Work a newer build wrote, which this one leaves for it (docs/operations.md, Rolling back and the two-version window).
    let (events, jobs) = st.kernel.waiting_for_newer().await?;
    if events + jobs > 0 {
        out.push(Finding::warning(
            "window.waiting",
            format!("{events} events and {jobs} jobs wait for a newer build than this one"),
            "They run once a server at least as new as the one that wrote them runs again; roll forward when it is safe.",
        ));
    }

    out.push(match tokio::task::spawn_blocking(ferrum_kernel::sso::saml_self_check).await.ok().flatten() {
        None => Finding::ok("identity.saml", "built without SAML: single sign-on is OpenID Connect only"),
        Some(Ok(())) => Finding::ok("identity.saml", "SAML signatures are checked here (xmlsec1 signed and verified one)"),
        Some(Err(e)) => Finding::blocker(
            "identity.saml",
            format!("SAML cannot check signatures here: {e}"),
            "Install libxmlsec1 with its OpenSSL backend (libxmlsec1-openssl) beside the server, the versions it was built with.",
        ),
    });
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
    out.extend(service_findings(st).await);
    out.sort_by_key(|f| match f.level {
        Level::Blocker => 0,
        Level::Warning => 1,
        Level::Off => 2,
        Level::Ok => 3,
    });
    Ok(out)
}

/// The extensions a WOPI editor's discovery document offers an `edit` action for, and whether it
/// publishes a proof key (so that Ferrum can tell its calls from a stolen token's).
pub fn discovery_summary(xml: &str) -> (std::collections::BTreeSet<String>, bool) {
    let attr = |tag: &str, name: &str| -> Option<String> {
        let start = tag.find(&format!("{name}=\""))? + name.len() + 2;
        Some(tag[start..].split('"').next()?.to_ascii_lowercase())
    };
    let mut edit = std::collections::BTreeSet::new();
    let mut proof = false;
    for tag in xml.split('<') {
        if tag.starts_with("proof-key ") {
            proof = true;
        } else if tag.starts_with("action ") && attr(tag, "name").as_deref() == Some("edit") {
            edit.extend(attr(tag, "ext"));
        }
    }
    (edit, proof)
}

/// The types Drive opens in the online editor: what a person creates from its "New" menu.
pub const OFFICE_TYPES: [&str; 3] = ["docx", "xlsx", "pptx"];

/// What to say about the online editor, from its settings and what its discovery document gave
/// (`Err`: why it could not be read). No address or token is ever part of a finding.
pub fn office_findings(url: &str, wopi: &str, discovery: Result<String, String>) -> Vec<Finding> {
    let (url, wopi) = (url.trim(), wopi.trim());
    if url.is_empty() && wopi.is_empty() {
        return vec![Finding::off("office", "online editing is off: Word, Excel and PowerPoint files cannot be opened in Drive")];
    }
    if url.is_empty() || wopi.is_empty() {
        return vec![Finding::warning(
            "office",
            "online editing is half set up: it needs both FERRUM_OFFICE_URL and FERRUM_OFFICE_WOPI_URL",
            "Set both, or neither (docs/operations.md, Online editing).",
        )];
    }
    let xml = match discovery {
        Ok(xml) => xml,
        Err(why) => {
            return vec![Finding::warning(
                "office",
                format!("the online editor cannot be reached ({why}): Word, Excel and PowerPoint files do not open in Drive"),
                "Check that it runs and that FERRUM_OFFICE_URL names it (docs/operations.md, Online editing).",
            )];
        }
    };
    let (edit, proof) = discovery_summary(&xml);
    let missing: Vec<&str> = OFFICE_TYPES.into_iter().filter(|t| !edit.contains(*t)).collect();
    let mut out = vec![if edit.is_empty() {
        Finding::warning("office", "the online editor answers but lists no editors", "Check its image and its logs (docs/operations.md, Online editing).")
    } else if missing.is_empty() {
        Finding::ok("office", "the online editor answers and opens docx, xlsx and pptx files")
    } else {
        Finding::warning(
            "office",
            format!("the online editor answers but cannot edit: {}", missing.join(", ")),
            "Check its image and its logs (docs/operations.md, Online editing).",
        )
    }];
    out.push(if proof {
        Finding::ok("office.proof", "the online editor signs its calls (WOPI proof key)")
    } else {
        Finding::warning(
            "office.proof",
            "the online editor publishes no proof key: its calls to Ferrum are not signed",
            "Give it one: docs/operations.md, Online editing, Proof key.",
        )
    });
    out
}

/// What to say about the PDF renderer, from its address and what its health check gave.
pub fn pdf_finding(url: &str, health: Option<Result<u16, ()>>) -> Finding {
    match (url.trim().is_empty(), health) {
        (true, _) | (_, None) => Finding::off("pdf", "no PDF renderer: documents print as HTML only"),
        (_, Some(Ok(200..=299))) => Finding::ok("pdf", "the PDF renderer answers"),
        (_, Some(Ok(code))) => Finding::warning("pdf", format!("the PDF renderer answered {code}"), "Check it: printing to PDF fails until it answers."),
        (_, Some(Err(()))) => Finding::warning("pdf", "the PDF renderer cannot be reached", "Check FERRUM_PDF_URL: printing to PDF fails until it answers."),
    }
}

/// The services Ferrum depends on for what people do in Drive and print: the online editor and the
/// PDF renderer, each configured and reachable or not. `ferrum task doctor` and
/// `GET /api/admin/services` say it.
pub async fn service_findings(st: &AppState) -> Vec<Finding> {
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build().unwrap_or_default();
    let var = |name: &str| std::env::var(name).unwrap_or_default().trim().trim_end_matches('/').to_string();
    let (office, wopi) = (var("FERRUM_OFFICE_URL"), var("FERRUM_OFFICE_WOPI_URL"));
    let discovery = if office.is_empty() || wopi.is_empty() {
        Err(String::new())
    } else {
        match client.get(format!("{office}/hosting/discovery")).send().await {
            Ok(r) if r.status().is_success() => r.text().await.map_err(|_| "its answer could not be read".to_string()),
            Ok(r) => Err(format!("it answered {}", r.status().as_u16())),
            Err(e) if e.is_timeout() => Err("it did not answer in time".into()),
            Err(_) => Err("connection failed".into()),
        }
    };
    let mut out = office_findings(&office, &wopi, discovery);
    let pdf = st.settings.pdf_url.trim().trim_end_matches('/');
    let health = if pdf.is_empty() { None } else { Some(client.get(format!("{pdf}/health")).send().await.map(|r| r.status().as_u16()).map_err(|_| ())) };
    out.push(pdf_finding(pdf, health));
    out
}

/// The report as a person reads it.
pub fn render(findings: &[Finding]) -> String {
    let mut out = String::new();
    for f in findings {
        let level = match f.level {
            Level::Blocker => "BLOCKER",
            Level::Warning => "warning",
            Level::Off => "off",
            Level::Ok => "ok",
        };
        out.push_str(&format!("{level:8} {:24} {}\n", f.check, f.detail));
        if let Some(fix) = f.fix {
            out.push_str(&format!("{:33} -> {fix}\n", ""));
        }
    }
    let count = |l: Level| findings.iter().filter(|f| f.level == l).count();
    out.push_str(&format!(
        "\n{} blockers, {} warnings, {} switched off, {} fine\n",
        count(Level::Blocker),
        count(Level::Warning),
        count(Level::Off),
        count(Level::Ok)
    ));
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

    const COLLABORA: &str = r#"<wopi-discovery><net-zone name="external-http">
        <app name="writer"><action default="true" ext="docx" name="edit" urlsrc="http://x/cool.html?"/><action ext="docx" name="view" urlsrc="http://x/cool.html?"/></app>
        <app name="calc"><action ext="xlsx" name="edit" urlsrc="http://x/cool.html?"/></app>
        <app name="impress"><action ext="pptx" name="view" urlsrc="http://x/cool.html?"/></app></net-zone></wopi-discovery>"#;

    #[test]
    fn discovery_says_what_can_be_edited_and_whether_calls_are_signed() {
        let (edit, proof) = discovery_summary(COLLABORA);
        assert_eq!(edit.iter().map(String::as_str).collect::<Vec<_>>(), ["docx", "xlsx"], "pptx is only viewable");
        assert!(!proof);
        let (_, proof) = discovery_summary(&COLLABORA.replace("<net-zone", r#"<proof-key value="x" modulus="AQ==" exponent="AQAB"/><net-zone"#));
        assert!(proof);
    }

    #[test]
    fn the_editor_report_names_what_is_wrong_and_holds_no_address() {
        let secret = "https://office.internal.example:9980";
        let lv = |f: &[Finding], check: &str| f.iter().find(|f| f.check == check).map(|f| f.level);
        let off = office_findings("", "", Err(String::new()));
        assert_eq!(lv(&off, "office"), Some(Level::Off), "switched off on purpose: information, not a fault");
        assert!(off[0].detail.contains("is off") && off[0].fix.is_none(), "{off:?}");
        assert_eq!(lv(&office_findings(secret, "", Err(String::new())), "office"), Some(Level::Warning), "half set up needs attention");
        assert!(office_findings(secret, "", Err(String::new()))[0].detail.contains("half set up"));
        let down = office_findings(secret, "http://ferrum:5150", Err("connection failed".into()));
        assert_eq!((down.len(), lv(&down, "office")), (1, Some(Level::Warning)));
        assert!(down[0].detail.contains("cannot be reached") && !render(&down).contains("internal.example"), "{down:?}");
        let partial = office_findings(secret, "http://ferrum:5150", Ok(COLLABORA.into()));
        assert!(partial[0].detail.contains("pptx") && !partial[0].detail.contains("docx"), "{partial:?}");
        assert_eq!(lv(&partial, "office.proof"), Some(Level::Warning));
        let full = COLLABORA.replace("</net-zone>", r#"<action ext="pptx" name="edit" urlsrc="http://x/cool.html?"/></net-zone>"#);
        let good = office_findings(secret, "http://ferrum:5150", Ok(full));
        assert_eq!(lv(&good, "office"), Some(Level::Ok));
    }

    #[test]
    fn the_pdf_renderer_report() {
        assert_eq!((pdf_finding("", None).level, pdf_finding("", None).fix), (Level::Off, None));
        assert_eq!(pdf_finding("http://pdf:3000", Some(Ok(200))).level, Level::Ok);
        assert!(pdf_finding("http://pdf:3000", Some(Ok(503))).detail.contains("503"));
        assert!(pdf_finding("http://pdf:3000", Some(Err(()))).detail.contains("cannot be reached"));
    }

    #[test]
    fn it_counts_what_stands_in_the_way() {
        let text = render(&[Finding::blocker("a", "b", "c"), Finding::off("f", "g"), Finding::ok("d", "e")]);
        assert!(text.contains("BLOCKER") && text.contains("-> c") && text.contains("1 blockers, 0 warnings, 1 switched off, 1 fine"), "{text}");
        assert!(text.contains("off      f"), "{text}");
    }
}
