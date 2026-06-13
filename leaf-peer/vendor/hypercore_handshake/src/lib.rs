//! State machine
#![warn(
    unreachable_pub,
    missing_debug_implementations,
    missing_docs,
    redundant_lifetimes,
    unsafe_code,
    non_local_definitions,
    clippy::needless_pass_by_value,
    clippy::needless_pass_by_ref_mut
)]

mod cipher;
mod crypto;
mod error;
pub mod state_machine;

pub use cipher::{Cipher, CipherIo, CipherTrait, Event as CipherEvent};
pub use crypto::snow_keypair_from_secret_and_public;
pub use error::Error;
pub use state_machine::{HandshakePattern, IK, XX};
