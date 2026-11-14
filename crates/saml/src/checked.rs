//! The calls, with xmlsec1 checking signatures (through samael).

use crate::people::person;
use crate::xmlenc::{self, ASSERTION, PROTOCOL};
use crate::{NameId, Person, Sp, SpKey};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use samael::crypto::{AllowedSignatureAlgorithm, CertificateDer, Crypto, CryptoProvider, ReduceMode};
use samael::idp::{CertificateParams, IdentityProvider, KeyType, Rsa};
use samael::key_info::{KeyInfo, X509Data};
use samael::metadata::{
    EncryptionMethod, Endpoint, EntityDescriptor, HTTP_POST_BINDING, HTTP_REDIRECT_BINDING, IndexedEndpoint, KeyDescriptor, SpSsoDescriptor,
};
use samael::schema::Response;
use samael::service_provider::ServiceProvider;
use samael::traits::ToXml;

/// The most metadata or a response may hold.
const MAX_METADATA: usize = 1 << 20;
pub(crate) const MAX_RESPONSE: usize = 512 << 10;
const SUCCESS: &str = "urn:oasis:names:tc:SAML:2.0:status:Success";
/// No SHA-1, and nothing else old enough to be forged.
pub(crate) const ALGORITHMS: [AllowedSignatureAlgorithm; 6] = [
    AllowedSignatureAlgorithm::RsaSha256,
    AllowedSignatureAlgorithm::RsaSha384,
    AllowedSignatureAlgorithm::RsaSha512,
    AllowedSignatureAlgorithm::EcdsaSha256,
    AllowedSignatureAlgorithm::EcdsaSha384,
    AllowedSignatureAlgorithm::EcdsaSha512,
];

/// An identity provider, as its metadata describes it.
#[derive(Debug, Clone)]
pub struct Idp {
    metadata: EntityDescriptor,
    pub entity_id: String,
    /// Where it takes sign-in requests (HTTP-Redirect binding).
    pub sso_url: String,
    /// Where it takes logout requests and answers, when it offers single logout: the binding
    /// (HTTP-Redirect preferred), the address, and the address for answers when another.
    pub slo: Option<(String, String, Option<String>)>,
}

pub(crate) fn private_key(key: &SpKey) -> Result<openssl::pkey::PKey<openssl::pkey::Private>, String> {
    openssl::pkey::PKey::private_key_from_der(&key.private_der).map_err(|e| format!("this service's SAML key cannot be read: {e}"))
}

fn service_provider(sp: &Sp, metadata: &EntityDescriptor) -> Result<ServiceProvider, String> {
    Ok(ServiceProvider {
        entity_id: Some(sp.entity_id.to_string()),
        acs_url: Some(sp.acs_url.to_string()),
        certificate: sp.key.map(|k| CertificateDer::from(k.certificate_der.clone())),
        metadata_url: None,
        slo_url: None,
        idp_metadata: metadata.clone(),
        allow_idp_initiated: false,
        max_clock_skew: chrono::Duration::seconds(120),
        allowed_signature_algorithms: Some(ALGORITHMS.to_vec()),
        ..ServiceProvider::default()
    })
}

/// A new key for a service: RSA 2048 and a certificate for ten years, named `name` (at most 64
/// characters; providers show it).
pub fn new_sp_key(name: &str) -> Result<SpKey, String> {
    let key = IdentityProvider::generate_new(KeyType::Rsa(Rsa::Rsa2048)).map_err(|e| format!("making a key: {e}"))?;
    let name: String = name.chars().take(64).collect();
    let cert = key
        .create_certificate(&CertificateParams { common_name: &name, issuer_name: &name, days_until_expiration: 3650 })
        .map_err(|e| format!("making a certificate: {e}"))?;
    Ok(SpKey { private_der: key.export_private_key_der().map_err(|e| e.to_string())?, certificate_der: cert.der_data().to_vec() })
}

