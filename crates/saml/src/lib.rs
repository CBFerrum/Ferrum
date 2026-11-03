//! SAML 2.0 single sign-on, the service provider's side of the Web Browser SSO profile: an identity
//! provider's metadata read, a sign-in request sent by HTTP-Redirect, and the response it posts back
//! checked. Signatures are checked by xmlsec1, through samael, which keeps only what a valid
//! signature covers, so a signed element wrapped in unsigned ones proves nothing. This adds what the
//! profile requires and samael leaves to its caller: a signing certificate in the metadata (without
//! one samael checks no signature at all), an audience naming this service, and a subject.
//!
//! Built without the `xmlsec` feature nothing native is linked, and every call says so.

use serde::{Deserialize, Serialize};

#[cfg(feature = "xmlsec")]
mod checked;
#[cfg(feature = "xmlsec")]
pub use checked::{Idp, new_sp_key, request_url, self_check, sp_metadata, verify};

#[cfg(all(feature = "xmlsec", any(test, feature = "testing")))]
pub mod testing;

#[cfg(not(feature = "xmlsec"))]
mod off;
#[cfg(not(feature = "xmlsec"))]
pub use off::{Idp, new_sp_key, request_url, self_check, sp_metadata, verify};

/// Whether this build checks SAML at all.
pub const AVAILABLE: bool = cfg!(feature = "xmlsec");

/// This service as identity providers know it.
#[derive(Debug, Clone, Copy)]
pub struct Sp<'a> {
    pub entity_id: &'a str,
    /// Where responses are posted (HTTP-POST binding).
    pub acs_url: &'a str,
    /// Its own key: with one, requests are signed and assertions encrypted to it can be read.
    pub key: Option<&'a SpKey>,
}

/// A service's own SAML key: RSA, with a self-signed certificate its metadata publishes, which
/// providers check its requests with and encrypt assertions to.
#[derive(Clone)]
pub struct SpKey {
    /// PKCS#8 or PKCS#1, DER.
    pub private_der: Vec<u8>,
    pub certificate_der: Vec<u8>,
}

impl std::fmt::Debug for SpKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpKey(…)")
    }
}

/// Who signed in, as the identity provider vouches for them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    /// The same person at the provider every time: the NameID, or the email address when the NameID
    /// is transient (a new one each sign-in).
    pub subject: String,
    pub email: Option<String>,
    pub name: Option<String>,
}

/// Reading who an assertion names (used only where SAML is built in).
#[cfg_attr(not(feature = "xmlsec"), allow(dead_code))]
pub(crate) mod people {
    use super::Person;

    pub(crate) const TRANSIENT: &str = "urn:oasis:names:tc:SAML:2.0:nameid-format:transient";
    pub(crate) const EMAIL_FORMAT: &str = "urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress";
    pub(crate) const UNSPECIFIED: &str = "urn:oasis:names:tc:SAML:1.1:nameid-format:unspecified";

    /// Attribute names providers use, compared without case: Microsoft Entra ID and AD FS, Okta,
    /// OneLogin, Google, Keycloak's X.500 mappers, and the LDAP object identifiers.
    const EMAIL: [&str; 7] = [
        "email",
        "mail",
        "emailaddress",
        "user.email",
        "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/emailaddress",
        "urn:oid:0.9.2342.19200300.100.1.3",
        "urn:oid:1.2.840.113549.1.9.1",
    ];
    // Not Entra's `…/claims/name`, which is the user principal name.
    const NAME: [&str; 5] = ["displayname", "name", "cn", "http://schemas.microsoft.com/identity/claims/displayname", "urn:oid:2.16.840.1.113730.3.1.241"];
    const GIVEN: [&str; 5] = ["givenname", "firstname", "first_name", "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/givenname", "urn:oid:2.5.4.42"];
    const FAMILY: [&str; 6] = ["sn", "surname", "lastname", "last_name", "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/surname", "urn:oid:2.5.4.4"];

