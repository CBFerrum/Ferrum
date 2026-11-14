//! Single logout (the SAML 2.0 profile's front channel): a logout request this service sends when
//! someone signs out here, the provider's answer, a request the provider sends when someone signs out
//! there, and this service's answer. Messages go by the browser, by HTTP-Redirect (signed in the
//! query, over the query exactly as it came) or HTTP-POST (signed in the XML, checked by xmlsec1 like
//! a response, keeping only what the signature covers). An incoming one must be signed by a
//! certificate in the provider's metadata (RSA with SHA-256 or better), from the provider, sent to
//! this service's logout address, and recent; its ID is the caller's to remember.

use crate::checked::{ALGORITHMS, Idp, MAX_RESPONSE, private_key};
use crate::xmlenc::{ASSERTION, PROTOCOL, is};
use crate::{Incoming, Logout, NameId, Outgoing, Sp};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use libxml::parser::{Parser, ParserOptions};
use openssl::hash::MessageDigest;
use samael::crypto::{Crypto, CryptoProvider, ReduceMode};
use samael::metadata::HTTP_REDIRECT_BINDING;
use std::io::{Read, Write};

const SUCCESS: &str = "urn:oasis:names:tc:SAML:2.0:status:Success";
const RESPONDER: &str = "urn:oasis:names:tc:SAML:2.0:status:Responder";
const RSA_SHA256: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256";
/// Signatures in a query: RSA, SHA-256 or better.
const QUERY_ALGORITHMS: [(&str, fn() -> MessageDigest); 3] = [
    (RSA_SHA256, MessageDigest::sha256),
    ("http://www.w3.org/2001/04/xmldsig-more#rsa-sha384", MessageDigest::sha384),
    ("http://www.w3.org/2001/04/xmldsig-more#rsa-sha512", MessageDigest::sha512),
];
/// How old a message may be, and how far ahead the provider's clock.
const MAX_AGE_SECS: i64 = 600;
const SKEW_SECS: i64 = 120;
const MAX_SESSION_INDEXES: usize = 20;

/// Refused, without saying which check: what comes is unsigned, or forged, or not for us.
const REFUSED: &str = "the logout message was refused";

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Where a message to the provider goes, by which binding: answers to its `ResponseLocation` when
/// it has one.
fn destination(idp: &Idp, answer: bool) -> Result<(bool, String), String> {
    let (binding, location, response_location) = idp.slo.as_ref().ok_or("the provider offers no single logout")?;
    let to = if answer { response_location.clone().unwrap_or_else(|| location.clone()) } else { location.clone() };
    Ok((binding == HTTP_REDIRECT_BINDING, to))
}

