//! Encrypted assertions (XML Encryption 1.1), opened by xmlsec1, the library that checks the
//! signatures, through its C interface (the few calls below; samael's own decryption is not used).
//!
//! Encryption proves nothing about who wrote an assertion: anyone can encrypt to the key this
//! service publishes. So what is opened is trusted only under a provider's signature: either the
//! response's, covering the encrypted assertion and checked before anything is decrypted, or the
//! assertion's own inside the encryption, checked after (checked.rs, `verify`).
//!
//! What reaches xmlsec1 is rebuilt here from what was checked, never the provider's XML as sent: the
//! algorithms on a short list (AES-GCM for the data, RSA-OAEP for its key), the cipher values as
//! base64, and nothing that would make xmlsec1 fetch anything, follow a reference, or look a key up
//! by a certificate the sender chose. AES-CBC is accepted only under a response signature checked
//! first: CBC is open to padding-oracle attacks when what is decrypted may have been changed, and a
//! signed response cannot have been. RSA PKCS#1 v1.5 key transport is never accepted. Once a key is
//! used, every failure says the same thing, so nobody learns which step failed.

use crate::SpKey;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use libxml::bindings::{
    _xmlNode, xmlAddChildList, xmlDocCopyNode, xmlElementType_XML_ELEMENT_NODE, xmlElementType_XML_TEXT_NODE, xmlFreeNode, xmlFreeNodeList,
    xmlParseInNodeContext, xmlReplaceNode, xmlUnlinkNode,
};
use libxml::parser::{Parser, ParserOptions};
use libxml::tree::{Document, Node};
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr::null_mut;

pub(crate) const XENC: &str = "http://www.w3.org/2001/04/xmlenc#";
const XENC11: &str = "http://www.w3.org/2009/xmlenc11#";
const DSIG: &str = "http://www.w3.org/2000/09/xmldsig#";
pub(crate) const ASSERTION: &str = "urn:oasis:names:tc:SAML:2.0:assertion";
pub(crate) const PROTOCOL: &str = "urn:oasis:names:tc:SAML:2.0:protocol";

/// The data: AES-GCM always…
pub(crate) const GCM: [&str; 3] =
    ["http://www.w3.org/2009/xmlenc11#aes128-gcm", "http://www.w3.org/2009/xmlenc11#aes192-gcm", "http://www.w3.org/2009/xmlenc11#aes256-gcm"];
/// …AES-CBC only under a signature checked before decrypting.
pub(crate) const CBC: [&str; 3] =
    ["http://www.w3.org/2001/04/xmlenc#aes128-cbc", "http://www.w3.org/2001/04/xmlenc#aes192-cbc", "http://www.w3.org/2001/04/xmlenc#aes256-cbc"];
/// Its key: RSA-OAEP. Never `rsa-1_5`.
pub(crate) const OAEP: [&str; 2] = ["http://www.w3.org/2001/04/xmlenc#rsa-oaep-mgf1p", "http://www.w3.org/2009/xmlenc11#rsa-oaep"];
const OAEP_DIGESTS: [&str; 4] = [
    "http://www.w3.org/2000/09/xmldsig#sha1",
    "http://www.w3.org/2001/04/xmlenc#sha256",
    "http://www.w3.org/2001/04/xmldsig-more#sha384",
    "http://www.w3.org/2001/04/xmlenc#sha512",
];
const MGFS: [&str; 4] = [
    "http://www.w3.org/2009/xmlenc11#mgf1sha1",
    "http://www.w3.org/2009/xmlenc11#mgf1sha256",
    "http://www.w3.org/2009/xmlenc11#mgf1sha384",
    "http://www.w3.org/2009/xmlenc11#mgf1sha512",
];
const MAX_KEYS: usize = 4;
const MAX_CIPHER: usize = 512 << 10;

/// An encrypted assertion where the library cannot open one: in so many words, as before.
pub(crate) const OFF: &str = "the provider encrypted the assertion, which this server does not read; have it send assertions unencrypted (HTTPS protects them)";
/// Every failure once a key is used.
pub(crate) const UNREADABLE: &str = "the encrypted assertion could not be opened";
/// Anything refused before a key is used: what it is, which the sender chose anyway.
pub(crate) const NOT_ACCEPTED: &str = "the provider encrypted the assertion in a way this server does not accept \
     (AES-GCM, or AES-CBC only when the whole response is signed; the key by RSA-OAEP)";

