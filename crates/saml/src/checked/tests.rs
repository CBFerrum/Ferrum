//! Responses signed by xmlsec1 with keys made here, as an identity provider would sign them, and the
//! ways an attacker would change them.

use super::*;
use crate::testing::{Data, RSA_1_5, RSA_OAEP, TestIdp, encrypt};
use crate::{Incoming, Logout, NameId, logout_request, logout_response, peek_issuer, read_logout};
use samael::crypto::{CertificateDer, UrlVerifier};
use samael::idp::sp_extractor::SPMetadataExtractor;
use samael::signature::Signature;

const SP: Sp = Sp {
    entity_id: "https://erp.example.test/api/auth/saml/p1",
    acs_url: "https://erp.example.test/api/auth/saml/p1/acs",
    key: None,
    decrypt: &[],
    slo_url: Some("https://erp.example.test/api/auth/saml/p1/slo"),
};
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
    assert_eq!((p.subject.as_str(), p.email.as_deref(), p.name), ("persistent-7", Some("pat@acme.test"), None));
    assert_eq!(p.name_id, Some(NameId { value: "persistent-7".into(), format: Some(crate::testing::PERSISTENT.into()) }), "kept for logging out");
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

/// The assertion of `xml` (a response), encrypted to `to` with AES-GCM and RSA-OAEP, as XML.
fn encrypted_part(xml: &str, to: &SpKey) -> String {
    let e = encrypt(xml, &to.certificate_der, Data::Gcm, RSA_OAEP, false);
    let start = e.find("<saml2:EncryptedAssertion").unwrap();
    let end = e.find("</saml2:EncryptedAssertion>").unwrap() + "</saml2:EncryptedAssertion>".len();
    e[start..end].to_string()
}

/// The usual answer with only its assertion signed.
fn assertion_signed(k: &TestIdp) -> Response {
    response(k, |r| {
        r.signature = None;
        let a = r.assertion.as_mut().unwrap();
        a.signature = Some(k.template(&a.id));
    })
}

#[test]
fn an_assertion_encrypted_to_our_key_is_opened_and_checked_as_a_plain_one() {
    let (k, ours) = (idp(), [new_sp_key("ours").unwrap()]);
    let sp = Sp { decrypt: &ours, ..SP };
    // Signed, then encrypted (Shibboleth, AD FS, Entra ID): the key inside the data, or beside it.
    for peer in [false, true] {
        let xml = encrypt(&k.signed(&assertion_signed(&k)), &ours[0].certificate_der, Data::Gcm, RSA_OAEP, peer);
        assert!(!xml.contains("Pat@Acme.test") && !xml.contains("persistent-7"), "encrypted");
        let p = verify(&sp, &metadata(&k), &posted(&xml), REQUEST).unwrap();
        assert_eq!((p.subject.as_str(), p.email.as_deref()), ("persistent-7", Some("pat@acme.test")));
    }
    // Encrypted, then the whole response signed (Keycloak's default): AES-CBC too, the signature
    // being checked before anything is decrypted.
    for data in [Data::Gcm, Data::Cbc] {
        let xml = k.sign_xml(&encrypt(&response(&k, |_| {}).to_string().unwrap(), &ours[0].certificate_der, data, RSA_OAEP, false));
        assert_eq!(verify(&sp, &metadata(&k), &posted(&xml), REQUEST).unwrap().subject, "persistent-7", "{data:?}");
    }
    // And a plain one, as before.
    assert_eq!(verify(&sp, &metadata(&k), &posted(&k.signed(&response(&k, |_| {}))), REQUEST).unwrap().subject, "persistent-7");
}