impl Idp {
    /// An identity provider from its metadata: its entity ID, where it takes sign-in requests by
    /// HTTP-Redirect, and at least one certificate it signs with. Metadata past its `validUntil` is
    /// refused: it is stale, whatever else it says.
    pub fn read(xml: &str) -> Result<Idp, String> {
        if xml.len() > MAX_METADATA {
            return Err("the metadata is larger than a megabyte".into());
        }
        let metadata: EntityDescriptor = xml.trim().parse().map_err(|e| format!("that is not an identity provider's SAML metadata: {e}"))?;
        let entity_id = metadata.entity_id.clone().map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).ok_or("the metadata names no entityID")?;
        if metadata.idp_sso_descriptors.as_ref().is_none_or(Vec::is_empty) {
            return Err("the metadata describes no identity provider (no IDPSSODescriptor)".into());
        }
        if let Some(until) = metadata.valid_until
            && until < chrono::Utc::now()
        {
            return Err(format!("the metadata expired on {}; download it from the provider again", until.format("%Y-%m-%d")));
        }
        let sp = service_provider(&Sp { entity_id: "", acs_url: "", key: None, decrypt: &[], slo_url: None }, &metadata)?;
        let sso_url = sp.sso_binding_location(HTTP_REDIRECT_BINDING).ok_or("the metadata offers no sign-in service by HTTP-Redirect")?;
        if !sso_url.starts_with("https://") && !sso_url.starts_with("http://") {
            return Err("the metadata's sign-in service is not a web address".into());
        }
        match sp.idp_signing_certs() {
            Ok(Some(certs)) if !certs.is_empty() => {}
            Ok(_) => return Err("the metadata holds no signing certificate, so nothing the provider sends could be checked".into()),
            Err(e) => return Err(format!("a certificate in the metadata cannot be read: {e}")),
        }
        let services: Vec<&Endpoint> = metadata.idp_sso_descriptors.iter().flatten().flat_map(|d| &d.single_logout_services).collect();
        let slo = [HTTP_REDIRECT_BINDING, HTTP_POST_BINDING]
            .iter()
            .find_map(|b| services.iter().find(|e| e.binding == *b))
            .filter(|e| web_address(&e.location) && e.response_location.as_deref().is_none_or(web_address))
            .map(|e| (e.binding.clone(), e.location.trim().to_string(), e.response_location.as_deref().map(|r| r.trim().to_string())));
        Ok(Idp { metadata, entity_id, sso_url, slo })
    }

    /// The certificates it signs with.
    pub(crate) fn certs(&self) -> Result<Vec<CertificateDer>, String> {
        let sp = service_provider(&Sp { entity_id: "", acs_url: "", key: None, decrypt: &[], slo_url: None }, &self.metadata)?;
        sp.idp_signing_certs().map_err(|e| e.to_string())?.filter(|c| !c.is_empty()).ok_or_else(|| "the metadata holds no signing certificate".into())
    }
}

fn web_address(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("https://") || s.starts_with("http://")
}

/// This service's metadata, for an identity provider's administrator to load: its entity ID, where
/// responses go, with a key the certificate its requests are signed with, the keys assertions may be
/// encrypted to (with the algorithms it takes), and where logout messages go.
pub fn sp_metadata(sp: &Sp) -> Result<String, String> {
    let descriptor = |use_: &str, k: &SpKey, methods: Option<Vec<EncryptionMethod>>| KeyDescriptor {
        key_use: Some(use_.into()),
        key_info: KeyInfo { id: None, x509_data: Some(X509Data { certificates: vec![STANDARD.encode(&k.certificate_der)] }) },
        encryption_methods: methods,
    };
    let methods = || [xmlenc::GCM[2], xmlenc::GCM[0], xmlenc::OAEP[0], xmlenc::OAEP[1]].map(|a| EncryptionMethod { algorithm: a.into() }).to_vec();
    let keys: Vec<KeyDescriptor> =
        sp.key.map(|k| descriptor("signing", k, None)).into_iter().chain(sp.decrypt.iter().map(|k| descriptor("encryption", k, Some(methods())))).collect();
    let logout = sp
        .slo_url
        .map(|slo| [HTTP_REDIRECT_BINDING, HTTP_POST_BINDING].map(|b| Endpoint { binding: b.into(), location: slo.into(), response_location: None }).to_vec());
    let descriptor = EntityDescriptor {
        entity_id: Some(sp.entity_id.to_string()),
        sp_sso_descriptors: Some(vec![SpSsoDescriptor {
            protocol_support_enumeration: Some("urn:oasis:names:tc:SAML:2.0:protocol".into()),
            key_descriptors: Some(keys),
            single_logout_services: logout,
            authn_requests_signed: Some(sp.key.is_some()),
            want_assertions_signed: Some(true),
            name_id_formats: Some(vec![
                "urn:oasis:names:tc:SAML:2.0:nameid-format:persistent".into(),
                "urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress".into(),
            ]),
            assertion_consumer_services: vec![IndexedEndpoint {
                binding: HTTP_POST_BINDING.into(),
                location: sp.acs_url.into(),
                response_location: None,
                index: 0,
                is_default: Some(true),
            }],
            ..SpSsoDescriptor::default()
        }]),
        ..EntityDescriptor::default()
    };
    descriptor.to_string().map_err(|e| e.to_string())
}