// xmlsec1's C interface, as far as opening encrypted data. Linked with xmlsec1 by samael's build;
// built with its crypto library loaded at run time (Debian, Ubuntu), the app calls have generic
// names (build.rs says which).
#[allow(non_camel_case_types)]
type xmlSecPtr = *mut c_void;
unsafe extern "C" {
    fn xmlSecKeysMngrCreate() -> xmlSecPtr;
    fn xmlSecKeysMngrDestroy(mngr: xmlSecPtr);
    #[cfg_attr(xmlsec_dynamic, link_name = "xmlSecCryptoAppDefaultKeysMngrInit")]
    fn xmlSecOpenSSLAppDefaultKeysMngrInit(mngr: xmlSecPtr) -> c_int;
    #[cfg_attr(xmlsec_dynamic, link_name = "xmlSecCryptoAppDefaultKeysMngrAdoptKey")]
    fn xmlSecOpenSSLAppDefaultKeysMngrAdoptKey(mngr: xmlSecPtr, key: xmlSecPtr) -> c_int;
    // `xmlSecSize` is `size_t` (or `unsigned int` in old builds, which a `size_t` argument also fills).
    #[cfg_attr(xmlsec_dynamic, link_name = "xmlSecCryptoAppKeyLoadMemory")]
    fn xmlSecOpenSSLAppKeyLoadMemory(
        data: *const u8,
        size: usize,
        format: c_int,
        pwd: *const c_char,
        pwd_callback: *mut c_void,
        pwd_ctx: *mut c_void,
    ) -> xmlSecPtr;
    fn xmlSecKeyDestroy(key: xmlSecPtr);
    fn xmlSecKeySetName(key: xmlSecPtr, name: *const u8) -> c_int;
    fn xmlSecEncCtxCreate(mngr: xmlSecPtr) -> xmlSecPtr;
    fn xmlSecEncCtxDestroy(ctx: xmlSecPtr);
    fn xmlSecEncCtxDecrypt(ctx: xmlSecPtr, node: *mut _xmlNode) -> c_int;
    fn xmlSecTransformIdsGet() -> xmlSecPtr;
    fn xmlSecTransformIdListFindByHref(list: xmlSecPtr, href: *const u8, usage: std::os::raw::c_uint) -> xmlSecPtr;
}
/// `xmlSecTransformUsageEncryptionMethod`, the same in 1.2 and 1.3.
const USAGE_ENCRYPTION: std::os::raw::c_uint = 0x0010;
/// The xmlsec1 this was built against.
pub(crate) const VERSION: &str = env!("FERRUM_XMLSEC_VERSION");

/// What the linked xmlsec1 can open, of what is accepted: asked of the library, algorithm by
/// algorithm, not read from its version. xmlsec1 1.2 (Debian bookworm has 1.2.37, Ubuntu 24.04
/// 1.2.39) has AES-GCM and `rsa-oaep-mgf1p` (with SHA-1 only), but not XML Encryption 1.1's
/// `rsa-oaep`; 1.3 has all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Supported {
    pub data: Vec<&'static str>,
    pub cbc: Vec<&'static str>,
    pub transport: Vec<&'static str>,
}

/// [`Supported`], asked once → or why encrypted assertions are off: the library lacks AES-GCM or
/// RSA-OAEP, without which nothing safe can be opened.
pub fn supported() -> Result<&'static Supported, String> {
    static ASKED: std::sync::OnceLock<Result<Supported, String>> = std::sync::OnceLock::new();
    ASKED
        .get_or_init(|| {
            set_up();
            let has = |href: &str| {
                let href = format!("{href}\0");
                !unsafe { xmlSecTransformIdListFindByHref(xmlSecTransformIdsGet(), href.as_ptr(), USAGE_ENCRYPTION) }.is_null()
            };
            let s = Supported {
                data: GCM.iter().copied().filter(|a| has(a)).collect(),
                cbc: CBC.iter().copied().filter(|a| has(a)).collect(),
                transport: OAEP.iter().copied().filter(|a| has(a)).collect(),
            };
            let lacks: Vec<&str> = [(s.data.is_empty(), "AES-GCM"), (s.transport.is_empty(), "RSA-OAEP")].iter().filter(|l| l.0).map(|l| l.1).collect();
            if lacks.is_empty() {
                Ok(s)
            } else {
                Err(format!(
                    "encrypted assertions are off: the xmlsec1 this server runs ({VERSION}) lacks {} (xmlsec1 1.2.27 or later with its OpenSSL backend has them)",
                    lacks.join(" and ")
                ))
            }
        })
        .as_ref()
        .map_err(Clone::clone)
}
/// `xmlSecKeyDataFormatDer`: the format samael loads the same keys in.
const FORMAT_DER: c_int = 3;
/// The name the key is found by: xmlsec1 1.3 finds keys by name only, unless told to search loosely.
const KEY_NAME: &str = "ferrum-saml";

