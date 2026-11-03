//! Responses signed by xmlsec1 with keys made here, as an identity provider would sign them, and the
//! ways an attacker would change them.

use super::*;
use crate::testing::TestIdp;
use samael::crypto::{CertificateDer, UrlVerifier};
use samael::idp::sp_extractor::SPMetadataExtractor;
use samael::signature::Signature;

const SP: Sp = Sp { entity_id: "https://erp.example.test/api/auth/saml/p1", acs_url: "https://erp.example.test/api/auth/saml/p1/acs", key: None };
const REQUEST: &str = "_4f1c2d9e8b7a6f5e4d3c2b1a0f9e8d7c";

fn idp() -> TestIdp {
    TestIdp::new("https://idp.example.test/metadata", "https://idp.example.test/sso")
}

fn metadata(k: &TestIdp) -> Idp {
    Idp::read(&k.metadata()).unwrap()
}

/// The usual answer, with `change` made before it is signed.
fn response(k: &TestIdp, change: impl FnOnce(&mut Response)) -> Response {
    k.response(&SP, REQUEST, "persistent-7", &[("email", "Pat@Acme.test")], change)
}

fn posted(xml: &str) -> String {
    TestIdp::posted(xml)
}

#[test]
fn a_signed_answer_to_our_request_signs_the_person_in() {
    let k = idp();
    let p = verify(&SP, &metadata(&k), &posted(&k.signed(&response(&k, |_| {}))), REQUEST).unwrap();
    assert_eq!(p, Person { subject: "persistent-7".into(), email: Some("pat@acme.test".into()), name: None });
}

#[test]
fn an_assertion_signed_on_its_own_counts_too() {
    let k = idp();
    let r = response(&k, |r| {
        r.signature = None;
        let a = r.assertion.as_mut().unwrap();
        a.signature = Some(k.template(&a.id));
    });
    assert_eq!(verify(&SP, &metadata(&k), &posted(&k.signed(&r)), REQUEST).unwrap().subject, "persistent-7");
}

#[test]
fn a_change_after_signing_is_refused() {
    let k = idp();
    let xml = k.signed(&response(&k, |_| {}));
    assert!(xml.contains("Pat@Acme.test"));
    assert!(verify(&SP, &metadata(&k), &posted(&xml.replace("Pat@Acme.test", "boss@acme.test")), REQUEST).is_err());
    assert!(verify(&SP, &metadata(&k), &posted(&xml.replace("persistent-7", "persistent-1")), REQUEST).is_err());
}

#[test]
fn only_the_providers_own_key_counts() {
    let (k, other) = (idp(), idp());
    let xml = k.signed_by(&other, &response(&k, |_| {}));
    assert!(verify(&SP, &metadata(&k), &posted(&xml), REQUEST).is_err(), "signed by a key the metadata does not hold");
    let unsigned = response(&k, |r| r.signature = None).to_string().unwrap();
    assert!(verify(&SP, &metadata(&k), &posted(&unsigned), REQUEST).is_err(), "not signed at all");
    let sha1 = response(&k, |r| r.signature = Some(Signature::template(&r.id, &k.cert)));
    assert!(verify(&SP, &metadata(&k), &posted(&k.signed(&sha1)), REQUEST).is_err(), "a SHA-1 digest");
}

#[test]
fn a_wrapped_assertion_proves_nothing() {
    // The signed response stays intact; a forged assertion is put before the signed one, and after
    // it, where a reader of the first or the last assertion would take it.
    let k = idp();
    let xml = k.signed(&response(&k, |_| {}));
    let start = xml.find("<saml2:Assertion").expect("an assertion");
    let end = xml.rfind("</saml2:Assertion>").map(|i| i + "</saml2:Assertion>".len()).expect("its end");
    let forged = xml[start..end].replace("Pat@Acme.test", "boss@acme.test").replace("persistent-7", "persistent-1").replacen(" ID=\"", " ID=\"x", 1);
    for wrapped in [format!("{}{forged}{}", &xml[..start], &xml[start..]), format!("{}{forged}{}", &xml[..end], &xml[end..])] {
        if let Ok(p) = verify(&SP, &metadata(&k), &posted(&wrapped), REQUEST) {
            assert_eq!(p.subject, "persistent-7", "never the forged person");
        }
    }
}

#[test]
fn an_answer_to_another_request_or_service_is_refused() {
    let k = idp();
    let idp = metadata(&k);
    let ok = posted(&k.signed(&response(&k, |_| {})));
    assert!(verify(&SP, &idp, &ok, "_another").is_err(), "another request");
    let elsewhere = Sp { acs_url: "https://erp.example.test/api/auth/saml/p2/acs", ..SP };
    assert!(verify(&elsewhere, &idp, &ok, REQUEST).is_err(), "posted to another address");
    let other = Sp { entity_id: "https://erp.example.test/api/auth/saml/p2", ..SP };
    assert!(verify(&other, &idp, &ok, REQUEST).is_err(), "for another service");

    let no_audience = response(&k, |r| r.assertion.as_mut().unwrap().conditions = None);
    assert!(verify(&SP, &idp, &posted(&k.signed(&no_audience)), REQUEST).is_err(), "an assertion for anyone");
    let unsolicited = response(&k, |r| r.in_response_to = None);
    assert!(verify(&SP, &idp, &posted(&k.signed(&unsolicited)), REQUEST).is_err(), "nobody asked for it");
    let stale = response(&k, |r| {
        let old = chrono::Utc::now() - chrono::Duration::hours(1);
        r.issue_instant = old;
        r.assertion.as_mut().unwrap().issue_instant = old;
    });
    assert!(verify(&SP, &idp, &posted(&k.signed(&stale)), REQUEST).is_err(), "issued an hour ago");
    let impostor = response(&k, |r| r.assertion.as_mut().unwrap().issuer.value = Some("https://evil.example.test".into()));
    assert!(verify(&SP, &idp, &posted(&k.signed(&impostor)), REQUEST).is_err(), "from another issuer");
}