#[test]
fn an_opened_assertion_is_checked_like_any_other() {
    let (k, ours) = (idp(), [new_sp_key("ours").unwrap()]);
    let sp = Sp { decrypt: &ours, ..SP };
    let to_us = |r: &Response| posted(&encrypt(&k.signed(r), &ours[0].certificate_der, Data::Gcm, RSA_OAEP, false));
    let elsewhere = Sp { acs_url: "https://erp.example.test/api/auth/saml/p2/acs", ..sp };
    assert!(verify(&elsewhere, &metadata(&k), &to_us(&assertion_signed(&k)), REQUEST).is_err(), "for another address");
    assert!(verify(&sp, &metadata(&k), &to_us(&assertion_signed(&k)), "_another").is_err(), "for another request");
    let stale = response(&k, |r| {
        r.signature = None;
        let a = r.assertion.as_mut().unwrap();
        a.issue_instant = chrono::Utc::now() - chrono::Duration::hours(1);
        a.signature = Some(k.template(&a.id));
    });
    assert!(verify(&sp, &metadata(&k), &to_us(&stale), REQUEST).is_err(), "issued an hour ago");
    let no_audience = response(&k, |r| {
        r.signature = None;
        let a = r.assertion.as_mut().unwrap();
        a.conditions = None;
        a.signature = Some(k.template(&a.id));
    });
    assert!(verify(&sp, &metadata(&k), &to_us(&no_audience), REQUEST).is_err(), "for anyone");
}

#[test]
fn encryption_proves_nothing_without_the_providers_signature() {
    let (k, ours, other) = (idp(), [new_sp_key("ours").unwrap()], new_sp_key("someone else").unwrap());
    let sp = Sp { decrypt: &ours, ..SP };
    let unsigned = response(&k, |r| r.signature = None).to_string().unwrap();
    // Anyone can encrypt to the published key: an assertion nobody signed is refused once opened.
    let forged = encrypt(&unsigned.replace("persistent-7", "persistent-1"), &ours[0].certificate_der, Data::Gcm, RSA_OAEP, false);
    assert!(verify(&sp, &metadata(&k), &posted(&forged), REQUEST).is_err(), "unsigned inside");
    // A response signed as an empty shell, with an encrypted assertion put in after signing.
    let shell = k.signed(&response(&k, |r| r.assertion = None));
    let stuffed = shell.replace("</saml2p:Response>", &format!("{}</saml2p:Response>", encrypted_part(&unsigned, &ours[0])));
    assert!(verify(&sp, &metadata(&k), &posted(&stuffed), REQUEST).is_err(), "a signature over less than the assertion");
    // A good encrypted-then-signed response whose encrypted assertion is swapped for another.
    let good = k.sign_xml(&encrypt(&response(&k, |_| {}).to_string().unwrap(), &ours[0].certificate_der, Data::Gcm, RSA_OAEP, false));
    assert!(verify(&sp, &metadata(&k), &posted(&good), REQUEST).is_ok());
    let start = good.find("<saml2:EncryptedAssertion").unwrap();
    let end = good.find("</saml2:EncryptedAssertion>").unwrap() + "</saml2:EncryptedAssertion>".len();
    let swapped = format!("{}{}{}", &good[..start], encrypted_part(&unsigned.replace("persistent-7", "persistent-1"), &ours[0]), &good[end..]);
    assert!(verify(&sp, &metadata(&k), &posted(&swapped), REQUEST).is_err(), "swapped under the response's signature");
    // Both signed, then the assertion encrypted: the response's signature no longer checks, and the
    // assertion's inside does not rescue it.
    let both = response(&k, |r| {
        let a = r.assertion.as_mut().unwrap();
        a.signature = Some(k.template(&a.id));
    });
    let broken = encrypt(&k.signed(&both), &ours[0].certificate_der, Data::Gcm, RSA_OAEP, false);
    assert!(verify(&sp, &metadata(&k), &posted(&broken), REQUEST).is_err(), "a response signature that does not check");
    let inner_signed = encrypt(&k.signed(&assertion_signed(&k)), &ours[0].certificate_der, Data::Gcm, RSA_OAEP, false);
    // Two assertions, one of them encrypted.
    let two = good.replace("</saml2p:Response>", &format!("{}</saml2p:Response>", encrypted_part(&unsigned, &ours[0])));
    assert!(verify(&sp, &metadata(&k), &posted(&two), REQUEST).is_err(), "two assertions");
    // Encrypted to another key, or this service has none.
    let elsewhere = encrypt(&k.signed(&assertion_signed(&k)), &other.certificate_der, Data::Gcm, RSA_OAEP, false);
    let why = verify(&sp, &metadata(&k), &posted(&elsewhere), REQUEST).unwrap_err();
    assert!(verify(&SP, &metadata(&k), &posted(&inner_signed), REQUEST).is_err(), "no key here");
    // A changed cipher text fails the same way as the wrong key: nothing says which step failed.
    let at = inner_signed.rfind("</xenc:CipherValue>").unwrap() - 4;
    let flipped = format!("{}{}{}", &inner_signed[..at], if &inner_signed[at..at + 1] == "A" { "B" } else { "A" }, &inner_signed[at + 1..]);
    assert_eq!(verify(&sp, &metadata(&k), &posted(&flipped), REQUEST).unwrap_err(), why);
    assert_eq!(why, xmlenc::UNREADABLE);
}

