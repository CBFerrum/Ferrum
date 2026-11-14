//! An identity provider for tests: a key made for the run, its metadata, and responses signed by
//! xmlsec1 the way providers sign them (RSA-SHA256 over a SHA-256 digest), which a test can change
//! before or after signing; assertions encrypted with OpenSSL (not xmlsec1, which opens them), and
//! logout messages in either binding. Nothing here is used to check anything.

use crate::Sp;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use flate2::read::DeflateDecoder;
use samael::crypto::{CertificateDer, Crypto, CryptoProvider};
use samael::idp::response_builder::{ResponseAttribute, build_response_template};
use samael::idp::sp_extractor::RequiredAttribute;
use samael::idp::{CertificateParams, IdentityProvider, KeyType, Rsa};
use samael::metadata::{HTTP_POST_BINDING, HTTP_REDIRECT_BINDING};
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
    /// Where it takes logout messages, and by which binding (none: it offers no single logout).
    pub slo: Option<(String, String)>,
}

/// How an assertion's data is encrypted.
#[derive(Debug, Clone, Copy)]
pub enum Data {
    Gcm,
    Cbc,
}

pub const RSA_OAEP: &str = "http://www.w3.org/2001/04/xmlenc#rsa-oaep-mgf1p";
pub const RSA_1_5: &str = "http://www.w3.org/2001/04/xmlenc#rsa-1_5";
const XENC: &str = "http://www.w3.org/2001/04/xmlenc#";
const DSIG: &str = "http://www.w3.org/2000/09/xmldsig#";

impl TestIdp {
    pub fn new(entity_id: &str, sso_url: &str) -> TestIdp {
        let key = IdentityProvider::generate_new(KeyType::Rsa(Rsa::Rsa2048)).expect("a key");
        let cert =
            key.create_certificate(&CertificateParams { common_name: "idp.test", issuer_name: "idp.test", days_until_expiration: 30 }).expect("a certificate");
        TestIdp { key, cert, entity_id: entity_id.into(), sso_url: sso_url.into(), slo: None }
    }

