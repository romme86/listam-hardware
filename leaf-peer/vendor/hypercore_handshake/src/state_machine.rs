//! State machine for creating a Noise IK and XX patterns (using a typestate pattern)
//!
//! I originally chose to use a typestates here when there was just one pattern, because it made
//! state transitions obvious and brought the flow of the protocol into the typesystem. However, it
//! is **a lot** of code.
//!
//!
//! IK Pattern
//!
//! ```text
//! Initiator:
//! SecStream<Initiator<IK, Start>>
//!   → write_msg()
//!   → SecStream<Initiator<IK, HsMsgSent>>
//!   → read_msg()
//!   → SecStream<Initiator<IK, HsDone>>
//!   → write_msg()
//!   → SecStream<EncryptorReady>
//!   → read_msg()
//!   → SecStream<Ready>
//!
//! Responder:
//! SecStream<Responder<IK, Start>>
//!   → read_msg()
//!   → SecStream<Responder<IK, HsDone>>
//!   → write_msg() → [handshake_msg, setup_msg]
//!   → SecStream<EncryptorReady>
//!   → read_msg()
//!   → SecStream<Ready>
//!```
//!
//! XX Pattern
//!
//!```text
//! Initiator:
//! SecStream<Initiator<XX, Start>>
//!   → write_msg()
//!   → SecStream<Initiator<XX, HsMsgSent>>
//!   → read_msg()
//!   → SecStream<Initiator<XX, InitiatorXxFinalMsg>>
//!   → write_msg() (third handshake message)
//!   → SecStream<Initiator<XX, HsDone>>
//!   → write_msg() (setup message)
//!   → SecStream<EncryptorReady>
//!   → read_msg()
//!   → SecStream<Ready>
//!
//! Responder:
//! SecStream<Responder<XX, Start>>
//!   → read_msg()
//!   → SecStream<Responder<XX, ResponderXxAwaitingFinal>>
//!   → write_msg() → [handshake_msg, empty_vec]
//!   → SecStream<Responder<XX, ResponderXxAwaitingFinal>>
//!   → read_msg() (third handshake message)
//!   → SecStream<Responder<XX, HsDone>>
//!   → write_msg() → setup_msg (Vec<u8>)
//!   → SecStream<EncryptorReady>
//!   → read_msg()
//!   → SecStream<Ready>
//! ```
//!
//! The flow for IK looks like this:
//! ```
//! // Excessive typing to demonstrate flow through typestates
//! use hypercore_handshake::state_machine::{
//!    EncryptorReady, HsDone, HsMsgSent, Initiator, Ready, Responder, SecStream, Start, IK,
//!    hc_specific::generate_keypair,
//! };
//! let kp: snow::Keypair = generate_keypair()?;
//! // Create an initiator and responder
//! let init: SecStream<Initiator<IK, Start>> =
//!    SecStream::new_initiator_ik(&kp.public.clone().try_into().unwrap(), &[])?;
//! let resp: SecStream<Responder<IK, Start>> = SecStream::new_responder_ik(&kp, &[])?;
//!
//! // initiator sends the first handshake message, a payload can be included to send extra data to the
//! // responder.
//! let (init, msg): (SecStream<Initiator<IK, HsMsgSent>>, Vec<u8>) = init.write_msg(Some(b"one"))?;
//!
//! // responder receives the hs message, extracts the payload
//! let (resp, payload): (SecStream<Responder<IK, HsDone>>, Vec<u8>) = resp.read_msg(&msg)?;
//! assert_eq!(payload, b"one");
//!
//! // responder sends a handshake message, which can include a payload. As well as a second
//! // message which contains the symmetric key needed to set up the decryptor
//! let (resp, [msg1, msg2]): (SecStream<EncryptorReady>, [Vec<u8>; 2]) =
//!    resp.write_msg(Some(b"two"))?;
//!
//! // Initiator receives last handshake message, use handshake to create the extract payload.
//! let (init, payload_recv): (SecStream<Initiator<IK, HsDone>>, Vec<u8>) = init.read_msg(&msg1)?;
//! assert_eq!(payload_recv, b"two");
//!
//! // receive decryptor keey
//! let (init, to_resp_final): (SecStream<EncryptorReady>, Vec<u8>) = init.write_msg()?;
//!
//! // finalize both sides
//! let mut init: SecStream<Ready> = init.read_msg(&msg2)?;
//! let mut resp: SecStream<Ready> = resp.read_msg(&to_resp_final)?;
//!
//! // Now both sides can send and receive messages
//! let mut msg = b"three".to_vec();
//! init.push(&mut msg, &[], crypto_secretstream::Tag::Message)?;
//! let tag = resp.pull(&mut msg, &[])?;
//! assert_eq!(msg, b"three");
//! Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![expect(
    clippy::type_complexity,
    reason = "Using the type definitions would obscure the very types I'm trying to show"
)]
use crypto_secretstream::{Header, Key, PullStream, PushStream, Tag};
use rand::rngs::OsRng;
use snow::{HandshakeState, Keypair};
use std::{fmt::Debug, marker::PhantomData};
use tracing::error;