#[test]
fn an_encrypted_assertion_is_refused_in_so_many_words() {
    let k = idp();
    let xml = k.signed(&response(&k, |_| {}));
    let start = xml.find("<saml2:Assertion").expect("an assertion");
    let end = xml.rfind("</saml2:Assertion>").map(|i| i + "</saml2:Assertion>".len()).expect("its end");
    let encrypted = format!(
        "{}<saml2:EncryptedAssertion><xenc:EncryptedData xmlns:xenc=\"http://www.w3.org/2001/04/xmlenc#\"/></saml2:EncryptedAssertion>{}",
        &xml[..start],
        &xml[end..]
    );
    let why = verify(&SP, &metadata(&k), &posted(&encrypted), REQUEST).unwrap_err();
    assert!(why.contains("encrypted"), "{why}");
}

#[test]
fn a_refusal_says_what_it_is() {
    let k = idp();
    let refused = response(&k, |r| {
        r.signature = None;
        r.assertion = None;
        r.status.as_mut().unwrap().status_code.value = Some("urn:oasis:names:tc:SAML:2.0:status:Responder".into());
    });
    let why = verify(&SP, &metadata(&k), &posted(&refused.to_string().unwrap()), REQUEST).unwrap_err();
    assert!(why.contains("Responder"), "{why}");
    assert!(verify(&SP, &metadata(&k), "not base64!", REQUEST).is_err());
    assert!(verify(&SP, &metadata(&k), &"A".repeat(MAX_RESPONSE + 1), REQUEST).is_err());
}

#[test]
fn metadata_must_let_responses_be_checked() {
    let k = idp();
    assert_eq!(metadata(&k).entity_id, "https://idp.example.test/metadata");
    assert_eq!(metadata(&k).sso_url, "https://idp.example.test/sso");
    assert!(Idp::read(&k.metadata_with(None, HTTP_REDIRECT_BINDING, "")).unwrap_err().contains("signing certificate"));
    assert!(Idp::read(&k.metadata_with(Some(&k.cert), HTTP_POST_BINDING, "")).unwrap_err().contains("HTTP-Redirect"));
    assert!(Idp::read(&k.metadata_with(Some(&k.cert), HTTP_REDIRECT_BINDING, r#" validUntil="2020-01-01T00:00:00Z""#)).unwrap_err().contains("expired"));
    assert!(Idp::read("<html/>").is_err());
    assert!(Idp::read(&format!("<x>{}</x>", " ".repeat(MAX_METADATA))).is_err());
}

#[test]
fn the_request_and_our_metadata_say_where_answers_go() {
    let k = idp();
    let url = request_url(&SP, &metadata(&k), REQUEST, "relay-1").unwrap();
    assert!(url.starts_with("https://idp.example.test/sso?"));
    let (xml, relay) = TestIdp::request(&url);
    assert_eq!((relay.as_str(), TestIdp::request_id(&url).as_str()), ("relay-1", REQUEST));
    for part in [format!("AssertionConsumerServiceURL=\"{}\"", SP.acs_url), "Destination=\"https://idp.example.test/sso\"".into(), SP.entity_id.into()] {
        assert!(xml.contains(&part), "{part} in {xml}");
    }

    let ours = SPMetadataExtractor::try_from_xml(&sp_metadata(&SP).unwrap()).unwrap();
    assert_eq!(ours.issuer().unwrap(), SP.entity_id);
    assert_eq!(ours.acs().unwrap().url, SP.acs_url);
}

#[test]
fn xmlsec_works_here() {
    self_check().unwrap();
}

#[test]
fn with_its_own_key_a_service_signs_its_requests() {
    let (k, key, other) = (idp(), new_sp_key("ferrum test").unwrap(), new_sp_key("someone else").unwrap());
    let sp = Sp { key: Some(&key), ..SP };
    let url = url::Url::parse(&request_url(&sp, &metadata(&k), REQUEST, "relay-1").unwrap()).unwrap();
    let checks =
        |key: &SpKey| UrlVerifier::from_x509(&CertificateDer::from(key.certificate_der.clone())).unwrap().verify_signed_request_url(&url).unwrap_or(false);
    assert!(checks(&key), "{url}");
    assert!(!checks(&other));
    let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(q["SigAlg"], "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256");
    assert_eq!((TestIdp::request_id(url.as_str()).as_str(), q["RelayState"].as_str()), (REQUEST, "relay-1"));

    let ours = sp_metadata(&sp).unwrap();
    assert!(ours.contains("AuthnRequestsSigned=\"true\"") && ours.matches(&STANDARD.encode(&key.certificate_der)).count() == 1, "{ours}");
    assert!(ours.contains("use=\"signing\"") && !ours.contains("use=\"encryption\""), "no key to encrypt to");
    assert!(sp_metadata(&SP).unwrap().contains("AuthnRequestsSigned=\"false\""));
    // Answers are read as before.
    assert_eq!(verify(&sp, &metadata(&k), &posted(&k.signed(&response(&k, |_| {}))), REQUEST).unwrap().subject, "persistent-7");
    assert_eq!(format!("{key:?}"), "SpKey(…)", "never printed");
}
