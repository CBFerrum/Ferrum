//! The calls, with xmlsec1 checking signatures (through samael).

use crate::people::person;
use crate::{Person, Sp};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use samael::crypto::AllowedSignatureAlgorithm;
use samael::metadata::{EntityDescriptor, HTTP_POST_BINDING, HTTP_REDIRECT_BINDING, IndexedEndpoint, SpSsoDescriptor};
use samael::schema::Response;
use samael::service_provider::ServiceProvider;
use samael::traits::ToXml;

/// The most metadata or a response may hold.
const MAX_METADATA: usize = 1 << 20;
const MAX_RESPONSE: usize = 512 << 10;
const SUCCESS: &str = "urn:oasis:names:tc:SAML:2.0:status:Success";
/// No SHA-1, and nothing else old enough to be forged.
const ALGORITHMS: [AllowedSignatureAlgorithm; 6] = [
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
}

fn service_provider(sp: &Sp, metadata: &EntityDescriptor) -> ServiceProvider {
    ServiceProvider {
        entity_id: Some(sp.entity_id.to_string()),
        acs_url: Some(sp.acs_url.to_string()),
        metadata_url: None,
        slo_url: None,
        idp_metadata: metadata.clone(),
        allow_idp_initiated: false,
        max_clock_skew: chrono::Duration::seconds(120),
        allowed_signature_algorithms: Some(ALGORITHMS.to_vec()),
        ..ServiceProvider::default()
    }
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
        let sp = service_provider(&Sp { entity_id: "", acs_url: "" }, &metadata);
        let sso_url = sp.sso_binding_location(HTTP_REDIRECT_BINDING).ok_or("the metadata offers no sign-in service by HTTP-Redirect")?;
        if !sso_url.starts_with("https://") && !sso_url.starts_with("http://") {
            return Err("the metadata's sign-in service is not a web address".into());
        }
        match sp.idp_signing_certs() {
            Ok(Some(certs)) if !certs.is_empty() => {}
            Ok(_) => return Err("the metadata holds no signing certificate, so nothing the provider sends could be checked".into()),
            Err(e) => return Err(format!("a certificate in the metadata cannot be read: {e}")),
        }
        Ok(Idp { metadata, entity_id, sso_url })
    }
}

/// This service's metadata, for an identity provider's administrator to load: its entity ID and
/// where responses go. It signs nothing and wants assertions signed.
pub fn sp_metadata(sp: &Sp) -> Result<String, String> {
    let descriptor = EntityDescriptor {
        entity_id: Some(sp.entity_id.to_string()),
        sp_sso_descriptors: Some(vec![SpSsoDescriptor {
            protocol_support_enumeration: Some("urn:oasis:names:tc:SAML:2.0:protocol".into()),
            authn_requests_signed: Some(false),
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
/// an XML ID: a letter or `_` first) and `relay_state`, which the provider posts back unchanged.
pub fn request_url(sp: &Sp, idp: &Idp, request_id: &str, relay_state: &str) -> Result<String, String> {
    let provider = service_provider(sp, &idp.metadata);
    let mut request = provider.make_authentication_request(&idp.sso_url).map_err(|e| e.to_string())?;
    request.id = request_id.to_string();
    request.force_authn = None;
    let url = request.redirect(relay_state).map_err(|e| e.to_string())?.ok_or("the request has no destination")?;
    Ok(url.to_string())
}

/// Checks a response the provider posted (`SAMLResponse`, base64) to the request `request_id`: signed
/// by a certificate in the provider's metadata with an accepted algorithm, from the provider, for
/// this service and its address, in answer to that request, in its time window, with a bearer
/// confirmation. Anything outside what the signature covers is not read.
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
    let assertion = service_provider(sp, &idp.metadata).parse_xml_response(&xml, Some(&[request_id])).map_err(|e| e.to_string())?;
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
    person(name_id, &attributes)
}

/// Whether xmlsec1 works on this machine: a key made now signs a response, the signature verifies,
/// and a changed response does not. A server whose crypto backend cannot load fails this, rather
/// than a person's sign-in.
pub fn self_check() -> Result<(), String> {
    use samael::crypto::{Crypto, CryptoProvider, ReduceMode};
    use samael::idp::response_builder::build_response_template;
    use samael::idp::{CertificateParams, IdentityProvider, KeyType, Rsa};
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