use crate::{Error, crypto::write_stream_id};

/// NB: This is what the params SHOULD be, but hypercore uses "..Ed25519.."
//pub const PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2b";
const STREAM_ID_LENGTH: usize = 32;
const RAW_HEADER_MSG_LEN: usize = STREAM_ID_LENGTH + Header::BYTES;
const SNOW_CIPHERKEYLEN: usize = 32;
/// Length in bytes of a public key
pub const PUBLIC_KEYLEN: usize = 32;

/// Noise handshake pattern to use
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HandshakePattern {
    /// IK pattern - Initiator knows responder's static public key
    #[default]
    IK,
    /// XX pattern - Mutual authentication, neither party knows the other's key beforehand
    XX,
}

/// Pattern marker types for compile-time pattern tracking
/// IK pattern - Initiator knows responder's static public key
#[derive(Debug)]
pub struct IK;

/// XX pattern - Mutual authentication, neither party knows the other's key beforehand
#[derive(Debug)]
pub struct XX;

/// Secret Stream protocol state
pub struct SecStream<Step> {
    is_initiator: bool,
    pattern: HandshakePattern, // Runtime pattern tracking for snow library
    state: HandshakeState,
    local_public_key: [u8; PUBLIC_KEYLEN],
    msg_buf: [u8; 1024],
    step: Step,
}

impl<Step: Debug> std::fmt::Debug for SecStream<Step> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecStream")
            .field("is_initiator", &self.is_initiator)
            .field("step", &self.step)
            .finish()
    }
}

impl<Step> SecStream<Step> {
    /// split handshake into (tx, rx)
    pub fn split_handshake(&mut self) -> ([u8; SNOW_CIPHERKEYLEN], [u8; SNOW_CIPHERKEYLEN]) {
        let (a, b) = self.state.dangerously_get_raw_split();
        if self.is_initiator { (a, b) } else { (b, a) }
    }

    /// Get the local public key.
    pub fn get_local_public_key(&self) -> [u8; PUBLIC_KEYLEN] {
        self.local_public_key
    }

    /// Get the handshake pattern being used
    pub fn pattern(&self) -> HandshakePattern {
        self.pattern
    }

    /// Get the remote peer's static public key.
    ///
    /// For Responders this is `None` until processing reading the first handshake message
    /// For Initiators using IK pattern, this is always `Some(_)` because IK requires the Initiator
    /// to know the Responder's public key beforehand.
    /// For Initiators using XX pattern, this is `None` until the responder reveals their key.
    pub fn get_remote_static(&self) -> Option<[u8; PUBLIC_KEYLEN]> {
        self.state.get_remote_static().map(|bytes| {
                bytes
                    .try_into().inspect_err(|error| error!(?error, "snow gave us a key with the wrong size? Expected length = [{PUBLIC_KEYLEN}] but got length = [{}]", bytes.len()))
                    .expect("snow gave us a key with the wrong size?")
        })
    }

    /// If this is the initiator
    pub fn is_initiator(&self) -> bool {
        self.is_initiator
    }
}

/// Initiator with pattern and step tracking
pub struct Initiator<Pattern, Step> {
    _pattern: PhantomData<Pattern>,
    _step: PhantomData<Step>,
}

impl<Pattern: 'static, Step: 'static> Debug for Initiator<Pattern, Step> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pattern = std::any::type_name::<Pattern>()
            .rsplit("::")
            .next()
            .unwrap_or("?");
        let step = std::any::type_name::<Step>()
            .rsplit("::")
            .next()
            .unwrap_or("?");
        write!(f, "Initiator[{pattern}]({})", step)
    }
}