/// Where to send the browser to sign in: the provider's sign-in service with a request (`request_id`,
/// an XML ID: a letter or `_` first) and `relay_state`, which the provider posts back unchanged;
/// signed (RSA-SHA256, the HTTP-Redirect binding's way) when this service has a key.
pub fn request_url(sp: &Sp, idp: &Idp, request_id: &str, relay_state: &str) -> Result<String, String> {
    let provider = service_provider(sp, &idp.metadata)?;
    let mut request = provider.make_authentication_request(&idp.sso_url).map_err(|e| e.to_string())?;
    request.id = request_id.to_string();
    request.force_authn = None;
    let url = match sp.key {
        Some(key) => request.signed_redirect(relay_state, &private_key(key)?),
        None => request.redirect(relay_state),
    };
    Ok(url.map_err(|e| e.to_string())?.ok_or("the request has no destination")?.to_string())
}

/// Checks a response the provider posted (`SAMLResponse`, base64) to the request `request_id`: signed
/// by a certificate in the provider's metadata with an accepted algorithm, from the provider, for
/// this service and its address, in answer to that request, in its time window, with a bearer
/// confirmation. Anything outside what the signature covers is not read.
///
/// An encrypted assertion is opened first (xmlenc.rs) and then checked as a plain one is. What must
/// be signed: the response, covering the encrypted assertion, checked before anything is decrypted
/// (and then everything opened from it counts); or else the assertion itself, inside the encryption,
/// checked once opened. A response that carries a signature must have a good one over all of it:
/// one covering less, or none that checks, refuses the whole response, never falling back to the
/// assertion's.
pub fn verify(sp: &Sp, idp: &Idp, response: &str, request_id: &str) -> Result<Person, String> {
    if response.len() > MAX_RESPONSE {
        return Err("the response is too large".into());
    }
    let bytes = STANDARD.decode(response.trim().replace(['\r', '\n'], "")).map_err(|_| "the response is not base64")?;
    let xml = String::from_utf8(bytes).map_err(|_| "the response is not text")?;
    // A refusal usually comes unsigned: say what it is, rather than only that its signature failed.
    if let Ok(r) = xml.parse::<Response>()
        && let Some(code) = r.status.as_ref().and_then(|s| s.status_code.value.as_deref())
        && code != SUCCESS
    {
        return Err(format!("the provider did not sign you in ({})", code.rsplit(':').next().unwrap_or(code)));
    }
    let encrypted = if xml.contains("EncryptedAssertion") { open_encrypted(sp, idp, &xml)? } else { None };
    let assertion = if let Some((opened, response_signed)) = encrypted {
        let checker = if response_signed {
            // Checked already, all of it: what is left is what the signature covered, opened.
            let mut metadata = idp.metadata.clone();
            metadata.idp_sso_descriptors.iter_mut().flatten().for_each(|d| d.key_descriptors.clear());
            service_provider(sp, &metadata)?
        } else {
            service_provider(sp, &idp.metadata)?
        };
        checker.parse_xml_response(&opened, Some(&[request_id])).map_err(|e| e.to_string())?
    } else {
        service_provider(sp, &idp.metadata)?.parse_xml_response(&xml, Some(&[request_id])).map_err(|e| e.to_string())?
    };
    // The profile requires an audience naming this service; samael checks one only when present.
    let addressed = assertion
        .conditions
        .as_ref()
        .and_then(|c| c.audience_restrictions.as_ref())
        .is_some_and(|all| !all.is_empty() && all.iter().all(|r| r.audience.iter().any(|a| a.trim() == sp.entity_id)));
    if !addressed {
        return Err("the assertion is not addressed to this service (no audience restriction names it)".into());
    }
    let name_id = assertion.subject.as_ref().and_then(|s| s.name_id.as_ref()).map(|n| (n.value.as_str(), n.format.as_deref()));
    let attributes: Vec<_> = assertion
        .attribute_statements
        .iter()
        .flatten()
        .flat_map(|s| &s.attributes)
        .map(|a| (a.name.as_deref(), a.friendly_name.as_deref(), a.values.iter().filter_map(|v| v.value.as_deref()).collect()))
        .collect();
    let mut p = person(name_id, &attributes)?;
    p.name_id = name_id.map(|(value, format)| NameId { value: value.trim().to_string(), format: format.map(|f| f.trim().to_string()) });
    p.session_index = assertion.authn_statements.iter().flatten().find_map(|s| s.session_index.clone()).filter(|s| !s.is_empty() && s.len() <= 255);
    Ok(p)
}