/// `xml` (whose root has `id`) signed by this service and put in the binding the provider takes.
fn send(sp: &Sp, idp: &Idp, answer: bool, xml: String, id: &str, relay_state: &str) -> Result<Outgoing, String> {
    let key = sp.key.ok_or("this service has no key to sign with")?;
    let (redirect, to) = destination(idp, answer)?;
    let param = if answer { "SAMLResponse" } else { "SAMLRequest" };
    if redirect {
        let mut deflated = DeflateEncoder::new(Vec::new(), Compression::default());
        deflated.write_all(xml.as_bytes()).map_err(|e| e.to_string())?;
        let message = STANDARD.encode(deflated.finish().map_err(|e| e.to_string())?);
        let mut query = format!("{param}={}", encode(&message));
        if !relay_state.is_empty() {
            query.push_str(&format!("&RelayState={}", encode(relay_state)));
        }
        query.push_str(&format!("&SigAlg={}", encode(RSA_SHA256)));
        let pkey = private_key(key)?;
        let mut signer = openssl::sign::Signer::new(MessageDigest::sha256(), &pkey).map_err(|e| e.to_string())?;
        signer.update(query.as_bytes()).map_err(|e| e.to_string())?;
        let signature = STANDARD.encode(signer.sign_to_vec().map_err(|e| e.to_string())?);
        let joiner = if to.contains('?') { '&' } else { '?' };
        return Ok(Outgoing { url: format!("{to}{joiner}{query}&Signature={}", encode(&signature)), fields: None });
    }
    // HTTP-POST: an enveloped signature after the Issuer, as the schema places it.
    let template = format!(
        r##"<ds:Signature xmlns:ds="http://www.w3.org/2000/09/xmldsig#"><ds:SignedInfo><ds:CanonicalizationMethod Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#"/><ds:SignatureMethod Algorithm="{RSA_SHA256}"/><ds:Reference URI="#{id}"><ds:Transforms><ds:Transform Algorithm="http://www.w3.org/2000/09/xmldsig#enveloped-signature"/><ds:Transform Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#"/></ds:Transforms><ds:DigestMethod Algorithm="http://www.w3.org/2001/04/xmlenc#sha256"/><ds:DigestValue/></ds:Reference></ds:SignedInfo><ds:SignatureValue/><ds:KeyInfo><ds:X509Data><ds:X509Certificate>{}</ds:X509Certificate></ds:X509Data></ds:KeyInfo></ds:Signature>"##,
        STANDARD.encode(&key.certificate_der)
    );
    let xml = xml.replacen("</saml:Issuer>", &format!("</saml:Issuer>{template}"), 1);
    let signed = Crypto::sign_xml(xml, &key.private_der).map_err(|e| format!("signing: {e}"))?;
    let mut fields = vec![(param.to_string(), STANDARD.encode(signed))];
    if !relay_state.is_empty() {
        fields.push(("RelayState".into(), relay_state.to_string()));
    }
    Ok(Outgoing { url: to, fields: Some(fields) })
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A logout request (`id`, an XML ID) for `name_id`, of the provider's session `session_index` when
/// known, signed by this service, to the provider's logout service.
pub fn logout_request(sp: &Sp, idp: &Idp, id: &str, name_id: &NameId, session_index: Option<&str>, relay_state: &str) -> Result<Outgoing, String> {
    let (_, to) = destination(idp, false)?;
    let format = name_id.format.as_deref().map(|f| format!(r#" Format="{}""#, xml_escape(f))).unwrap_or_default();
    let index = session_index.map(|s| format!("<samlp:SessionIndex>{}</samlp:SessionIndex>", xml_escape(s))).unwrap_or_default();
    let xml = format!(
        r#"<samlp:LogoutRequest xmlns:samlp="{PROTOCOL}" xmlns:saml="{ASSERTION}" ID="{id}" Version="2.0" IssueInstant="{}" Destination="{}"><saml:Issuer>{}</saml:Issuer><saml:NameID{format}>{}</saml:NameID>{index}</samlp:LogoutRequest>"#,
        now(),
        xml_escape(&to),
        xml_escape(sp.entity_id),
        xml_escape(&name_id.value)
    );
    send(sp, idp, false, xml, id, relay_state)
}

/// The answer (`id`) to the provider's logout request `in_response_to`: done, or not (`success`),
/// signed by this service.
pub fn logout_response(sp: &Sp, idp: &Idp, id: &str, in_response_to: &str, success: bool, relay_state: &str) -> Result<Outgoing, String> {
    let (_, to) = destination(idp, true)?;
    let xml = format!(
        r#"<samlp:LogoutResponse xmlns:samlp="{PROTOCOL}" xmlns:saml="{ASSERTION}" ID="{id}" Version="2.0" IssueInstant="{}" Destination="{}" InResponseTo="{}"><saml:Issuer>{}</saml:Issuer><samlp:Status><samlp:StatusCode Value="{}"/></samlp:Status></samlp:LogoutResponse>"#,
        now(),
        xml_escape(&to),
        xml_escape(in_response_to),
        xml_escape(sp.entity_id),
        if success { SUCCESS } else { RESPONDER }
    );
    send(sp, idp, true, xml, id, relay_state)
}

/// A message as it came: its XML, the relay state, and for HTTP-Redirect what was signed and the
/// signature.
struct Unpacked {
    xml: String,
    relay_state: Option<String>,
    query_signature: Option<(String, String, String)>,
}

fn unpack(incoming: Incoming) -> Result<Unpacked, String> {
    let refused = || REFUSED.to_string();
    match incoming {
        Incoming::Post { message, relay_state } => {
            if message.len() > MAX_RESPONSE {
                return Err(refused());
            }
            let bytes = STANDARD.decode(message.trim().replace(['\r', '\n'], "")).map_err(|_| refused())?;
            Ok(Unpacked { xml: String::from_utf8(bytes).map_err(|_| refused())?, relay_state: relay_state.map(str::to_string), query_signature: None })
        }
        Incoming::Redirect(query) => {
            if query.len() > MAX_RESPONSE {
                return Err(refused());
            }
            // The pairs as they came: the signature is over them, not over a new encoding.
            let mut raw: Vec<(&str, &str)> = Vec::new();
            for pair in query.split('&').filter(|p| !p.is_empty()) {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                if matches!(k, "SAMLRequest" | "SAMLResponse" | "RelayState" | "SigAlg" | "Signature") {
                    if raw.iter().any(|(seen, _)| *seen == k) {
                        return Err(refused());
                    }
                    raw.push((k, v));
                }
            }
            let get = |k: &str| raw.iter().find(|(seen, _)| *seen == k).map(|(_, v)| *v);
            let decoded = |v: &str| url::form_urlencoded::parse(format!("x={v}").as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default();
            let (param, message) = match (get("SAMLRequest"), get("SAMLResponse")) {
                (Some(m), None) => ("SAMLRequest", m),
                (None, Some(m)) => ("SAMLResponse", m),
                _ => return Err(refused()),
            };
            let (alg, signature) = (get("SigAlg").ok_or_else(refused)?, get("Signature").ok_or_else(refused)?);
            let mut signed = format!("{param}={message}");
            if let Some(r) = get("RelayState") {
                signed.push_str(&format!("&RelayState={r}"));
            }
            signed.push_str(&format!("&SigAlg={alg}"));
            let deflated = STANDARD.decode(decoded(message).replace(['\r', '\n', ' '], "")).map_err(|_| refused())?;
            let mut xml = String::new();
            DeflateDecoder::new(&deflated[..]).take(MAX_RESPONSE as u64 + 1).read_to_string(&mut xml).map_err(|_| refused())?;
            if xml.len() > MAX_RESPONSE {
                return Err(refused());
            }
            Ok(Unpacked { xml, relay_state: get("RelayState").map(decoded), query_signature: Some((signed, decoded(alg), decoded(signature))) })
        }
    }
}

fn parse(xml: &str) -> Result<libxml::tree::Document, String> {
    Parser::default()
        .parse_string_with_options(xml, ParserOptions { recover: false, no_net: true, ..ParserOptions::default() })
        .map_err(|_| REFUSED.to_string())
}

fn issuer_of(root: &libxml::tree::Node) -> Option<String> {
    root.get_child_elements().into_iter().find(|c| is(c, ASSERTION, "Issuer")).map(|i| i.get_content().trim().to_string())
}

/// Who says they sent a logout message, before anything is checked: only to find the provider
/// whose metadata then checks it ([`read_logout`]).
pub fn peek_issuer(incoming: Incoming) -> Result<String, String> {
    let doc = parse(&unpack(incoming)?.xml)?;
    let root = doc.get_root_element().ok_or(REFUSED)?;
    issuer_of(&root).filter(|i| !i.is_empty()).ok_or_else(|| REFUSED.into())
}

fn time(root: &libxml::tree::Node, name: &str) -> Result<Option<chrono::DateTime<chrono::Utc>>, String> {
    root.get_attribute(name).map(|t| chrono::DateTime::parse_from_rfc3339(t.trim()).map(|t| t.to_utc()).map_err(|_| REFUSED.to_string())).transpose()
}

/// A logout message from `idp` to `sp`, checked: signed by a certificate in the provider's metadata,
/// issued by it, sent to this service's logout address, issued in the last ten minutes (and not past
/// its `NotOnOrAfter`). Every refusal says the same.
pub fn read_logout(sp: &Sp, idp: &Idp, incoming: Incoming) -> Result<Logout, String> {
    let refused = || REFUSED.to_string();
    let m = unpack(incoming)?;
    let certs = idp.certs()?;
    let xml = match &m.query_signature {
        Some((signed, alg, signature)) => {
            let md = QUERY_ALGORITHMS.iter().find(|(a, _)| a == alg).map(|(_, md)| md()).ok_or_else(refused)?;
            let signature = STANDARD.decode(signature.replace(['\r', '\n', ' '], "")).map_err(|_| refused())?;
            let good = certs.iter().any(|c| {
                let Ok(key) = openssl::x509::X509::from_der(c.der_data()).and_then(|x| x.public_key()) else { return false };
                openssl::sign::Verifier::new(md, &key).and_then(|mut v| v.verify_oneshot(&signature, signed.as_bytes())).unwrap_or(false)
            });
            if !good {
                return Err(refused());
            }
            m.xml
        }
        // Only what the signature covers is read: the message itself must be what it signs.
        None => Crypto::reduce_xml_to_signed_with_allowed_algorithms(&m.xml, &certs, ReduceMode::default(), Some(&ALGORITHMS)).map_err(|_| refused())?,
    };
    let doc = parse(&xml)?;
    let root = doc.get_root_element().ok_or_else(refused)?;
    let request = is(&root, PROTOCOL, "LogoutRequest");
    if !(request || is(&root, PROTOCOL, "LogoutResponse")) || root.get_attribute("Version").as_deref() != Some("2.0") {
        return Err(refused());
    }
    let id = root.get_attribute("ID").filter(|i| !i.is_empty() && i.len() <= 255).ok_or_else(refused)?;
    let destination = root.get_attribute("Destination").ok_or_else(refused)?;
    if Some(destination.trim()) != sp.slo_url || issuer_of(&root).as_deref() != Some(idp.entity_id.as_str()) {
        return Err(refused());
    }
    let now = chrono::Utc::now();
    let issued = time(&root, "IssueInstant")?.ok_or_else(refused)?;
    let until = time(&root, "NotOnOrAfter")?;
    if issued > now + chrono::Duration::seconds(SKEW_SECS)
        || issued < now - chrono::Duration::seconds(MAX_AGE_SECS)
        || until.is_some_and(|u| u + chrono::Duration::seconds(SKEW_SECS) <= now)
    {
        return Err(refused());
    }
    let children = root.get_child_elements();
    if !request {
        let in_response_to = root.get_attribute("InResponseTo").filter(|i| !i.is_empty()).ok_or_else(refused)?;
        let code = children
            .iter()
            .find(|c| is(c, PROTOCOL, "Status"))
            .and_then(|s| s.get_child_elements().into_iter().find(|c| is(c, PROTOCOL, "StatusCode")))
            .and_then(|c| c.get_attribute("Value"));
        return Ok(Logout::Response { in_response_to, success: code.as_deref().map(str::trim) == Some(SUCCESS) });
    }
    // A NameID encrypted to us is not read: providers send it plain unless asked.
    let name_id = children.iter().find(|c| is(c, ASSERTION, "NameID")).ok_or_else(refused)?;
    let value = name_id.get_content().trim().to_string();
    if value.is_empty() || value.len() > 255 {
        return Err(refused());
    }
    let session_indexes: Vec<String> =
        children.iter().filter(|c| is(c, PROTOCOL, "SessionIndex")).map(|c| c.get_content().trim().to_string()).filter(|s| !s.is_empty()).collect();
    if session_indexes.len() > MAX_SESSION_INDEXES {
        return Err(refused());
    }
    let format = name_id.get_attribute("Format").map(|f| f.trim().to_string()).filter(|f| !f.is_empty());
    Ok(Logout::Request { id, name_id: NameId { value, format }, session_indexes, relay_state: m.relay_state })
}