/// Responder with pattern and step tracking
pub struct Responder<Pattern, Step> {
    _pattern: PhantomData<Pattern>,
    _step: PhantomData<Step>,
}

impl<Pattern: 'static, Step: 'static> Debug for Responder<Pattern, Step> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pattern = std::any::type_name::<Pattern>()
            .rsplit("::")
            .next()
            .unwrap_or("?");
        let step = std::any::type_name::<Step>()
            .rsplit("::")
            .next()
            .unwrap_or("?");
        write!(f, "Responder[{pattern}]({})", step)
    }
}
/// The first step. We must send or receive a handshake message to proceed.
#[derive(Debug)]
pub struct Start;

/// The handshake message has been sent. We must receive a handshake message to proceed.
/// Only on [`Initiator`].
#[derive(Debug)]
pub struct HsMsgSent;

/// XX-specific: Initiator has received responder's second message and must send the final handshake message.
/// Only for XX pattern on [`Initiator<XX, _>`].
#[derive(Debug)]
pub struct InitiatorXxFinalMsg;

/// XX-specific: Responder has received the first message and must send the handshake response.
/// Only for XX pattern on [`Responder<XX, _>`].
#[derive(Debug)]
pub struct ResponderXxReceivedFirst;

/// XX-specific: Responder has sent handshake response and is awaiting initiator's final message.
/// Only for XX pattern on [`Responder<XX, _>`].
#[derive(Debug)]
pub struct ResponderXxAwaitingFinal;

/// [`snow::HandshakeState::is_handshake_finished`] is `true`.
/// We are ready to create a [`PushStream`] and proceed to [`EncryptorReady`].
#[derive(Debug)]
pub struct HsDone;

/// No decryptor yet
pub struct EncryptorReady {
    rx: Key,
    pusher: PushStream,
    handshake_hash: Vec<u8>,
}

/// Encryptor and decryptor
pub struct Ready {
    puller: PullStream,
    pusher: PushStream,
    handshake_hash: Vec<u8>,
}
impl Debug for EncryptorReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptorReady").finish()
    }
}
impl Debug for Ready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ready").finish()
    }
}
pub mod hc_specific {
    //! Stuff for generating Hypercore specific things like Noise parameters, keys, etc

    use crate::Error;
    use std::sync::LazyLock;

    pub use snow::Keypair;
    use snow::{
        Builder,
        params::{BaseChoice, HandshakeChoice, NoiseParams},
        resolvers::{DefaultResolver, FallbackResolver},
    };

    /// The Hypercore IK parameter string
    const IK_PARAM_STR: &str = "Noise_IK_Ed25519_ChaChaPoly_BLAKE2b";
    /// The Hypercore XX parameter string
    const XX_PARAM_STR: &str = "Noise_XX_Ed25519_ChaChaPoly_BLAKE2b";

    static IK_NOISE_PARAMS: LazyLock<NoiseParams> = LazyLock::new(|| {
        NoiseParams::new(
            IK_PARAM_STR.to_string(),
            BaseChoice::Noise,
            HandshakeChoice {
                pattern: snow::params::HandshakePattern::IK,
                modifiers: snow::params::HandshakeModifierList { list: vec![] },
            },
            snow::params::DHChoice::Curve25519,
            snow::params::CipherChoice::ChaChaPoly,
            snow::params::HashChoice::Blake2b,
        )
    });

    static XX_NOISE_PARAMS: LazyLock<NoiseParams> = LazyLock::new(|| {
        NoiseParams::new(
            XX_PARAM_STR.to_string(),
            BaseChoice::Noise,
            HandshakeChoice {
                pattern: snow::params::HandshakePattern::XX,
                modifiers: snow::params::HandshakeModifierList { list: vec![] },
            },
            snow::params::DHChoice::Curve25519,
            snow::params::CipherChoice::ChaChaPoly,
            snow::params::HashChoice::Blake2b,
        )
    });