fn strict() -> ParserOptions<'static> {
    ParserOptions { recover: false, no_net: true, ..ParserOptions::default() }
}

/// Whether `node` is the element `name` in namespace `ns`.
pub(crate) fn is(node: &Node, ns: &str, name: &str) -> bool {
    node.get_name() == name && node.get_namespace().is_some_and(|n| n.get_href() == ns)
}

/// An algorithm attribute that is on `list`.
fn algorithm<'a>(node: &Node, list: &[&'a str]) -> Result<&'a str, String> {
    let a = node.get_attribute("Algorithm").unwrap_or_default();
    list.iter().find(|x| **x == a.trim()).copied().ok_or_else(|| NOT_ACCEPTED.to_string())
}

/// A `CipherData` holding a `CipherValue` (never a `CipherReference`, which xmlsec1 would fetch), as
/// base64 again.
fn cipher_value(node: &Node) -> Result<String, String> {
    let shape = || NOT_ACCEPTED.to_string();
    let [value]: [Node; 1] = node.get_child_elements().try_into().map_err(|_| shape())?;
    if !is(&value, XENC, "CipherValue") {
        return Err(shape());
    }
    let text: String = value.get_content().chars().filter(|c| !c.is_whitespace()).collect();
    if text.len() > MAX_CIPHER {
        return Err(shape());
    }
    Ok(STANDARD.encode(STANDARD.decode(text).map_err(|_| shape())?))
}

