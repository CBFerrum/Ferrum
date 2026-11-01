//! The adapters. None of them claims a certification: `docs/compatibility.md` is the record of what
//! each was tried on, and today that is simulators only.

pub mod defined;
pub mod operator;
pub mod sim;
pub mod transport;

#[cfg(feature = "modbus")]
pub mod modbus;
#[cfg(feature = "mqtt")]
pub mod mqtt;
#[cfg(feature = "mqtt")]
pub mod vda5050;