    /// Get Hypercore Noise parameters for the specified pattern.
    fn noise_params(pattern: crate::HandshakePattern) -> &'static NoiseParams {
        match pattern {
            crate::HandshakePattern::IK => &IK_NOISE_PARAMS,
            crate::HandshakePattern::XX => &XX_NOISE_PARAMS,
        }
    }

    pub(super) fn builder(pattern: crate::HandshakePattern) -> Builder<'static> {
        let params = noise_params(pattern);
        Builder::with_resolver(
            params.clone(),
            //Box::new(DefaultResolver::default()),
            Box::new(FallbackResolver::new(
                Box::<crate::crypto::CurveResolver>::default(),
                Box::<DefaultResolver>::default(),
            )),
        )
    }

    /// Generate Hypercore key pair.
    pub fn generate_keypair() -> Result<Keypair, Error> {
        // Use IK pattern for backward compatibility
        Ok(builder(crate::HandshakePattern::default()).generate_keypair()?)
    }
}

impl SecStream<Initiator<IK, Start>> {
    /// Create an initiator using the IK pattern (requires knowing remote's public key)
    pub fn new_initiator_ik(
        remote_public_key: &[u8; PUBLIC_KEYLEN],
        prologue: &[u8],
    ) -> Result<Self, Error> {
        let key_pair = hc_specific::generate_keypair()?;

        let state = hc_specific::builder(HandshakePattern::IK)
            .prologue(prologue)?
            .local_private_key(&key_pair.private)?
            .remote_public_key(remote_public_key.as_slice())?
            .build_initiator()?;

        Ok(Self {
            is_initiator: true,
            pattern: HandshakePattern::IK,
            state,
            local_public_key: key_pair
                .public
                .try_into()
                .expect("Wrong sized key from snow?"),
            msg_buf: [0; 1024],
            step: Initiator {
                _pattern: PhantomData,
                _step: PhantomData,
            },
        })
    }

    /// Create the first message the initiator sends to the responder (IK pattern)
    pub fn write_msg(
        mut self,
        payload: Option<&[u8]>,
    ) -> Result<(SecStream<Initiator<IK, HsMsgSent>>, Vec<u8>), Error> {
        let payload = payload.unwrap_or_default();
        let len = self.state.write_message(payload, &mut self.msg_buf)?;
        let msg = self.msg_buf[..len].to_vec();
        let Self {
            is_initiator,
            pattern,
            state,
            msg_buf,
            local_public_key,
            ..
        } = self;
        Ok((
            SecStream {
                is_initiator,
                pattern,
                state,
                local_public_key,
                msg_buf,
                step: Initiator {
                    _pattern: PhantomData,
                    _step: PhantomData,
                },
            },
            msg,
        ))
    }
}

impl SecStream<Initiator<XX, Start>> {
    /// Create an initiator using the XX pattern (anonymous handshake)
    pub fn new_initiator_xx(prologue: &[u8]) -> Result<Self, Error> {
        let key_pair = hc_specific::generate_keypair()?;

        let state = hc_specific::builder(HandshakePattern::XX)
            .prologue(prologue)?
            .local_private_key(&key_pair.private)?
            .build_initiator()?;

        Ok(Self {
            is_initiator: true,
            pattern: HandshakePattern::XX,
            state,
            local_public_key: key_pair
                .public
                .try_into()
                .expect("Wrong sized key from snow?"),
            msg_buf: [0; 1024],
            step: Initiator {
                _pattern: PhantomData,
                _step: PhantomData,
            },
        })
    }

    /// Create the first message the initiator sends to the responder (XX pattern)
    pub fn write_msg(
        mut self,
        payload: Option<&[u8]>,
    ) -> Result<(SecStream<Initiator<XX, HsMsgSent>>, Vec<u8>), Error> {
        let payload = payload.unwrap_or_default();
        let len = self.state.write_message(payload, &mut self.msg_buf)?;
        let msg = self.msg_buf[..len].to_vec();
        let Self {
            is_initiator,
            pattern,
            state,
            msg_buf,
            local_public_key,
            ..
        } = self;
        Ok((
            SecStream {
                is_initiator,
                pattern,
                state,
                local_public_key,
                msg_buf,
                step: Initiator {
                    _pattern: PhantomData,
                    _step: PhantomData,
                },
            },
            msg,
        ))
    }
}

