//! A build without SAML: the same calls, each saying so.

use crate::{Person, Sp};

const OFF: &str = "this server was built without SAML (the saml feature)";

#[derive(Debug, Clone)]
pub struct Idp {
    pub entity_id: String,
    pub sso_url: String,
}

impl Idp {
    pub fn read(_xml: &str) -> Result<Idp, String> {
        Err(OFF.into())
    }
}

pub fn sp_metadata(_sp: &Sp) -> Result<String, String> {
    Err(OFF.into())
}

pub fn request_url(_sp: &Sp, _idp: &Idp, _request_id: &str, _relay_state: &str) -> Result<String, String> {
    Err(OFF.into())
}

pub fn verify(_sp: &Sp, _idp: &Idp, _response: &str, _request_id: &str) -> Result<Person, String> {
    Err(OFF.into())
}

pub fn self_check() -> Result<(), String> {
    Err(OFF.into())
}