#[test]
fn only_the_listed_algorithms_are_used() {
    let (k, ours) = (idp(), [new_sp_key("ours").unwrap()]);
    let sp = Sp { decrypt: &ours, ..SP };
    let signed_inside = k.signed(&assertion_signed(&k));
    // CBC where only the assertion is signed: what is decrypted could have been changed.
    let cbc = encrypt(&signed_inside, &ours[0].certificate_der, Data::Cbc, RSA_OAEP, false);
    assert_eq!(verify(&sp, &metadata(&k), &posted(&cbc), REQUEST).unwrap_err(), xmlenc::NOT_ACCEPTED);
    // RSA PKCS#1 v1.5 for the key, never.
    let v15 = encrypt(&signed_inside, &ours[0].certificate_der, Data::Gcm, RSA_1_5, false);
    assert_eq!(verify(&sp, &metadata(&k), &posted(&v15), REQUEST).unwrap_err(), xmlenc::NOT_ACCEPTED);
    let v15_signed = k.sign_xml(&encrypt(&response(&k, |_| {}).to_string().unwrap(), &ours[0].certificate_der, Data::Cbc, RSA_1_5, false));
    assert_eq!(verify(&sp, &metadata(&k), &posted(&v15_signed), REQUEST).unwrap_err(), xmlenc::NOT_ACCEPTED, "nor under a signature");
    // Nothing xmlsec1 would fetch.
    let gcm = encrypt(&signed_inside, &ours[0].certificate_der, Data::Gcm, RSA_OAEP, false);
    let at = gcm.rfind("<xenc:CipherData>").unwrap();
    let fetched = format!(
        r#"{}<xenc:CipherData><xenc:CipherReference URI="file:///etc/passwd"/></xenc:CipherData></xenc:EncryptedData></saml2:EncryptedAssertion>{}"#,
        &gcm[..at],
        &gcm[gcm.find("</saml2:EncryptedAssertion>").unwrap() + 27..]
    );
    assert_eq!(verify(&sp, &metadata(&k), &posted(&fetched), REQUEST).unwrap_err(), xmlenc::NOT_ACCEPTED);
}

#[test]
fn while_a_key_is_replaced_both_open_assertions() {
    let (k, old, new) = (idp(), new_sp_key("old").unwrap(), new_sp_key("new").unwrap());
    let both = [new.clone(), old.clone()];
    let sp = Sp { decrypt: &both, ..SP };
    for to in [&old, &new] {
        let xml = encrypt(&k.signed(&assertion_signed(&k)), &to.certificate_der, Data::Gcm, RSA_OAEP, false);
        assert_eq!(verify(&sp, &metadata(&k), &posted(&xml), REQUEST).unwrap().subject, "persistent-7");
    }
    let only_new = [new.clone()];
    let xml = encrypt(&k.signed(&assertion_signed(&k)), &old.certificate_der, Data::Gcm, RSA_OAEP, false);
    assert!(verify(&Sp { decrypt: &only_new, ..SP }, &metadata(&k), &posted(&xml), REQUEST).is_err(), "once the old one is gone");
    let ours = sp_metadata(&sp).unwrap();
    assert_eq!(ours.matches("use=\"encryption\"").count(), 2, "{ours}");
    assert!(ours.find(&STANDARD.encode(&new.certificate_der)).unwrap() < ours.find(&STANDARD.encode(&old.certificate_der)).unwrap(), "the new one first");
    assert!(ours.contains("aes256-gcm") && ours.contains("rsa-oaep-mgf1p") && !ours.contains("cbc"), "{ours}");
    assert_eq!(ours.matches(&format!("Location=\"{}\"", SP.slo_url.unwrap())).count(), 2, "both bindings");
}