impl SecStream<Responder<IK, Start>> {
    /// Create a responder using IK pattern
    pub fn new_responder_ik(keypair: &Keypair, prologue: &[u8]) -> Result<Self, Error> {
        let state = hc_specific::builder(HandshakePattern::IK)
            .prologue(prologue)?
            .local_private_key(&keypair.private)?
            .build_responder()?;
        Ok(Self {
            is_initiator: false,
            pattern: HandshakePattern::IK,
            state,
            local_public_key: keypair
                .public
                .clone()
                .try_into()
                .expect("Wrong sized key from snow?"),
            msg_buf: [0; 1024],
            step: Responder {
                _pattern: PhantomData,
                _step: PhantomData,
            },
        })
    }

    /// Read msg and return it's payload (IK pattern)
    pub fn read_msg(
        mut self,
        msg: &[u8],
    ) -> Result<(SecStream<Responder<IK, HsDone>>, Vec<u8>), Error> {
        let len = self.state.read_message(msg, &mut self.msg_buf)?;
        let payload = &self.msg_buf[..len];
        let Self {
            is_initiator,
            pattern,
            state,
            msg_buf,
            local_public_key,
            ..
        } = self;
        Ok((
            SecStream {
                is_initiator,
                pattern,
                state,
                local_public_key,
                msg_buf,
                step: Responder {
                    _pattern: PhantomData,
                    _step: PhantomData,
                },
            },
            payload.to_vec(),
        ))
    }

    /// Read the first message of the protocol, create the next two messages to send to the initiator.
    pub fn read_and_write_msg(
        self,
        msg: &[u8],
    ) -> Result<(SecStream<EncryptorReady>, [Vec<u8>; 2]), Error> {
        let (self2, _rx_payload) = self.read_msg(msg)?;
        self2.write_msg(Some(&[]))
    }
}

impl SecStream<Responder<XX, Start>> {
    /// Create a responder using XX pattern
    pub fn new_responder_xx(keypair: &Keypair, prologue: &[u8]) -> Result<Self, Error> {
        let state = hc_specific::builder(HandshakePattern::XX)
            .prologue(prologue)?
            .local_private_key(&keypair.private)?
            .build_responder()?;
        Ok(Self {
            is_initiator: false,
            pattern: HandshakePattern::XX,
            state,
            local_public_key: keypair
                .public
                .clone()
                .try_into()
                .expect("Wrong sized key from snow?"),
            msg_buf: [0; 1024],
            step: Responder {
                _pattern: PhantomData,
                _step: PhantomData,
            },
        })
    }

    /// Read first message (XX pattern)
    pub fn read_msg(
        mut self,
        msg: &[u8],
    ) -> Result<(SecStream<Responder<XX, ResponderXxReceivedFirst>>, Vec<u8>), Error> {
        let len = self.state.read_message(msg, &mut self.msg_buf)?;
        let payload = &self.msg_buf[..len];
        let Self {
            is_initiator,
            pattern,
            state,
            msg_buf,
            local_public_key,
            ..
        } = self;
        Ok((
            SecStream {
                is_initiator,
                pattern,
                state,
                local_public_key,
                msg_buf,
                step: Responder {
                    _pattern: PhantomData,
                    _step: PhantomData,
                },
            },
            payload.to_vec(),
        ))
    }
}

impl SecStream<Responder<XX, ResponderXxReceivedFirst>> {
    /// Write handshake response (XX pattern) - returns [handshake_msg, empty_vec]
    pub fn write_msg(
        mut self,
        payload: Option<&[u8]>,
    ) -> Result<
        (
            SecStream<Responder<XX, ResponderXxAwaitingFinal>>,
            [Vec<u8>; 2],
        ),
        Error,
    > {
        let payload = payload.unwrap_or_default();
        let len = self.state.write_message(payload, &mut self.msg_buf)?;
        let hs_msg = self.msg_buf[..len].to_vec();

        // Handshake is NOT finished yet - awaiting initiator's third message
        assert!(
            !self.state.is_handshake_finished(),
            "XX handshake should not be finished yet"
        );

        let Self {
            is_initiator,
            pattern,
            state,
            msg_buf,
            local_public_key,
            ..
        } = self;

        Ok((
            SecStream {
                is_initiator,
                pattern,
                state,
                local_public_key,
                msg_buf,
                step: Responder {
                    _pattern: PhantomData,
                    _step: PhantomData,
                },
            },
            [hs_msg, Vec::new()], // Second vec is empty - setup message comes later
        ))
    }
}