    /// The same provider, offering single logout at `url` by `binding` (`redirect` or `post`).
    pub fn with_logout(mut self, url: &str, binding: &str) -> TestIdp {
        let binding = if binding == "post" { HTTP_POST_BINDING } else { HTTP_REDIRECT_BINDING };
        self.slo = Some((binding.into(), url.into()));
        self
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
            r#"<md:EntityDescriptor xmlns:md="urn:oasis:names:tc:SAML:2.0:metadata" entityID="{}"{extra}><md:IDPSSODescriptor protocolSupportEnumeration="urn:oasis:names:tc:SAML:2.0:protocol">{}{}<md:SingleSignOnService Binding="{binding}" Location="{}"/></md:IDPSSODescriptor></md:EntityDescriptor>"#,
            self.entity_id,
            key.unwrap_or_default(),
            self.slo.as_ref().map(|(b, l)| format!(r#"<md:SingleLogoutService Binding="{b}" Location="{l}"/>"#)).unwrap_or_default(),
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
        other.sign_xml(&r.to_string().expect("XML"))
    }

    /// The response as XML, as it is (its signature templates empty).
    pub fn xml(r: &Response) -> String {
        r.to_string().expect("XML")
    }

    /// `xml` with its signature templates signed by this provider's key.
    pub fn sign_xml(&self, xml: &str) -> String {
        Crypto::sign_xml(xml, &self.key.export_private_key_der().expect("the key")).expect("signed")
    }

    /// A logout request to `sp`'s logout address, unsigned: naming `name_id` (of `format`) and the
    /// sessions `session_indexes`.
    pub fn logout_request(&self, sp: &Sp, id: &str, name_id: &str, format: Option<&str>, session_indexes: &[&str]) -> String {
        let format = format.map(|f| format!(r#" Format="{f}""#)).unwrap_or_default();
        let indexes: String = session_indexes.iter().map(|s| format!("<samlp:SessionIndex>{s}</samlp:SessionIndex>")).collect();
        format!(
            r#"<samlp:LogoutRequest xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" ID="{id}" Version="2.0" IssueInstant="{}" Destination="{}"><saml:Issuer>{}</saml:Issuer><saml:NameID{format}>{name_id}</saml:NameID>{indexes}</samlp:LogoutRequest>"#,
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            sp.slo_url.unwrap_or_default(),
            self.entity_id
        )
    }

    /// The answer to `sp`'s logout request `in_response_to`, unsigned.
    pub fn logout_response(&self, sp: &Sp, id: &str, in_response_to: &str, success: bool) -> String {
        let status = if success { "Success" } else { "Responder" };
        format!(
            r#"<samlp:LogoutResponse xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" ID="{id}" Version="2.0" IssueInstant="{}" Destination="{}" InResponseTo="{in_response_to}"><saml:Issuer>{}</saml:Issuer><samlp:Status><samlp:StatusCode Value="urn:oasis:names:tc:SAML:2.0:status:{status}"/></samlp:Status></samlp:LogoutResponse>"#,
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            sp.slo_url.unwrap_or_default(),
            self.entity_id
        )
    }

    /// A logout message (`param`: `SAMLRequest` or `SAMLResponse`) as the query of an HTTP-Redirect,
    /// signed (RSA-SHA256) by `signer`'s key.
    pub fn redirect(signer: &TestIdp, param: &str, xml: &str, relay_state: Option<&str>) -> String {
        use std::io::Write;
        let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
        let mut deflated = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        deflated.write_all(xml.as_bytes()).expect("deflated");
        let mut query = format!("{param}={}", enc(&STANDARD.encode(deflated.finish().expect("deflated"))));
        if let Some(r) = relay_state {
            query.push_str(&format!("&RelayState={}", enc(r)));
        }
        query.push_str(&format!("&SigAlg={}", enc("http://www.w3.org/2001/04/xmldsig-more#rsa-sha256")));
        let key = openssl::pkey::PKey::private_key_from_der(&signer.key.export_private_key_der().expect("the key")).expect("a key");
        let mut s = openssl::sign::Signer::new(openssl::hash::MessageDigest::sha256(), &key).expect("a signer");
        s.update(query.as_bytes()).expect("signed");
        format!("{query}&Signature={}", enc(&STANDARD.encode(s.sign_to_vec().expect("signed"))))
    }

    /// A logout message as an HTTP-POST carries it (base64), signed by `signer`'s key in the XML.
    pub fn post(signer: &TestIdp, xml: &str) -> String {
        let id = &xml[xml.find(" ID=\"").expect("an ID") + 5..];
        let id = &id[..id.find('"').expect("its end")];
        let mut template = signer.template(id);
        template.key_info = None;
        let with = xml.replacen("</saml:Issuer>", &format!("</saml:Issuer>{}", template.to_string().expect("a template")), 1);
        STANDARD.encode(signer.sign_xml(&with))
    }

    /// A message this service sends by HTTP-Redirect (`param`: `SAMLRequest` or `SAMLResponse`): its
    /// XML and its RelayState.
    pub fn message(url: &str, param: &str) -> (String, String) {
        let parsed = url::Url::parse(url).expect("a URL");
        let q: std::collections::HashMap<String, String> = parsed.query_pairs().into_owned().collect();
        let mut xml = String::new();
        DeflateDecoder::new(&STANDARD.decode(&q[param]).expect("base64")[..]).read_to_string(&mut xml).expect("deflated");
        (xml, q.get("RelayState").cloned().unwrap_or_default())
    }

    /// Whether a logout answer this service sent by HTTP-Redirect is signed by the key its metadata
    /// publishes.
    pub fn answer_signed_by_metadata(url: &str, sp_metadata: &str) -> bool {
        let Ok(cert) = samael::idp::sp_extractor::SPMetadataExtractor::try_from_xml(sp_metadata).and_then(|m| m.verification_cert()) else { return false };
        let Ok(url) = url::Url::parse(url) else { return false };
        samael::crypto::UrlVerifier::from_x509(&cert).and_then(|v| v.verify_signed_response_url(&url)).unwrap_or(false)
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

/// `xml` with its one assertion (`saml2:Assertion`, as signed or not) encrypted to the certificate
/// `to` (DER): its data by AES-256 in `data`'s mode, its key by `key_transport` (`RSA_OAEP`, or
/// `RSA_1_5` to see it refused). `peer`: the key beside the data (Shibboleth's way) rather than in it.
pub fn encrypt(xml: &str, to: &[u8], data: Data, key_transport: &str, peer: bool) -> String {
    use openssl::rsa::Padding;
    use openssl::symm::Cipher;
    let start = xml.find("<saml2:Assertion").expect("an assertion");
    let end = xml.rfind("</saml2:Assertion>").map(|i| i + "</saml2:Assertion>".len()).expect("its end");
    let plain = &xml.as_bytes()[start..end];
    let mut key = [0u8; 32];
    openssl::rand::rand_bytes(&mut key).expect("random");
    let (algorithm, value) = match data {
        Data::Gcm => {
            let mut iv = [0u8; 12];
            openssl::rand::rand_bytes(&mut iv).expect("random");
            let mut tag = [0u8; 16];
            let ct = openssl::symm::encrypt_aead(Cipher::aes_256_gcm(), &key, Some(&iv), &[], plain, &mut tag).expect("encrypted");
            ("http://www.w3.org/2009/xmlenc11#aes256-gcm", [&iv[..], &ct, &tag].concat())
        }
        Data::Cbc => {
            let mut iv = [0u8; 16];
            openssl::rand::rand_bytes(&mut iv).expect("random");
            let ct = openssl::symm::encrypt(Cipher::aes_256_cbc(), &key, Some(&iv), plain).expect("encrypted");
            ("http://www.w3.org/2001/04/xmlenc#aes256-cbc", [&iv[..], &ct].concat())
        }
    };
    let cert = openssl::x509::X509::from_der(to).expect("a certificate");
    let rsa = cert.public_key().expect("its key").rsa().expect("RSA");
    let mut wrapped = vec![0u8; rsa.size() as usize];
    let padding = if key_transport == RSA_1_5 { Padding::PKCS1 } else { Padding::PKCS1_OAEP };
    let n = rsa.public_encrypt(&key, &mut wrapped, padding).expect("wrapped");
    wrapped.truncate(n);
    let digest = if key_transport == RSA_1_5 { String::new() } else { format!(r#"<ds:DigestMethod Algorithm="{DSIG}sha1"/>"#) };
    let encrypted_key = format!(
        r#"<xenc:EncryptedKey xmlns:xenc="{XENC}" xmlns:ds="{DSIG}" Id="_k1"><xenc:EncryptionMethod Algorithm="{key_transport}">{digest}</xenc:EncryptionMethod><ds:KeyInfo><ds:X509Data><ds:X509Certificate>{}</ds:X509Certificate></ds:X509Data></ds:KeyInfo><xenc:CipherData><xenc:CipherValue>{}</xenc:CipherValue></xenc:CipherData></xenc:EncryptedKey>"#,
        STANDARD.encode(to),
        STANDARD.encode(&wrapped)
    );
    let (inside, beside) =
        if peer { (format!(r##"<ds:RetrievalMethod Type="{XENC}EncryptedKey" URI="#_k1"/>"##), encrypted_key) } else { (encrypted_key, String::new()) };
    let encrypted = format!(
        r#"<saml2:EncryptedAssertion xmlns:saml2="urn:oasis:names:tc:SAML:2.0:assertion"><xenc:EncryptedData xmlns:xenc="{XENC}" Type="{XENC}Element"><xenc:EncryptionMethod Algorithm="{algorithm}"/><ds:KeyInfo xmlns:ds="{DSIG}">{inside}</ds:KeyInfo><xenc:CipherData><xenc:CipherValue>{}</xenc:CipherValue></xenc:CipherData></xenc:EncryptedData>{beside}</saml2:EncryptedAssertion>"#,
        STANDARD.encode(&value)
    );
    format!("{}{encrypted}{}", &xml[..start], &xml[end..])
}