#[test]
fn a_logout_request_from_the_provider_is_read_when_signed_by_it() {
    let (k, rogue) = (idp(), idp());
    let idp = metadata(&k);
    let xml = k.logout_request(&SP, "_lr1", "persistent-7", Some(crate::testing::PERSISTENT), &["s1", "s2"]);
    let want = Logout::Request {
        id: "_lr1".into(),
        name_id: NameId { value: "persistent-7".into(), format: Some(crate::testing::PERSISTENT.into()) },
        session_indexes: vec!["s1".into(), "s2".into()],
        relay_state: Some("r1".into()),
    };
    let redirect = TestIdp::redirect(&k, "SAMLRequest", &xml, Some("r1"));
    assert_eq!(read_logout(&SP, &idp, Incoming::Redirect(&redirect)).unwrap(), want);
    assert_eq!(peek_issuer(Incoming::Redirect(&redirect)).unwrap(), k.entity_id);
    let post = TestIdp::post(&k, &xml);
    assert_eq!(read_logout(&SP, &idp, Incoming::Post { message: &post, relay_state: Some("r1") }).unwrap(), want);

    let refused = |incoming: Incoming| read_logout(&SP, &idp, incoming).unwrap_err();
    let unsigned = &redirect[..redirect.find("&Signature=").unwrap()];
    let why = refused(Incoming::Redirect(unsigned));
    for bad in [
        refused(Incoming::Redirect(&TestIdp::redirect(&rogue, "SAMLRequest", &xml, Some("r1")))),
        refused(Incoming::Redirect(&redirect.replace("RelayState=r1", "RelayState=r2"))),
        refused(Incoming::Post { message: &TestIdp::post(&rogue, &xml), relay_state: None }),
        refused(Incoming::Post { message: &STANDARD.encode(&xml), relay_state: None }),
        refused(Incoming::Redirect(&TestIdp::redirect(&k, "SAMLRequest", &xml.replace("/p1/slo", "/p2/slo"), None))),
        refused(Incoming::Redirect(&TestIdp::redirect(&k, "SAMLRequest", &xml.replace(&k.entity_id, "https://other.test"), None))),
        refused(Incoming::Redirect(&TestIdp::redirect(&k, "SAMLRequest", &xml.replace("IssueInstant=\"", "IssueInstant=\"2020-01-01T00:00:00Z\" x=\""), None))),
        refused(Incoming::Redirect(&format!("{redirect}&Signature=x"))),
    ] {
        assert_eq!(bad, why, "every refusal says the same");
    }
    let answer = k.logout_response(&SP, "_a1", "_ours", true);
    assert_eq!(
        read_logout(&SP, &idp, Incoming::Redirect(&TestIdp::redirect(&k, "SAMLResponse", &answer, None))).unwrap(),
        Logout::Response { in_response_to: "_ours".into(), success: true }
    );
}