impl SecStream<Responder<XX, ResponderXxAwaitingFinal>> {
    /// Read third handshake message from initiator (XX pattern)
    pub fn read_msg(
        mut self,
        msg: &[u8],
    ) -> Result<(SecStream<Responder<XX, HsDone>>, Vec<u8>), Error> {
        let len = self.state.read_message(msg, &mut self.msg_buf)?;
        let payload = &self.msg_buf[..len];

        // NOW the handshake should be finished
        assert!(
            self.state.is_handshake_finished(),
            "XX handshake should be finished after third message"
        );

        let Self {
            is_initiator,
            pattern,
            state,
            msg_buf,
            local_public_key,
            ..
        } = self;

        Ok((
            SecStream {
                is_initiator,
                pattern,
                state,
                local_public_key,
                msg_buf,
                step: Responder {
                    _pattern: PhantomData,
                    _step: PhantomData,
                },
            },
            payload.to_vec(),
        ))
    }
}

impl SecStream<Responder<IK, HsDone>> {
    /// Make second message with the given payload. Returns two messages, the first completes the
    /// Noise handshake. The second has the shared key for the remote to set up a Decryptor.
    pub fn write_msg(
        mut self,
        payload: Option<&[u8]>,
    ) -> Result<(SecStream<EncryptorReady>, [Vec<u8>; 2]), Error> {
        let payload = payload.unwrap_or_default();
        let len = self.state.write_message(payload, &mut self.msg_buf)?;
        let hs_msg = self.msg_buf[..len].to_vec();

        // For IK pattern, handshake is finished after responder sends message
        assert!(
            self.state.is_handshake_finished(),
            "IK handshake should be finished after responder's message"
        );

        let handshake_hash = self.state.get_handshake_hash().to_vec();
        let mut msg: [u8; RAW_HEADER_MSG_LEN] = [0; RAW_HEADER_MSG_LEN];
        // write stream id to front of pull_stream_msg
        write_stream_id(
            &handshake_hash,
            self.is_initiator,
            &mut msg[..STREAM_ID_LENGTH],
        );

        let (tx, rx) = self.split_handshake();
        let (header, pusher) = PushStream::init(OsRng, &Key::from(tx));

        // write push header to back of pull_stream_msg
        msg[STREAM_ID_LENGTH..].copy_from_slice(header.as_ref());

        let Self {
            is_initiator,
            pattern,
            state,
            msg_buf,
            local_public_key,
            ..
        } = self;

        Ok((
            SecStream {
                is_initiator,
                pattern,
                state,
                local_public_key,
                msg_buf,
                step: EncryptorReady {
                    rx: Key::from(rx),
                    pusher,
                    handshake_hash,
                },
            },
            [hs_msg, msg.to_vec()],
        ))
    }
}

impl SecStream<Responder<XX, HsDone>> {
    /// Send setup message (XX pattern) - handshake is already complete
    pub fn write_msg(mut self) -> Result<(SecStream<EncryptorReady>, Vec<u8>), Error> {
        // Handshake should already be finished
        assert!(
            self.state.is_handshake_finished(),
            "XX handshake should be finished before sending setup"
        );

        let handshake_hash = self.state.get_handshake_hash().to_vec();
        let mut msg: [u8; RAW_HEADER_MSG_LEN] = [0; RAW_HEADER_MSG_LEN];
        // write stream id to front of msg
        write_stream_id(
            &handshake_hash,
            self.is_initiator,
            &mut msg[..STREAM_ID_LENGTH],
        );

        let (tx, rx) = self.split_handshake();
        let (header, pusher) = PushStream::init(OsRng, &Key::from(tx));

        // write push header to back of msg
        msg[STREAM_ID_LENGTH..].copy_from_slice(header.as_ref());

        let Self {
            is_initiator,
            pattern,
            state,
            msg_buf,
            local_public_key,
            ..
        } = self;

        Ok((
            SecStream {
                is_initiator,
                pattern,
                state,
                local_public_key,
                msg_buf,
                step: EncryptorReady {
                    rx: Key::from(rx),
                    pusher,
                    handshake_hash,
                },
            },
            msg.to_vec(),
        ))
    }
}

