//! A direct connection to the Invoice Registration Portal's API — NIC's own, or a GSP's that passes
//! it through — for companies that turn it on (`in_irp_mode`). It answers the
//! `india.e_invoice.requested`, `india.e_invoice.cancel_requested` and `india.e_way_bill.requested`
//! events: it signs in (the credentials encrypted with the IRP's public key, a session key
//! returned under a fresh app key), sends the payload encrypted with the session key, and records
//! the IRP's answer on the invoice, or its refusal. An invoice the IRP already registered (a retry
//! after an answer was lost) is fetched by its IRN instead. When the IRP cannot be reached the
//! event is left to be retried.

use crate::util::{get, id, s};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Duration as Span, NaiveDateTime, TimeZone, Utc};
use dashmap::DashMap;
use ferrum_kernel::outbox::Event;
use ferrum_kernel::{Ctx, Doc, Error, EventSink, Kernel, Result};
use serde_json::{Value, json};
use std::time::Duration;
use uuid::Uuid;

const AUTH: &str = "/eivital/v1.04/auth";
const INVOICE: &str = "/eicore/v1.03/Invoice";
const BY_IRN: &str = "/eicore/v1.03/Invoice/irn/";
const CANCEL: &str = "/eicore/v1.03/Invoice/Cancel";
const EWB: &str = "/eiewb/v1.03/ewaybill";
/// NIC's error codes for an IRN already generated, and for a token it no longer knows.
const DUPLICATE: &str = "2150";
const STALE_TOKEN: &[&str] = &["1005", "1004"];

// ───────────────────────────── crypto ─────────────────────────────

/// AES-256-ECB with PKCS#7 padding, as the IRP uses for its session key and payloads.
pub fn ecb_encrypt(key: &[u8; 32], data: &[u8]) -> Vec<u8> {
    use aes::cipher::{Array, BlockCipherEncrypt, KeyInit};
    let cipher = aes::Aes256::new(&Array::from(*key));
    let pad = 16 - data.len() % 16;
    let mut out = data.to_vec();
    out.extend(std::iter::repeat_n(pad as u8, pad));
    for chunk in out.chunks_exact_mut(16) {
        let mut block = Array::from(<[u8; 16]>::try_from(&*chunk).expect("16 bytes"));
        cipher.encrypt_block(&mut block);
        chunk.copy_from_slice(&block);
    }
    out
}