#[test]
fn logout_messages_go_signed_where_the_provider_takes_them() {
    let (k, key) = (idp().with_logout("https://idp.example.test/slo", "redirect"), new_sp_key("ferrum test").unwrap());
    let sp = Sp { key: Some(&key), ..SP };
    let idp = Idp::read(&k.metadata()).unwrap();
    let name_id = NameId { value: "persistent-7".into(), format: Some(crate::testing::PERSISTENT.into()) };
    let out = logout_request(&sp, &idp, "_out1", &name_id, Some("s1"), "relay").unwrap();
    assert!(out.url.starts_with("https://idp.example.test/slo?") && out.fields.is_none(), "{out:?}");
    assert!(TestIdp::signed_by_metadata(&out.url, &sp_metadata(&sp).unwrap()));
    let (xml, relay) = TestIdp::message(&out.url, "SAMLRequest");
    for part in ["ID=\"_out1\"", "Destination=\"https://idp.example.test/slo\"", ">persistent-7</saml:NameID>", ">s1</samlp:SessionIndex>", SP.entity_id] {
        assert!(xml.contains(part), "{part} in {xml}");
    }
    assert_eq!(relay, "relay");
    let answer = logout_response(&sp, &idp, "_out2", "_theirs", true, "").unwrap();
    assert!(TestIdp::answer_signed_by_metadata(&answer.url, &sp_metadata(&sp).unwrap()));
    assert!(TestIdp::message(&answer.url, "SAMLResponse").0.contains("InResponseTo=\"_theirs\""));

    // A provider that takes them only posted: a form to post, signed in the XML.
    let posting =
        Idp::read(&k.metadata().replace("HTTP-Redirect\" Location=\"https://idp.example.test/slo", "HTTP-POST\" Location=\"https://idp.example.test/slo"))
            .unwrap();
    let out = logout_request(&sp, &posting, "_out3", &name_id, None, "relay").unwrap();
    let fields = out.fields.expect("a form");
    assert_eq!((out.url.as_str(), fields[0].0.as_str(), fields[1].1.as_str()), ("https://idp.example.test/slo", "SAMLRequest", "relay"));
    let signed = String::from_utf8(STANDARD.decode(&fields[0].1).unwrap()).unwrap();
    let cert = CertificateDer::from(key.certificate_der.clone());
    assert!(Crypto::reduce_xml_to_signed_with_allowed_algorithms(&signed, &[cert], ReduceMode::default(), Some(&ALGORITHMS)).is_ok(), "{signed}");
    assert!(logout_request(&SP, &posting, "_x", &name_id, None, "").is_err(), "unsigned: never");
    assert!(logout_request(&sp, &metadata(&self::idp()), "_x", &name_id, None, "").is_err(), "a provider without single logout");
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
    let url = request_url(&SP, &metadata(&k), REQUEST, "relay-1", false).unwrap();
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
    let url = url::Url::parse(&request_url(&sp, &metadata(&k), REQUEST, "relay-1", false).unwrap()).unwrap();
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
    assert!(!sp_metadata(&Sp { slo_url: None, ..sp }).unwrap().contains("SingleLogoutService"), "no single logout");
    assert!(sp_metadata(&SP).unwrap().contains("AuthnRequestsSigned=\"false\""));
    // Answers are read as before.
    assert_eq!(verify(&sp, &metadata(&k), &posted(&k.signed(&response(&k, |_| {}))), REQUEST).unwrap().subject, "persistent-7");
    assert_eq!(format!("{key:?}"), "SpKey(…)", "never printed");
}

#[test]
fn encryption_is_on_only_where_the_library_opens_it() {
    let can = crate::encryption().expect("this xmlsec1 has AES-GCM and RSA-OAEP");
    assert!(can.data.contains(&xmlenc::GCM[2]) && can.transport.contains(&xmlenc::OAEP[0]), "{can:?}");
    let (k, ours) = (idp(), [new_sp_key("ours").unwrap()]);
    let sp = Sp { decrypt: &ours, ..SP };
    let xml = posted(&encrypt(&k.signed(&assertion_signed(&k)), &ours[0].certificate_der, Data::Gcm, RSA_OAEP, false));
    // What xmlsec1 1.2 has (Debian bookworm's 1.2.37): AES-GCM and rsa-oaep-mgf1p, no XML Encryption 1.1 OAEP.
    let older = xmlenc::Supported { data: xmlenc::GCM.to_vec(), cbc: xmlenc::CBC.to_vec(), transport: vec![xmlenc::OAEP[0]] };
    assert_eq!(verify_with(&sp, &metadata(&k), &xml, REQUEST, Some(&older)).unwrap().subject, "persistent-7");
    let published = sp_metadata_with(&sp, Some(&older)).unwrap();
    assert!(published.contains("use=\"encryption\"") && !published.contains("xmlenc11#rsa-oaep\""), "{published}");
    // A library without them: no key published, and an encrypted assertion refused in so many words, as before.
    assert!(!sp_metadata_with(&sp, None).unwrap().contains("use=\"encryption\""));
    assert_eq!(verify_with(&sp, &metadata(&k), &xml, REQUEST, None).unwrap_err(), xmlenc::OFF);
    assert_eq!(
        verify_with(&sp, &metadata(&k), &posted(&k.signed(&response(&k, |_| {}))), REQUEST, None).unwrap().subject,
        "persistent-7",
        "plain ones as before"
    );
}