impl SecStream<Initiator<IK, HsMsgSent>> {
    /// Receive the responder's message (IK pattern)
    pub fn read_msg(
        mut self,
        msg: &[u8],
    ) -> Result<(SecStream<Initiator<IK, HsDone>>, Vec<u8>), Error> {
        let len = self.state.read_message(msg, &mut self.msg_buf)?;
        let payload = &self.msg_buf[..len];

        // For IK, handshake is finished after reading responder's message
        assert!(
            self.state.is_handshake_finished(),
            "IK handshake should be finished"
        );

        let Self {
            is_initiator,
            pattern,
            state,
            local_public_key,
            msg_buf,
            ..
        } = self;
        Ok((
            SecStream {
                is_initiator,
                pattern,
                state,
                local_public_key,
                msg_buf,
                step: Initiator {
                    _pattern: PhantomData,
                    _step: PhantomData,
                },
            },
            payload.to_vec(),
        ))
    }

    /// Read in a message, and write the next message. Any payload in the received message is dropped.
    pub fn read_and_write_msg(
        self,
        msg: &[u8],
    ) -> Result<(SecStream<EncryptorReady>, Vec<u8>), Error> {
        let (self2, _payload) = self.read_msg(msg)?;
        self2.write_msg()
    }
}

impl SecStream<Initiator<XX, HsMsgSent>> {
    /// Receive the responder's message (XX pattern)
    pub fn read_msg(
        mut self,
        msg: &[u8],
    ) -> Result<(SecStream<Initiator<XX, InitiatorXxFinalMsg>>, Vec<u8>), Error> {
        let len = self.state.read_message(msg, &mut self.msg_buf)?;
        let payload = &self.msg_buf[..len];

        // For XX, handshake is NOT finished yet - need to send third message
        assert!(
            !self.state.is_handshake_finished(),
            "XX handshake should not be finished yet"
        );

        let Self {
            is_initiator,
            pattern,
            state,
            local_public_key,
            msg_buf,
            ..
        } = self;
        Ok((
            SecStream {
                is_initiator,
                pattern,
                state,
                local_public_key,
                msg_buf,
                step: Initiator {
                    _pattern: PhantomData,
                    _step: PhantomData,
                },
            },
            payload.to_vec(),
        ))
    }
}

impl SecStream<Initiator<XX, InitiatorXxFinalMsg>> {
    /// Send the third handshake message (XX pattern)
    pub fn write_msg(mut self) -> Result<(SecStream<Initiator<XX, HsDone>>, Vec<u8>), Error> {
        let len = self.state.write_message(&[], &mut self.msg_buf)?;
        let msg = self.msg_buf[..len].to_vec();

        // NOW handshake should be finished
        assert!(
            self.state.is_handshake_finished(),
            "XX handshake should be finished after third message"
        );

        let Self {
            is_initiator,
            pattern,
            state,
            local_public_key,
            msg_buf,
            ..
        } = self;

        Ok((
            SecStream {
                is_initiator,
                pattern,
                state,
                local_public_key,
                msg_buf,
                step: Initiator {
                    _pattern: PhantomData,
                    _step: PhantomData,
                },
            },
            msg,
        ))
    }
}

impl SecStream<Initiator<IK, HsDone>> {
    /// Write the setup message (IK pattern)
    pub fn write_msg(mut self) -> Result<(SecStream<EncryptorReady>, Vec<u8>), Error> {
        // Handshake must be finished
        assert!(
            self.state.is_handshake_finished(),
            "Handshake must be finished before sending setup message"
        );

        let (tx, rx) = self.split_handshake();
        let key: [u8; SNOW_CIPHERKEYLEN] = tx[..SNOW_CIPHERKEYLEN]
            .try_into()
            .expect("split_tx with incorrect length");
        let key = Key::from(key);
        let handshake_hash = self.state.get_handshake_hash().to_vec();
        let (header, pusher) = PushStream::init(OsRng, &key);

        let mut msg: [u8; RAW_HEADER_MSG_LEN] = [0; RAW_HEADER_MSG_LEN];
        // write stream id to front of msg
        write_stream_id(
            &handshake_hash,
            self.is_initiator,
            &mut msg[..STREAM_ID_LENGTH],
        );
        // write push header to back of msg
        msg[STREAM_ID_LENGTH..].copy_from_slice(header.as_ref());

        let SecStream {
            is_initiator,
            pattern,
            state,
            local_public_key,
            msg_buf,
            ..
        } = self;
        Ok((
            SecStream {
                is_initiator,
                pattern,
                state,
                local_public_key,
                msg_buf,
                step: EncryptorReady {
                    pusher,
                    rx: Key::from(rx),
                    handshake_hash,
                },
            },
            msg.to_vec(),
        ))
    }
}