pub fn ecb_decrypt(key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>> {
    use aes::cipher::{Array, BlockCipherDecrypt, KeyInit};
    let bad = || Error::Unavailable("the IRP's answer could not be decrypted".into());
    if data.is_empty() || !data.len().is_multiple_of(16) {
        return Err(bad());
    }
    let cipher = aes::Aes256::new(&Array::from(*key));
    let mut out = data.to_vec();
    for chunk in out.chunks_exact_mut(16) {
        let mut block = Array::from(<[u8; 16]>::try_from(&*chunk).expect("16 bytes"));
        cipher.decrypt_block(&mut block);
        chunk.copy_from_slice(&block);
    }
    let pad = *out.last().ok_or_else(bad)? as usize;
    if pad == 0 || pad > 16 || out[out.len() - pad..].iter().any(|b| *b as usize != pad) {
        return Err(bad());
    }
    out.truncate(out.len() - pad);
    Ok(out)
}

/// An RSA public key from PEM (`PUBLIC KEY` or `RSA PUBLIC KEY`).
pub fn public_key(pem: &str) -> Result<rsa::RsaPublicKey> {
    use rsa::pkcs1::DecodeRsaPublicKey;
    use rsa::pkcs8::DecodePublicKey;
    let pem = pem.trim();
    rsa::RsaPublicKey::from_public_key_pem(pem)
        .or_else(|_| rsa::RsaPublicKey::from_pkcs1_pem(pem))
        .map_err(|_| Error::invalid("the key is not an RSA public key in PEM form"))
}

/// Whether `jwt` (a signed invoice or QR code) carries the signature of the key in `pem`.
pub fn signed_by(pem: &str, jwt: &str) -> Result<bool> {
    use rsa::traits::PublicKeyParts;
    let key = public_key(pem)?;
    let b64url = |n: &rsa::BigUint| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(n.to_bytes_be());
    let decoding = jsonwebtoken::DecodingKey::from_rsa_components(&b64url(key.n()), &b64url(key.e())).map_err(|e| Error::invalid(format!("the key: {e}")))?;
    let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    v.required_spec_claims.clear();
    v.validate_exp = false;
    v.validate_aud = false;
    Ok(jsonwebtoken::decode::<Value>(jwt, &decoding, &v).is_ok())
}

/// NIC's keys for the QR codes and invoices its IRPs sign, as NIC publishes them
/// (nic_keys/README.md says where and when): each under its certificate's SHA-1 thumbprint, the
/// `kid` a signed token names; the one key NIC published without its certificate has none.
const NIC_KEYS: &[(&str, &str)] = &[
    ("CD01AC2B6E720CF0B9E048F1CCBC7509C766AF78", include_str!("nic_keys/CD01AC2B6E720CF0B9E048F1CCBC7509C766AF78.pem")),
    ("644956C46E4E947AAD72FD15DFDB01EB9A96A66B", include_str!("nic_keys/644956C46E4E947AAD72FD15DFDB01EB9A96A66B.pem")),
    ("4DE1544AE695BDC84EC7BC12F2F57F813F44E301", include_str!("nic_keys/4DE1544AE695BDC84EC7BC12F2F57F813F44E301.pem")),
    ("DEB2031A279F203BBDDF3538E77D4122ACB4D07D", include_str!("nic_keys/DEB2031A279F203BBDDF3538E77D4122ACB4D07D.pem")),
    ("B8D63ECB58EA5E6F4AAC3D524543B2246669B0F8", include_str!("nic_keys/B8D63ECB58EA5E6F4AAC3D524543B2246669B0F8.pem")),
    ("6C9E76A15241484CBBFC0CB81A5F3F0E5D489918", include_str!("nic_keys/6C9E76A15241484CBBFC0CB81A5F3F0E5D489918.pem")),
    ("247EBBCBE931277CD0BCCEB00CB021D98E886DAC", include_str!("nic_keys/247EBBCBE931277CD0BCCEB00CB021D98E886DAC.pem")),
    ("115F4426617A7938BE1BA06DBEE91A427584EDAB", include_str!("nic_keys/115F4426617A7938BE1BA06DBEE91A427584EDAB.pem")),
    ("EDC57DE1358B300BA9F79340A6696831F3C85047", include_str!("nic_keys/EDC57DE1358B300BA9F79340A6696831F3C85047.pem")),
    ("", include_str!("nic_keys/ProdPubKey3.pem")),
];

/// What NIC's keys say about a signed QR code or invoice.
#[derive(Debug, PartialEq)]
pub enum NicSignature {
    /// Signed by one of NIC's keys.
    Valid,
    /// It names a key of NIC's that did not sign it: forged or altered.
    Invalid,
    /// It claims NIC but names a key NIC had not published when this release was made (NIC
    /// rotates them), or none, and no published key signed it.
    UnknownKey(String),
    /// Not issued by NIC (`iss`): a private IRP's, which NIC's keys say nothing about.
    NotNic,
}

/// Checks a token that claims NIC (`iss`) against the key it names, or against every published
/// key when it names none of them (a valid signature under any of NIC's keys is NIC's).
pub fn nic_signature(jwt: &str) -> NicSignature {
    let part = |i: usize| {
        let raw = jwt.split('.').nth(i)?;
        serde_json::from_slice::<Value>(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(raw.trim_end_matches('=')).ok()?).ok()
    };
    let (Some(header), Some(payload)) = (part(0), part(1)) else { return NicSignature::NotNic };
    if payload["iss"].as_str() != Some("NIC") {
        return NicSignature::NotNic;
    }
    let kid = header["kid"].as_str().unwrap_or_default().trim().to_ascii_uppercase();
    let by = |pem: &str| signed_by(pem, jwt).unwrap_or(false);
    match NIC_KEYS.iter().find(|(k, _)| !kid.is_empty() && *k == kid) {
        Some((_, pem)) if by(pem) => NicSignature::Valid,
        Some(_) => NicSignature::Invalid,
        None if NIC_KEYS.iter().any(|(_, pem)| by(pem)) => NicSignature::Valid,
        None => NicSignature::UnknownKey(if kid.is_empty() { "none".into() } else { kid }),
    }
}

fn random_key() -> [u8; 32] {
    use rsa::rand_core::{OsRng, RngCore};
    let mut k = [0u8; 32];
    OsRng.fill_bytes(&mut k);
    k
}

// ───────────────────────────── the connection ─────────────────────────────

/// A company's connection settings.
struct Config {
    tenant: Uuid,
    company: Uuid,
    base: String,
    gstin: String,
    client_id: String,
    client_secret: String,
    username: String,
    password: String,
    public_key: String,
    extra: Vec<(String, String)>,
}

impl Config {
    /// Where a session belongs: another address or user (a GSP instead of NIC, production instead
    /// of the sandbox) signs in afresh rather than sending this one's token.
    fn endpoint(&self) -> String {
        format!("{}|{}|{}", self.base, self.client_id, self.username)
    }
}

#[derive(Clone)]
struct Session {
    endpoint: String,
    token: String,
    sek: [u8; 32],
    until: DateTime<Utc>,
}

/// What the IRP answered: the decrypted data, or its refusal.
enum Answer {
    Data(Value),
    Refused { code: String, message: String, info: Value },
}

/// Sessions per company and replica (an IRP session lasts six hours).
#[derive(Default)]
pub struct IrpConnector {
    sessions: DashMap<(Uuid, Uuid), Session>,
}

fn status_ok(v: &Value) -> bool {
    v["Status"].as_i64().or_else(|| v["Status"].as_str().and_then(|s| s.parse().ok())) == Some(1)
}

/// The IRP's refusal: its first error's code and message, and its further details.
fn refusal(v: &Value) -> (String, String, Value) {
    let first = &v["ErrorDetails"][0];
    let text = |k: &str| first[k].as_str().map(str::to_string).or_else(|| first[k].as_i64().map(|n| n.to_string())).unwrap_or_default();
    let message = Some(text("ErrorMessage")).filter(|m| !m.is_empty()).unwrap_or_else(|| "the IRP refused it without saying why".into());
    (text("ErrorCode"), message, v["InfoDtls"].clone())
}

/// An IRP time (`2026-10-30 14:05:00`, India time).
fn ist(t: &str) -> Option<DateTime<Utc>> {
    let naive = NaiveDateTime::parse_from_str(t.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    chrono::FixedOffset::east_opt(5 * 3600 + 1800)?.from_local_datetime(&naive).single().map(|t| t.with_timezone(&Utc))
}

impl IrpConnector {
    async fn config(k: &Kernel, tenant: Uuid, company: &Doc) -> Result<Option<Config>> {
        let mode = s(company, "in_irp_mode").unwrap_or("Off");
        if mode == "Off" {
            return Ok(None);
        }
        let cid = id(company, "id").ok_or_else(|| Error::internal("company without id"))?;
        // NIC hands out the API address (sandbox and production) when it registers the user.
        let base = s(company, "in_irp_url")
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(|u| u.trim_end_matches('/').to_string())
            .ok_or_else(|| Error::invalid("set the company's IRP API address: the one NIC gave at registration, or your GSP's"))?;
        let mut tx = k.db.tenant_tx(tenant).await?;
        let mut secret = async |field: &str| k.secret(&mut tx, tenant, "Company", cid, field).await;
        let client_secret = secret("in_irp_client_secret").await?;
        let password = secret("in_irp_password").await?;
        let extra = secret("in_irp_gsp_headers").await?;
        tx.commit().await?;
        let need = |v: Option<&str>, what: &str| {
            v.map(str::trim).filter(|v| !v.is_empty()).map(str::to_string).ok_or_else(|| Error::invalid(format!("set the company's IRP {what}")))
        };
        // Checked here, so a mistake is the request's failure rather than a send that never works.
        let mut gsp = vec![];
        if let Some(text) = extra.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
            let map = serde_json::from_str::<serde_json::Map<String, Value>>(text)
                .map_err(|_| Error::invalid("the GSP headers must be a JSON object of names and values"))?;
            for (name, value) in map {
                let value = match value {
                    Value::String(v) => v,
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => return Err(Error::invalid(format!("the GSP header {name} needs a text value"))),
                };
                // Names only in messages: the values are credentials.
                if reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err() || reqwest::header::HeaderValue::from_str(&value).is_err() {
                    return Err(Error::invalid(format!("the GSP header {name} is not a valid HTTP header")));
                }
                gsp.push((name.to_lowercase(), value));
            }
        }
        Ok(Some(Config {
            tenant,
            company: cid,
            base,
            gstin: need(s(company, "in_gstin"), "GSTIN")?,
            client_id: need(s(company, "in_irp_client_id"), "client ID")?,
            client_secret: need(client_secret.as_deref(), "client secret")?,
            username: need(s(company, "in_irp_username"), "API user name")?,
            password: need(password.as_deref(), "API password")?,
            public_key: need(s(company, "in_irp_public_key"), "public key (PEM)")?,
            extra: gsp,
        }))
    }

    async fn post(k: &Kernel, cfg: &Config, method: &str, path: &str, headers: &[(&str, &str)], body: Option<Value>) -> Result<Value> {
        let url = format!("{}{path}", cfg.base);
        let (client, parsed) = k.vetted_client(&url, k.settings.webhooks_allow_private, Duration::from_secs(30), "Ferrum-IRP/1").await?;
        let mut req = if method == "GET" { client.get(parsed) } else { client.post(parsed) };
        // A GSP header replaces the IRP header of the same name rather than doubling it.
        let ours = headers.iter().copied().filter(|(n, _)| !cfg.extra.iter().any(|(g, _)| g.eq_ignore_ascii_case(n)));
        for (name, value) in cfg.extra.iter().map(|(n, v)| (n.as_str(), v.as_str())).chain(ours) {
            req = req.header(name, value);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.map_err(|e| Error::Unavailable(format!("the IRP cannot be reached: {e}")))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| Error::Unavailable(format!("reading the IRP's answer: {e}")))?;
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(Error::Unavailable(format!("the IRP answered HTTP {status}")));
        }
        let v = match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.get("Status").is_some() || !status.is_client_error() => return Ok(v),
            Ok(v) => Some(v),
            Err(_) if status.is_client_error() => None,
            Err(_) => return Err(Error::Unavailable(format!("the IRP answered HTTP {status} without JSON"))),
        };
        // A wrong address, a refused credential, or a GSP's own refusal (not in NIC's form):
        // retrying would not help, so it is a refusal, with what the server said.
        let said = v.as_ref().and_then(|v| ["message", "Message", "error_description", "errorMessage", "error"].iter().find_map(|k| v[*k].as_str()));
        let message = said.map_or_else(|| format!("the IRP answered HTTP {status}"), |m| format!("the IRP answered HTTP {status}: {m}"));
        Ok(json!({"Status": 0, "ErrorDetails": [{"ErrorCode": status.as_u16().to_string(), "ErrorMessage": message}]}))
    }

    async fn sign_in(&self, k: &Kernel, cfg: &Config) -> Result<std::result::Result<Session, String>> {
        let app_key = random_key();
        let inner = json!({"UserName": cfg.username, "Password": cfg.password, "AppKey": B64.encode(app_key), "ForceRefreshAccessToken": false});
        let rsa = public_key(&cfg.public_key)?;
        let sealed = rsa
            .encrypt(&mut rsa::rand_core::OsRng, rsa::Pkcs1v15Encrypt, B64.encode(inner.to_string()).as_bytes())
            .map_err(|e| Error::invalid(format!("encrypting the sign-in: {e}")))?;
        let headers = [("client_id", cfg.client_id.as_str()), ("client_secret", cfg.client_secret.as_str()), ("gstin", cfg.gstin.as_str())];
        let v = Self::post(k, cfg, "POST", AUTH, &headers, Some(json!({"Data": B64.encode(sealed)}))).await?;
        if !status_ok(&v) {
            let (code, message, _) = refusal(&v);
            return Ok(Err(format!("the IRP refused the sign-in ({code}: {message})")));
        }
        let data = &v["Data"];
        let sek = B64.decode(data["Sek"].as_str().unwrap_or_default()).map_err(|_| Error::Unavailable("the IRP sent no session key".into()))?;
        let sek: [u8; 32] = ecb_decrypt(&app_key, &sek)?.try_into().map_err(|_| Error::Unavailable("the IRP's session key is not 32 bytes".into()))?;
        let until = data["TokenExpiry"].as_str().and_then(ist).unwrap_or_else(|| Utc::now() + Span::hours(6)) - Span::minutes(10);
        Ok(Ok(Session { endpoint: cfg.endpoint(), token: data["AuthToken"].as_str().unwrap_or_default().to_string(), sek, until }))
    }

    async fn session(&self, k: &Kernel, cfg: &Config) -> Result<std::result::Result<Session, String>> {
        if let Some(s) = self.sessions.get(&(cfg.tenant, cfg.company)).filter(|s| s.until > Utc::now() && s.endpoint == cfg.endpoint()) {
            return Ok(Ok(s.clone()));
        }
        let fresh = self.sign_in(k, cfg).await?;
        if let Ok(s) = &fresh {
            self.sessions.insert((cfg.tenant, cfg.company), s.clone());
        }
        Ok(fresh)
    }

    /// One encrypted call (signing in when needed, and once more when the IRP has forgotten the
    /// session); a sign-in refused by the IRP comes back as a refusal.
    async fn call(&self, k: &Kernel, cfg: &Config, method: &str, path: &str, payload: Option<&Value>) -> Result<Answer> {
        for attempt in 0..2 {
            let session = match self.session(k, cfg).await? {
                Ok(s) => s,
                Err(message) => return Ok(Answer::Refused { code: "auth".into(), message, info: Value::Null }),
            };
            let headers = [
                ("client_id", cfg.client_id.as_str()),
                ("client_secret", cfg.client_secret.as_str()),
                ("gstin", cfg.gstin.as_str()),
                ("user_name", cfg.username.as_str()),
                ("authtoken", session.token.as_str()),
            ];
            let body = payload.map(|p| json!({"Data": B64.encode(ecb_encrypt(&session.sek, p.to_string().as_bytes()))}));
            let v = Self::post(k, cfg, method, path, &headers, body).await?;
            if status_ok(&v) {
                let sealed = B64.decode(v["Data"].as_str().unwrap_or_default()).map_err(|_| Error::Unavailable("the IRP's answer has no data".into()))?;
                let plain = ecb_decrypt(&session.sek, &sealed)?;
                return serde_json::from_slice(&plain).map(Answer::Data).map_err(|_| Error::Unavailable("the IRP's answer is not JSON".into()));
            }
            let (code, message, info) = refusal(&v);
            if attempt == 0 && STALE_TOKEN.contains(&code.as_str()) {
                self.sessions.remove(&(cfg.tenant, cfg.company));
                continue;
            }
            return Ok(Answer::Refused { code, message, info });
        }
        Err(Error::Unavailable("the IRP keeps refusing fresh sessions".into()))
    }

    /// Runs a record action; when the answer itself is refused (it does not match the invoice),
    /// that is recorded as the request's failure.
    async fn record(k: &Kernel, ctx: &Ctx, invoice: Uuid, action: &str, params: Value, failure: &str) -> Result<()> {
        match k.run_action(ctx, "SalesInvoice", invoice, action, params).await {
            Ok(_) => Ok(()),
            Err(e @ (Error::Validation(_) | Error::Invalid(_) | Error::Conflict(_))) => {
                let message = match &e {
                    Error::Validation(f) => f.iter().map(|f| f.message.clone()).collect::<Vec<_>>().join("; "),
                    other => other.to_string(),
                };
                Self::fail(k, ctx, invoice, failure, &format!("the IRP's answer was not recorded: {message}")).await
            }
            Err(e) => Err(e),
        }
    }

    async fn fail(k: &Kernel, ctx: &Ctx, invoice: Uuid, failure: &str, message: &str) -> Result<()> {
        match k.run_action(ctx, "SalesInvoice", invoice, failure, json!({"message": message.chars().take(2000).collect::<String>()})).await {
            Ok(_) | Err(Error::Validation(_) | Error::Invalid(_) | Error::Conflict(_) | Error::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn generate(&self, k: &Kernel, ctx: &Ctx, cfg: &Config, invoice: Uuid, payload: &Value) -> Result<()> {
        let answer = match self.call(k, cfg, "POST", INVOICE, Some(payload)).await? {
            Answer::Refused { code, info, .. } if code == DUPLICATE => {
                // Registered already (an answer lost on the way): fetch it.
                let irn = info
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|i| i["InfCd"] == "DUPIRN")
                    .and_then(|i| i["Desc"]["Irn"].as_str())
                    .unwrap_or_default()
                    .to_string();
                if irn.is_empty() {
                    return Self::fail(k, ctx, invoice, "record_e_invoice_failure", "the IRP says the invoice is registered already but not under which IRN")
                        .await;
                }
                self.call(k, cfg, "GET", &format!("{BY_IRN}{irn}"), None).await?
            }
            other => other,
        };
        match answer {
            Answer::Data(d) => Self::record(k, ctx, invoice, "record_e_invoice", json!({"response": d.to_string()}), "record_e_invoice_failure").await,
            Answer::Refused { code, message, .. } => Self::fail(k, ctx, invoice, "record_e_invoice_failure", &format!("{code}: {message}")).await,
        }
    }

    async fn cancel(&self, k: &Kernel, ctx: &Ctx, cfg: &Config, invoice: Uuid, payload: &Value) -> Result<()> {
        match self.call(k, cfg, "POST", CANCEL, Some(payload)).await? {
            Answer::Data(d) => Self::record(k, ctx, invoice, "record_e_invoice_cancellation", d, "record_e_invoice_cancellation_failure").await,
            Answer::Refused { code, message, .. } => Self::fail(k, ctx, invoice, "record_e_invoice_cancellation_failure", &format!("{code}: {message}")).await,
        }
    }

    async fn e_way_bill(&self, k: &Kernel, ctx: &Ctx, cfg: &Config, invoice: Uuid, payload: &Value) -> Result<()> {
        if payload["by_irn"] != json!(true) {
            return Self::fail(
                k,
                ctx,
                invoice,
                "record_e_way_bill_failure",
                "the IRP makes e-way bills for e-invoices only: make the e-invoice first, or upload the e-way bill JSON on the e-way bill portal",
            )
            .await;
        }
        match self.call(k, cfg, "POST", EWB, Some(&payload["bill"])).await? {
            Answer::Data(d) => Self::record(k, ctx, invoice, "record_e_way_bill", d, "record_e_way_bill_failure").await,
            Answer::Refused { code, message, .. } => Self::fail(k, ctx, invoice, "record_e_way_bill_failure", &format!("{code}: {message}")).await,
        }
    }
}

#[async_trait]
impl EventSink for IrpConnector {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        let failure = match ev.topic.as_str() {
            "india.e_invoice.requested" => "record_e_invoice_failure",
            "india.e_invoice.cancel_requested" => "record_e_invoice_cancellation_failure",
            "india.e_way_bill.requested" => "record_e_way_bill_failure",
            _ => return Ok(()),
        };
        let Some(invoice) = ev.record_id else { return Ok(()) };
        let ctx = Ctx::system(ev.tenant_id, k.registry.schema(ev.tenant_id).await?);
        let Some(company) = ev.payload["company"].as_str().and_then(|c| c.parse::<Uuid>().ok()) else { return Ok(()) };
        let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
        let company = get(k, &ctx, &mut tx, "Company", company).await?;
        tx.commit().await?;
        let cfg = match Self::config(k, ev.tenant_id, &company).await {
            Ok(Some(c)) => c,
            // Not connected: a GSP plugin, or someone at the portal, answers instead.
            Ok(None) => return Ok(()),
            Err(Error::Invalid(m)) => return Self::fail(k, &ctx, invoice, failure, &m).await,
            Err(e) => return Err(e),
        };
        let payload = &ev.payload["payload"];
        match ev.topic.as_str() {
            "india.e_invoice.requested" => self.generate(k, &ctx, &cfg, invoice, payload).await,
            "india.e_invoice.cancel_requested" => self.cancel(k, &ctx, &cfg, invoice, payload).await,
            _ => self.e_way_bill(k, &ctx, &cfg, invoice, payload).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_crypto_round_trips() {
        let key = [7u8; 32];
        for len in [0, 1, 15, 16, 17, 1000] {
            let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let sealed = ecb_encrypt(&key, &data);
            assert_eq!(sealed.len() % 16, 0);
            assert_eq!(ecb_decrypt(&key, &sealed).unwrap(), data);
        }
        assert!(ecb_decrypt(&key, &[1u8; 15]).is_err(), "not whole blocks");
        assert!(ecb_decrypt(&[8u8; 32], &ecb_encrypt(&key, b"x")).is_err(), "the wrong key does not unpad");
        // NIST SP 800-38A F.1.5, ECB-AES256, first block.
        let k: [u8; 32] = hex::decode("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4").unwrap().try_into().unwrap();
        let block = hex::decode("6bc1bee22e409f96e93d7e117393172a").unwrap();
        assert_eq!(hex::encode(&ecb_encrypt(&k, &block)[..16]), "f3eed1bdb5d2a03c064b5a7e3db181f8");
        assert_eq!(ist("2026-10-30 14:05:00").unwrap().to_rfc3339(), "2026-10-30T08:35:00+00:00");
    }

    /// The signed QR code in NIC's own generate-IRN API documentation
    /// (einv-apisandbox.nic.in/version1.03/generate-irn.html), signed by NIC's key 115F4426….
    const NIC_SAMPLE_QR: &str = "eyJhbGciOiJSUzI1NiIsImtpZCI6IjExNUY0NDI2NjE3QTc5MzhCRTFCQTA2REJFRTkxQTQyNzU4NEVEQUIiLCJ0eXAiOiJKV1QiLCJ4NXQiOiJFVjlFSm1GNmVUaS1HNkJ0dnVrYVFuV0U3YXMifQ.eyJkYXRhIjoie1wiU2VsbGVyR3N0aW5cIjpcIjM3QVJaUFQ0Mzg0UTFNVFwiLFwiQnV5ZXJHc3RpblwiOlwiMTFBQUFDVDM5MDRGMVpaXCIsXCJEb2NOb1wiOlwiREQtMjAyMDA4MDQtOVwiLFwiRG9jVHlwXCI6XCJJTlZcIixcIkRvY0R0XCI6XCIwNC8wOC8yMDIwXCIsXCJUb3RJbnZWYWxcIjo3Nzc2LjMsXCJJdGVtQ250XCI6MyxcIk1haW5Ic25Db2RlXCI6XCIxMDAxXCIsXCJJcm5cIjpcIjExZjhlZjcwMWZlMjk0ZDRhMTRhYWQwYjEyNDU3ZTYyNzc1ZDBmZGM0MWEwYWNmMDViNzRmYmIyZGRjNDdhY2JcIn0iLCJpc3MiOiJOSUMifQ.fya8oD85f2_K8pDWSf8N94_T24O1lA9OPpIuUwk14el_r1lhL13OFxGkklhiewSMUom8DvO9JKu4jjz2l5farRTJhiBWJ43EtEky2SLzRhJf23JYW_6PyLErYL2RTzv2PlZ75eXIBZzPkxc2erCx61T50oHmExLgl1Q6HclvgiQUAVxysq1VFv96zEZVH8I0xDNqjdvqdtsW74ZHqzpV28kDIvuyV4Z5j3bR39GE6YKMetext_x3bJ4Wt4F1z3DOzfUjuKGdEjP0fTSwNg1RpiDoH4wcaMP7RJgtbQYXn4j3YoppCEw916AmbihiT2gSODPn04vhCbBecI7oOZvxpw";

    #[test]
    fn qr_codes_are_checked_against_nics_published_keys() {
        for (kid, pem) in NIC_KEYS {
            public_key(pem).unwrap_or_else(|e| panic!("NIC key {kid}: {e}"));
        }
        assert_eq!(nic_signature(NIC_SAMPLE_QR), NicSignature::Valid);
        let parts: Vec<&str> = NIC_SAMPLE_QR.split('.').collect();
        let b64 = |v: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
        let decode = |p: &str| serde_json::from_slice::<Value>(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(p).unwrap()).unwrap();
        // Another IRN under NIC's signature: altered.
        let mut payload = decode(parts[1]);
        payload["data"] = json!(payload["data"].as_str().unwrap().replace("\"Irn\":\"", "\"Irn\":\"0"));
        assert_eq!(nic_signature(&format!("{}.{}.{}", parts[0], b64(&payload), parts[2])), NicSignature::Invalid);
        // A key NIC never published, and no kid at all: nobody vouches for it.
        let mut header = decode(parts[0]);
        header["kid"] = json!("00000000000000000000000000000000000000AA");
        assert_eq!(nic_signature(&format!("{}.{}.c2ln", b64(&header), parts[1])), NicSignature::UnknownKey("00000000000000000000000000000000000000AA".into()));
        assert_eq!(nic_signature(&format!("e30.{}.c2ln", parts[1])), NicSignature::UnknownKey("none".into()));
        // A private IRP's code: not NIC's to judge.
        payload["iss"] = json!("Cygnet");
        assert_eq!(nic_signature(&format!("{}.{}.{}", parts[0], b64(&payload), parts[2])), NicSignature::NotNic);
        assert_eq!(nic_signature("not a token"), NicSignature::NotNic);
    }
}