/// An `EncryptedKey`, checked and written again without what it said about whose key it is: it names
/// the one key being tried.
fn encrypted_key(node: &Node, s: &Supported) -> Result<String, String> {
    let shape = || NOT_ACCEPTED.to_string();
    let (mut method, mut value) = (None, None);
    for c in node.get_child_elements() {
        if is(&c, XENC, "EncryptionMethod") && method.is_none() {
            let alg = algorithm(&c, &s.transport)?;
            let mut params = String::new();
            for p in c.get_child_elements() {
                if is(&p, DSIG, "DigestMethod") {
                    params.push_str(&format!(r#"<ds:DigestMethod xmlns:ds="{DSIG}" Algorithm="{}"/>"#, algorithm(&p, &OAEP_DIGESTS)?));
                } else if is(&p, XENC11, "MGF") && alg == OAEP[1] {
                    params.push_str(&format!(r#"<xenc11:MGF xmlns:xenc11="{XENC11}" Algorithm="{}"/>"#, algorithm(&p, &MGFS)?));
                } else {
                    return Err(shape());
                }
            }
            method = Some(format!(r#"<xenc:EncryptionMethod Algorithm="{alg}">{params}</xenc:EncryptionMethod>"#));
        } else if is(&c, XENC, "CipherData") && value.is_none() {
            value = Some(cipher_value(&c)?);
        } else if !(is(&c, DSIG, "KeyInfo") || is(&c, XENC, "ReferenceList") || is(&c, XENC, "CarriedKeyName")) {
            return Err(shape());
        }
    }
    let (Some(method), Some(value)) = (method, value) else { return Err(shape()) };
    Ok(format!(
        r#"<xenc:EncryptedKey>{method}<ds:KeyInfo xmlns:ds="{DSIG}"><ds:KeyName>{KEY_NAME}</ds:KeyName></ds:KeyInfo><xenc:CipherData><xenc:CipherValue>{value}</xenc:CipherValue></xenc:CipherData></xenc:EncryptedKey>"#
    ))
}

/// The `EncryptedData` of an `EncryptedAssertion`, rebuilt from what was checked: its keys inside its
/// `KeyInfo` wherever the provider put them (inside, or beside it as Shibboleth does).
fn rebuild(encrypted_assertion: &Node, cbc: bool, s: &Supported) -> Result<String, String> {
    let shape = || NOT_ACCEPTED.to_string();
    let (mut data, mut keys) = (None, Vec::new());
    for c in encrypted_assertion.get_child_elements() {
        if is(&c, XENC, "EncryptedData") && data.is_none() {
            data = Some(c);
        } else if is(&c, XENC, "EncryptedKey") {
            keys.push(encrypted_key(&c, s)?);
        } else {
            return Err(shape());
        }
    }
    let data = data.ok_or_else(shape)?;
    if data.get_attribute("Type").is_some_and(|t| t.trim() != format!("{XENC}Element")) {
        return Err(shape());
    }
    let allowed: Vec<&str> = if cbc { s.data.iter().chain(s.cbc.iter()).copied().collect() } else { s.data.clone() };
    let (mut method, mut value) = (None, None);
    for c in data.get_child_elements() {
        if is(&c, XENC, "EncryptionMethod") && method.is_none() {
            if !c.get_child_elements().is_empty() {
                return Err(shape());
            }
            method = Some(algorithm(&c, &allowed)?);
        } else if is(&c, XENC, "CipherData") && value.is_none() {
            value = Some(cipher_value(&c)?);
        } else if is(&c, DSIG, "KeyInfo") {
            for k in c.get_child_elements() {
                if is(&k, XENC, "EncryptedKey") {
                    keys.push(encrypted_key(&k, s)?);
                } else if !(is(&k, DSIG, "RetrievalMethod") || is(&k, DSIG, "KeyName")) {
                    // A pointer to a key beside it (read above), or a name: neither is followed.
                    return Err(shape());
                }
            }
        } else if !is(&c, XENC, "EncryptionProperties") {
            return Err(shape());
        }
    }
    let (Some(method), Some(value)) = (method, value) else { return Err(shape()) };
    if keys.is_empty() || keys.len() > MAX_KEYS {
        return Err(shape());
    }
    Ok(format!(
        r#"<xenc:EncryptedData xmlns:xenc="{XENC}" Type="{XENC}Element"><xenc:EncryptionMethod Algorithm="{method}"/><ds:KeyInfo xmlns:ds="{DSIG}">{}</ds:KeyInfo><xenc:CipherData><xenc:CipherValue>{value}</xenc:CipherValue></xenc:CipherData></xenc:EncryptedData>"#,
        keys.concat()
    ))
}

/// The response's one assertion, which is encrypted: none plain beside it, no second one.
fn encrypted_assertion(doc: &Document) -> Result<Node, String> {
    let shape = || NOT_ACCEPTED.to_string();
    let root = doc.get_root_element().ok_or_else(shape)?;
    if !is(&root, PROTOCOL, "Response") {
        return Err("the encrypted assertion is not in a response this server can read".into());
    }
    let mut found = None;
    for c in root.get_child_elements() {
        if is(&c, ASSERTION, "Assertion") || (is(&c, ASSERTION, "EncryptedAssertion") && found.is_some()) {
            return Err("the response holds more than one assertion".into());
        }
        if is(&c, ASSERTION, "EncryptedAssertion") {
            found = Some(c);
        }
    }
    found.ok_or_else(shape)
}

/// Makes sure xmlsec1 is set up: samael does it the first time it loads a key, which this asks it
/// to do (with no key, so that fails, after the setup).
fn set_up() {
    use samael::crypto::{CertificateDer, Crypto, CryptoProvider};
    let _ = Crypto::verify_signed_xml("<x/>", &CertificateDer::from(Vec::new()), None);
}

/// Owns what xmlsec1 hands out, and gives it back.
struct Owned(xmlSecPtr, unsafe extern "C" fn(xmlSecPtr));

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { (self.1)(self.0) }
        }
    }
}

/// Decrypts `data` in place with `key`: the decrypted element takes its place.
///
/// # Safety
/// `data` is an `EncryptedData` element of a live document, untouched while this runs; when this
/// succeeds it no longer exists.
unsafe fn decrypt_with(key: &SpKey, data: *mut _xmlNode) -> bool {
    unsafe {
        let mngr = Owned(xmlSecKeysMngrCreate(), xmlSecKeysMngrDestroy);
        if mngr.0.is_null() || xmlSecOpenSSLAppDefaultKeysMngrInit(mngr.0) < 0 {
            return false;
        }
        let k = xmlSecOpenSSLAppKeyLoadMemory(key.private_der.as_ptr(), key.private_der.len(), FORMAT_DER, std::ptr::null(), null_mut(), null_mut());
        if k.is_null() {
            return false;
        }
        let name = format!("{KEY_NAME}\0");
        if xmlSecKeySetName(k, name.as_ptr()) < 0 || xmlSecOpenSSLAppDefaultKeysMngrAdoptKey(mngr.0, k) < 0 {
            xmlSecKeyDestroy(k);
            return false;
        }
        let ctx = Owned(xmlSecEncCtxCreate(mngr.0), xmlSecEncCtxDestroy);
        !ctx.0.is_null() && xmlSecEncCtxDecrypt(ctx.0, data) >= 0
    }
}

/// Whether `n` is a text node of blanks only.
unsafe fn blank(n: *mut _xmlNode) -> bool {
    unsafe {
        (*n).type_ == xmlElementType_XML_TEXT_NODE
            && ((*n).content.is_null() || CStr::from_ptr((*n).content as *const c_char).to_bytes().iter().all(u8::is_ascii_whitespace))
    }
}

/// `response` (a `Response`, its one assertion encrypted to one of `keys`) with the assertion opened
/// in its place. `cbc`: whether a signature over the whole response was checked already.
pub(crate) fn open(response: &str, keys: &[SpKey], cbc: bool, s: &Supported) -> Result<String, String> {
    let parser = Parser::default();
    let rebuilt = {
        let doc = parser.parse_string_with_options(response, strict()).map_err(|_| NOT_ACCEPTED.to_string())?;
        rebuild(&encrypted_assertion(&doc)?, cbc, s)?
    };
    if keys.is_empty() {
        return Err(UNREADABLE.into());
    }
    set_up();
    let doc = parser.parse_string_with_options(response, strict()).map_err(|_| UNREADABLE.to_string())?;
    let holder = encrypted_assertion(&doc)?.node_ptr();
    // Nothing below goes through libxml's wrappers, which would free what xmlsec1 frees.
    unsafe {
        while !(*holder).children.is_null() {
            let c = (*holder).children;
            xmlUnlinkNode(c);
            xmlFreeNode(c);
        }
        // Parsed where it stands, so the namespaces declared around it count.
        let mut list: *mut _xmlNode = null_mut();
        let len = c_int::try_from(rebuilt.len()).map_err(|_| UNREADABLE.to_string())?;
        if xmlParseInNodeContext(holder, rebuilt.as_ptr() as *const c_char, len, 0, &mut list) != 0 || list.is_null() {
            if !list.is_null() {
                xmlFreeNodeList(list);
            }
            return Err(UNREADABLE.into());
        }
        xmlAddChildList(holder, list);
        let data = (*holder).children;
        if !keys.iter().any(|k| decrypt_with(k, data)) {
            return Err(UNREADABLE.into());
        }
        // What came out must be one assertion and nothing else.
        let mut assertion = None;
        let mut c = (*holder).children;
        while !c.is_null() {
            let ns = (*c).ns;
            let element = (*c).type_ == xmlElementType_XML_ELEMENT_NODE;
            let named = element
                && !ns.is_null()
                && CStr::from_ptr((*ns).href as *const c_char).to_bytes() == ASSERTION.as_bytes()
                && CStr::from_ptr((*c).name as *const c_char).to_bytes() == b"Assertion";
            match (named, assertion) {
                (true, None) => assertion = Some(c),
                _ if !element && blank(c) => {}
                _ => return Err(UNREADABLE.into()),
            }
            c = (*c).next;
        }
        let assertion = assertion.ok_or_else(|| UNREADABLE.to_string())?;
        // A copy declares the namespaces it uses that were declared around it.
        let copy = xmlDocCopyNode(assertion, doc.doc_ptr(), 1);
        if copy.is_null() {
            return Err(UNREADABLE.into());
        }
        xmlReplaceNode(holder, copy);
        xmlFreeNode(holder);
    }
    Ok(doc.to_string())
}