impl SecStream<Initiator<XX, HsDone>> {
    /// Write the setup message (XX pattern)
    pub fn write_msg(mut self) -> Result<(SecStream<EncryptorReady>, Vec<u8>), Error> {
        // Handshake must be finished
        assert!(
            self.state.is_handshake_finished(),
            "Handshake must be finished before sending setup message"
        );

        let (tx, rx) = self.split_handshake();
        let key: [u8; SNOW_CIPHERKEYLEN] = tx[..SNOW_CIPHERKEYLEN]
            .try_into()
            .expect("split_tx with incorrect length");
        let key = Key::from(key);
        let handshake_hash = self.state.get_handshake_hash().to_vec();
        let (header, pusher) = PushStream::init(OsRng, &key);

        let mut msg: [u8; RAW_HEADER_MSG_LEN] = [0; RAW_HEADER_MSG_LEN];
        // write stream id to front of msg
        write_stream_id(
            &handshake_hash,
            self.is_initiator,
            &mut msg[..STREAM_ID_LENGTH],
        );
        // write push header to back of msg
        msg[STREAM_ID_LENGTH..].copy_from_slice(header.as_ref());

        let SecStream {
            is_initiator,
            pattern,
            state,
            local_public_key,
            msg_buf,
            ..
        } = self;
        Ok((
            SecStream {
                is_initiator,
                pattern,
                state,
                local_public_key,
                msg_buf,
                step: EncryptorReady {
                    pusher,
                    rx: Key::from(rx),
                    handshake_hash,
                },
            },
            msg.to_vec(),
        ))
    }
}

impl SecStream<EncryptorReady> {
    /// Get the handshake hash.
    ///
    /// This is a unique identifier for this encrypted session, the same on both sides.
    /// Used for capability verification in hypercore replication.
    pub fn handshake_hash(&self) -> &[u8] {
        &self.step.handshake_hash
    }
    /// Recieve message the last message, used to set up the decryption stream
    pub fn read_msg(self, msg: &[u8]) -> Result<SecStream<Ready>, Error> {
        let Self {
            is_initiator,
            pattern,
            state,
            local_public_key,
            msg_buf,
            step:
                EncryptorReady {
                    pusher,
                    rx,
                    handshake_hash,
                },
        } = self;
        // Read the received message from the other peer
        let mut expected_stream_id: [u8; STREAM_ID_LENGTH] = [0; STREAM_ID_LENGTH];
        write_stream_id(&handshake_hash, !is_initiator, &mut expected_stream_id);
        if expected_stream_id != msg[..STREAM_ID_LENGTH] {
            panic!(
                "stream ID's don't match\n{expected_stream_id:?}\n != \n{:?}",
                &msg[..STREAM_ID_LENGTH]
            );
        }

        let header: [u8; Header::BYTES] =
            msg[STREAM_ID_LENGTH..].try_into().expect("TODO wrong size");
        let puller = PullStream::init(header.into(), &rx);
        Ok(SecStream {
            is_initiator,
            pattern,
            state,
            local_public_key,
            msg_buf,
            step: Ready {
                pusher,
                puller,
                handshake_hash,
            },
        })
    }

    /// Encrypt a message in place
    pub fn push(
        &mut self,
        msg: &mut Vec<u8>,
        associated_data: &[u8],
        tag: Tag,
    ) -> Result<(), Error> {
        Ok(self.step.pusher.push(msg, associated_data, tag)?)
    }
}

impl SecStream<Ready> {
    /// Encrypt a message in place
    pub fn push(
        &mut self,
        msg: &mut Vec<u8>,
        associated_data: &[u8],
        tag: Tag,
    ) -> Result<(), Error> {
        Ok(self.step.pusher.push(msg, associated_data, tag)?)
    }
    /// Decrypt a message in place
    pub fn pull(&mut self, msg: &mut Vec<u8>, associated_data: &[u8]) -> Result<Tag, Error> {
        Ok(self.step.puller.pull(msg, associated_data)?)
    }
    /// Get the handshake hash.
    ///
    /// This is a unique identifier for this encrypted session, the same on both sides.
    /// Used for capability verification in hypercore replication.
    pub fn handshake_hash(&self) -> &[u8] {
        &self.step.handshake_hash
    }
}
