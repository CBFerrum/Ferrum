//! An identity provider for tests: a key made for the run, its metadata, and responses signed by
//! xmlsec1 the way providers sign them (RSA-SHA256 over a SHA-256 digest), which a test can change
//! before or after signing. Nothing here is used to check anything.

use crate::Sp;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use flate2::read::DeflateDecoder;
use samael::crypto::{CertificateDer, Crypto, CryptoProvider};
use samael::idp::response_builder::{ResponseAttribute, build_response_template};
use samael::idp::sp_extractor::RequiredAttribute;
use samael::idp::{CertificateParams, IdentityProvider, KeyType, Rsa};
use samael::metadata::HTTP_REDIRECT_BINDING;
use samael::signature::{DigestAlgorithm, Signature};
use samael::traits::ToXml;
use std::io::Read;

pub use samael::schema::Response;

pub const PERSISTENT: &str = "urn:oasis:names:tc:SAML:2.0:nameid-format:persistent";

pub struct TestIdp {
    key: IdentityProvider,
    pub cert: CertificateDer,
    pub entity_id: String,
    pub sso_url: String,
}

impl TestIdp {
    pub fn new(entity_id: &str, sso_url: &str) -> TestIdp {
        let key = IdentityProvider::generate_new(KeyType::Rsa(Rsa::Rsa2048)).expect("a key");
        let cert =
            key.create_certificate(&CertificateParams { common_name: "idp.test", issuer_name: "idp.test", days_until_expiration: 30 }).expect("a certificate");
        TestIdp { key, cert, entity_id: entity_id.into(), sso_url: sso_url.into() }
    }

    /// Its metadata; `cert` none leaves the signing certificate out, `binding` and `extra` (attributes
    /// of the root) say what else it holds.
    pub fn metadata_with(&self, cert: Option<&CertificateDer>, binding: &str, extra: &str) -> String {
        let key = cert.map(|c| {
            format!(
                r#"<md:KeyDescriptor use="signing"><ds:KeyInfo xmlns:ds="http://www.w3.org/2000/09/xmldsig#"><ds:X509Data><ds:X509Certificate>{}</ds:X509Certificate></ds:X509Data></ds:KeyInfo></md:KeyDescriptor>"#,
                STANDARD.encode(c.der_data())
            )
        });
        format!(
            r#"<md:EntityDescriptor xmlns:md="urn:oasis:names:tc:SAML:2.0:metadata" entityID="{}"{extra}><md:IDPSSODescriptor protocolSupportEnumeration="urn:oasis:names:tc:SAML:2.0:protocol">{}<md:SingleSignOnService Binding="{binding}" Location="{}"/></md:IDPSSODescriptor></md:EntityDescriptor>"#,
            self.entity_id,
            key.unwrap_or_default(),
            self.sso_url
        )
    }

    pub fn metadata(&self) -> String {
        self.metadata_with(Some(&self.cert), HTTP_REDIRECT_BINDING, "")
    }

    /// A signature template over the element `id`, as providers make them.
    pub fn template(&self, id: &str) -> Signature {
        let mut s = Signature::template(id, &self.cert);
        s.signed_info.reference[0].digest_method.algorithm = DigestAlgorithm::Sha256;
        s
    }

    /// An answer to `request` for `sp`, naming `name_id` (persistent) with `attributes`, the whole
    /// response to be signed; `change` adjusts it before it is.
    pub fn response(&self, sp: &Sp, request: &str, name_id: &str, attributes: &[(&str, &str)], change: impl FnOnce(&mut Response)) -> Response {
        let attributes: Vec<ResponseAttribute> = attributes
            .iter()
            .map(|(name, value)| ResponseAttribute { required_attribute: RequiredAttribute { name: name.to_string(), format: None }, value })
            .collect();
        let mut r = build_response_template(&self.cert, name_id, sp.entity_id, &self.entity_id, sp.acs_url, request, &attributes);
        let later = chrono::Utc::now() + chrono::Duration::minutes(5);
        let a = r.assertion.as_mut().expect("an assertion");
        let subject = a.subject.as_mut().expect("a subject");
        for c in subject.subject_confirmations.iter_mut().flatten() {
            c.subject_confirmation_data.as_mut().expect("confirmation data").not_on_or_after = Some(later);
        }
        subject.name_id.as_mut().expect("a NameID").format = Some(PERSISTENT.into());
        r.signature = Some(self.template(&r.id));
        change(&mut r);
        r
    }

    /// The response as XML, its signature templates signed by this provider's key.
    pub fn signed(&self, r: &Response) -> String {
        self.signed_by(self, r)
    }

    /// The response as XML, signed by `other`'s key.
    pub fn signed_by(&self, other: &TestIdp, r: &Response) -> String {
        Crypto::sign_xml(r.to_string().expect("XML"), &other.key.export_private_key_der().expect("the key")).expect("signed")
    }

    /// What a browser would post: the XML in base64.
    pub fn posted(xml: &str) -> String {
        STANDARD.encode(xml)
    }

    /// The request a sign-in address carries (HTTP-Redirect binding): its XML and its RelayState.
    pub fn request(url: &str) -> (String, String) {
        let parsed = url::Url::parse(url).expect("a URL");
        let q: std::collections::HashMap<String, String> = parsed.query_pairs().into_owned().collect();
        let mut xml = String::new();
        DeflateDecoder::new(&STANDARD.decode(&q["SAMLRequest"]).expect("base64")[..]).read_to_string(&mut xml).expect("deflated");
        (xml, q.get("RelayState").cloned().unwrap_or_default())
    }

    /// Whether a sign-in address is signed by the key a service's metadata publishes.
    pub fn signed_by_metadata(url: &str, sp_metadata: &str) -> bool {
        let Ok(cert) = samael::idp::sp_extractor::SPMetadataExtractor::try_from_xml(sp_metadata).and_then(|m| m.verification_cert()) else { return false };
        let Ok(url) = url::Url::parse(url) else { return false };
        samael::crypto::UrlVerifier::from_x509(&cert).and_then(|v| v.verify_signed_request_url(&url)).unwrap_or(false)
    }

    /// The ID of the request a sign-in address carries.
    pub fn request_id(url: &str) -> String {
        let (xml, _) = TestIdp::request(url);
        let start = xml.find(" ID=\"").expect("an ID") + 5;
        xml[start..start + xml[start..].find('"').expect("its end")].to_string()
    }
}
