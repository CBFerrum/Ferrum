//! A build without SAML: the same calls, each saying so.

use crate::{Incoming, Logout, NameId, Outgoing, Person, Sp, SpKey};

const OFF: &str = "this server was built without SAML (the saml feature)";

#[derive(Debug, Clone)]
pub struct Idp {
    pub entity_id: String,
    pub sso_url: String,
    pub slo: Option<(String, String, Option<String>)>,
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

pub fn new_sp_key(_name: &str) -> Result<SpKey, String> {
    Err(OFF.into())
}

pub fn self_check() -> Result<(), String> {
    Err(OFF.into())
}

pub fn logout_request(_sp: &Sp, _idp: &Idp, _id: &str, _name_id: &NameId, _session_index: Option<&str>, _relay_state: &str) -> Result<Outgoing, String> {
    Err(OFF.into())
}

pub fn logout_response(_sp: &Sp, _idp: &Idp, _id: &str, _in_response_to: &str, _success: bool, _relay_state: &str) -> Result<Outgoing, String> {
    Err(OFF.into())
}

pub fn peek_issuer(_incoming: Incoming) -> Result<String, String> {
    Err(OFF.into())
}

pub fn read_logout(_sp: &Sp, _idp: &Idp, _incoming: Incoming) -> Result<Logout, String> {
    Err(OFF.into())
}