/// A response whose assertion is encrypted, with the assertion opened → it, and whether a signature
/// over the whole response was checked (or else the opened assertion's own must be); `None` when its
/// assertion is not encrypted. See [`verify`].
fn open_encrypted(sp: &Sp, idp: &Idp, xml: &str) -> Result<Option<(String, bool)>, String> {
    use libxml::parser::{Parser, ParserOptions};
    let doc = Parser::default()
        .parse_string_with_options(xml, ParserOptions { recover: false, no_net: true, ..ParserOptions::default() })
        .map_err(|_| "the response is not XML")?;
    let root = doc.get_root_element().filter(|r| xmlenc::is(r, PROTOCOL, "Response")).ok_or("the response is not a SAML response")?;
    let signed = root.get_child_elements().iter().any(|c| xmlenc::is(c, "http://www.w3.org/2000/09/xmldsig#", "Signature"));
    if !root.get_child_elements().iter().any(|c| xmlenc::is(c, ASSERTION, "EncryptedAssertion")) {
        return Ok(None);
    }
    if !signed {
        return Ok(Some((xmlenc::open(xml, sp.decrypt, false)?, false)));
    }
    // Only what the signature covers is kept; `open` then finds the response at the root, holding the
    // encrypted assertion, or refuses: a signature over less than that proves nothing about it.
    let covered = Crypto::reduce_xml_to_signed_with_allowed_algorithms(xml, &idp.certs()?, ReduceMode::default(), Some(&ALGORITHMS))
        .map_err(|_| "the response's signature is not valid")?;
    Ok(Some((xmlenc::open(&covered, sp.decrypt, true)?, true)))
}

/// Whether xmlsec1 works on this machine: a key made now signs a response, the signature verifies,
/// and a changed response does not. A server whose crypto backend cannot load fails this, rather
/// than a person's sign-in.
pub fn self_check() -> Result<(), String> {
    use samael::idp::response_builder::build_response_template;
    use samael::signature::{DigestAlgorithm, Signature};
    let key = IdentityProvider::generate_new(KeyType::Rsa(Rsa::Rsa2048)).map_err(|e| format!("making a key: {e}"))?;
    let cert = key
        .create_certificate(&CertificateParams { common_name: "ferrum-self-check", issuer_name: "ferrum-self-check", days_until_expiration: 1 })
        .map_err(|e| format!("making a certificate: {e}"))?;
    let mut r = build_response_template(
        &cert,
        "self-check",
        "https://ferrum.invalid/sp",
        "https://ferrum.invalid/idp",
        "https://ferrum.invalid/acs",
        "_self_check",
        &[],
    );
    let mut template = Signature::template(&r.id, &cert);
    template.signed_info.reference[0].digest_method.algorithm = DigestAlgorithm::Sha256;
    r.signature = Some(template);
    let der = key.export_private_key_der().map_err(|e| e.to_string())?;
    let signed = Crypto::sign_xml(r.to_string().map_err(|e| e.to_string())?, &der).map_err(|e| format!("signing: {e}"))?;
    let check = |xml: &str| Crypto::reduce_xml_to_signed_with_allowed_algorithms(xml, std::slice::from_ref(&cert), ReduceMode::default(), Some(&ALGORITHMS));
    check(&signed).map_err(|e| format!("verifying: {e}"))?;
    match check(&signed.replace("self-check", "self-chuck")) {
        Ok(_) => Err("a changed response verified".into()),
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests;