    /// One attribute as the assertion states it: its name, friendly name and values.
    pub(crate) type Attribute<'a> = (Option<&'a str>, Option<&'a str>, Vec<&'a str>);

    /// The first value of the first attribute named (in the order of `names`) that `fits`.
    fn first<'a>(attributes: &[Attribute<'a>], names: &[&str], fits: fn(&str) -> bool) -> Option<&'a str> {
        names.iter().find_map(|wanted| {
            attributes.iter().find_map(|(name, friendly, values)| {
                let named = [name, friendly].into_iter().flatten().any(|n| n.trim().eq_ignore_ascii_case(wanted));
                if named { values.iter().map(|v| v.trim()).find(|v| fits(v)) } else { None }
            })
        })
    }

    fn some_text(s: &str) -> bool {
        !s.is_empty()
    }

    fn looks_like_email(s: &str) -> bool {
        s.len() <= 254 && !s.contains(char::is_whitespace) && s.split_once('@').is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
    }

    /// The person an assertion names, from its NameID (value and format) and attributes.
    pub(crate) fn person(name_id: Option<(&str, Option<&str>)>, attributes: &[Attribute]) -> Result<Person, String> {
        let (value, format) = name_id.map(|(v, f)| (v.trim(), f.map(str::trim))).ok_or("the assertion names nobody (it has no NameID)")?;
        let from_name_id = matches!(format, None | Some(EMAIL_FORMAT) | Some(UNSPECIFIED)) && looks_like_email(value);
        let email = first(attributes, &EMAIL, looks_like_email).or(from_name_id.then_some(value)).map(str::to_lowercase);
        let subject = match format {
            Some(TRANSIENT) => email.clone().ok_or("the provider sent a transient NameID and no email address, so nobody can be recognised")?,
            _ if value.is_empty() => return Err("the assertion's NameID is empty".into()),
            _ => value.to_string(),
        };
        if subject.len() > 255 {
            return Err("the assertion's NameID is too long".into());
        }
        let name = first(attributes, &NAME, some_text).map(str::to_string).or_else(|| {
            let whole = [first(attributes, &GIVEN, some_text), first(attributes, &FAMILY, some_text)].into_iter().flatten().collect::<Vec<_>>().join(" ");
            (!whole.is_empty()).then_some(whole)
        });
        Ok(Person { subject, email, name })
    }
}

#[cfg(test)]
mod tests {
    use super::people::*;
    use super::*;

    fn attrs<'a>(pairs: &[(&'a str, &'a str)]) -> Vec<Attribute<'a>> {
        pairs.iter().map(|(n, v)| (Some(*n), None, vec![*v])).collect()
    }

    #[test]
    fn people_are_read_the_way_providers_send_them() {
        let entra = attrs(&[
            ("http://schemas.xmlsoap.org/ws/2005/05/identity/claims/name", "pat@contoso.test"),
            ("http://schemas.xmlsoap.org/ws/2005/05/identity/claims/emailaddress", "Pat.Lee@Contoso.test"),
            ("http://schemas.microsoft.com/identity/claims/displayname", "Pat Lee"),
        ]);
        let p = person(Some(("AAAAAAAAAAAAAAAAAAAAAMa1", Some("urn:oasis:names:tc:SAML:2.0:nameid-format:persistent"))), &entra).unwrap();
        assert_eq!(p, Person { subject: "AAAAAAAAAAAAAAAAAAAAAMa1".into(), email: Some("pat.lee@contoso.test".into()), name: Some("Pat Lee".into()) });

        let keycloak = attrs(&[("urn:oid:1.2.840.113549.1.9.1", "kim@acme.test"), ("urn:oid:2.5.4.42", "Kim"), ("urn:oid:2.5.4.4", "Ito")]);
        let p = person(Some(("kim", Some(UNSPECIFIED))), &keycloak).unwrap();
        assert_eq!((p.subject.as_str(), p.email.as_deref(), p.name.as_deref()), ("kim", Some("kim@acme.test"), Some("Kim Ito")));

        let google = person(Some(("Sam@Acme.test", Some(EMAIL_FORMAT))), &[]).unwrap();
        assert_eq!((google.subject.as_str(), google.email.as_deref(), google.name), ("Sam@Acme.test", Some("sam@acme.test"), None));
    }

    #[test]
    fn a_transient_name_id_is_not_a_person() {
        let p = person(Some(("_8e3c", Some(TRANSIENT))), &attrs(&[("mail", "ana@acme.test")])).unwrap();
        assert_eq!(p.subject, "ana@acme.test", "the address stands in for a NameID that changes every time");
        assert!(person(Some(("_8e3c", Some(TRANSIENT))), &[]).is_err());
        assert!(person(None, &attrs(&[("mail", "ana@acme.test")])).is_err());
        assert!(person(Some(("  ", None)), &[]).is_err());
        assert!(person(Some((&"x".repeat(256), None)), &[]).is_err());
    }

    #[test]
    fn only_addresses_count_as_addresses() {
        let p = person(Some(("u1", None)), &attrs(&[("email", "not an address"), ("mail", "ok@acme.test")])).unwrap();
        assert_eq!(p.email.as_deref(), Some("ok@acme.test"));
        let p = person(Some(("persistent-id", Some("urn:oasis:names:tc:SAML:2.0:nameid-format:persistent"))), &[]).unwrap();
        assert_eq!(p.email, None, "a persistent NameID is never read as an address");
        let p = person(Some(("pat@acme.test", Some("urn:oasis:names:tc:SAML:2.0:nameid-format:kerberos"))), &[]).unwrap();
        assert_eq!(p.email, None, "nor is a NameID of another kind");
        assert_eq!(
            person(Some(("u", None)), &[(None, Some("Email"), vec!["", " x@y.test "])]).unwrap().email.as_deref(),
            Some("x@y.test"),
            "friendly names count"
        );
    }
}
