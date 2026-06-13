//! Interface for an encrypted channel
use std::{
    collections::VecDeque,
    fmt::Debug,
    io::Error as IoError,
    mem::replace,
    pin::Pin,
    task::{Context, Poll},
};

use crypto_secretstream::Tag;
use futures::{Sink, Stream};
use tracing::{instrument, trace, warn};

use crate::{
    Error, HandshakePattern, IK, XX,
    state_machine::{
        EncryptorReady, HsMsgSent, Initiator, PUBLIC_KEYLEN, Ready, Responder,
        ResponderXxAwaitingFinal, SecStream, Start,
    },
};

/// Describe's the interface needed [`Cipher`] IO.
pub trait CipherTrait:
    Stream<Item = Event> + Sink<Vec<u8>, Error = std::io::Error> + Unpin + Send + Sync
{
    /// Get the public key of the remote peer
    fn remote_public_key(&self) -> Option<[u8; PUBLIC_KEYLEN]>;
    /// Get the local public key
    fn local_public_key(&self) -> [u8; PUBLIC_KEYLEN];
    /// Get the handshake hash
    fn handshake_hash(&self) -> Option<Vec<u8>>;
    /// `true` if this is the initiator
    fn is_initiator(&self) -> bool;
}

pub(crate) enum State {
    // IK Initiator states
    InitiatorIkStart(SecStream<Initiator<IK, Start>>),
    InitiatorIkSent(SecStream<Initiator<IK, HsMsgSent>>),

    // IK Responder states
    RespIkStart(SecStream<Responder<IK, Start>>),

    // XX Initiator states
    InitiatorXxStart(SecStream<Initiator<XX, Start>>),
    InitiatorXxSent(SecStream<Initiator<XX, HsMsgSent>>),

    // XX Responder states
    RespXxStart(SecStream<Responder<XX, Start>>),
    RespXxAwaitingFinal(SecStream<Responder<XX, ResponderXxAwaitingFinal>>),

    // Common states (pattern-agnostic)
    EncReady(SecStream<EncryptorReady>),
    Ready(SecStream<Ready>),
    Invalid,
}

macro_rules! state_from_ss {
    ($variant:ident, $ss:ty) => {
        impl From<$ss> for State {
            fn from(value: $ss) -> Self {
                State::$variant(value)
            }
        }
        impl From<$ss> for SansIoCipher {
            fn from(value: $ss) -> Self {
                SansIoCipher::new(value.into())
            }
        }
    };
}

state_from_ss!(InitiatorIkStart, SecStream<Initiator<IK, Start>>);
state_from_ss!(RespIkStart, SecStream<Responder<IK, Start>>);
state_from_ss!(InitiatorXxStart, SecStream<Initiator<XX, Start>>);
state_from_ss!(RespXxStart, SecStream<Responder<XX, Start>>);

// Because we're using typestates, and each SecStream is a different type, there's no easy way do something with SecStream that is the same for every type. So we do this:
macro_rules! delegate_to_state {
    ($self:expr, $method:ident, $default:expr) => {
        match $self {
            State::InitiatorIkStart(s) => s.$method(),
            State::InitiatorIkSent(s) => s.$method(),
            State::InitiatorXxStart(s) => s.$method(),
            State::InitiatorXxSent(s) => s.$method(),
            State::RespIkStart(s) => s.$method(),
            State::RespXxStart(s) => s.$method(),
            State::RespXxAwaitingFinal(s) => s.$method(),
            State::EncReady(s) => s.$method(),
            State::Ready(s) => s.$method(),
            State::Invalid => $default,
        }
    };
}

impl Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InitiatorIkStart(s) => f.debug_tuple("InitiatorIkStart").field(s).finish(),
            Self::InitiatorIkSent(s) => f.debug_tuple("InitiatorIkSent").field(s).finish(),
            Self::RespIkStart(s) => f.debug_tuple("RespIkStart").field(s).finish(),
            Self::InitiatorXxStart(s) => f.debug_tuple("InitiatorXxStart").field(s).finish(),
            Self::InitiatorXxSent(s) => f.debug_tuple("InitiatorXxSent").field(s).finish(),
            Self::RespXxStart(s) => f.debug_tuple("RespXxStart").field(s).finish(),
            Self::RespXxAwaitingFinal(s) => f.debug_tuple("RespXxAwaitingFinal").field(s).finish(),
            Self::EncReady(s) => f.debug_tuple("EncReady").field(s).finish(),
            Self::Ready(s) => f.debug_tuple("Ready").field(s).finish(),
            Self::Invalid => write!(f, "Invalid"),
        }
    }
}

impl State {
    /// Get the remote peer's static public key if available.
    fn get_remote_static(&self) -> Option<[u8; PUBLIC_KEYLEN]> {
        delegate_to_state!(self, get_remote_static, None)
    }
    /// Get the local public key.
    fn get_local_public_key(&self) -> Option<[u8; PUBLIC_KEYLEN]> {
        Some(delegate_to_state!(self, get_local_public_key, return None))
    }
    /// Get the local public key.
    fn is_initiator(&self) -> Option<bool> {
        Some(delegate_to_state!(self, is_initiator, return None))
    }

    /// Get the handshake hash if available (only in Ready state).
    fn handshake_hash(&self) -> Option<&[u8]> {
        match self {
            Self::Ready(s) => Some(s.handshake_hash()),
            Self::EncReady(s) => Some(s.handshake_hash()),
            _ => None,
        }
    }
}

/// A ["Sans-IO"](https://fasterthanli.me/articles/the-case-for-sans-io) implementation of all the
/// logic of the [`Cipher`]
pub struct SansIoCipher {
    state: State,
    encrypted_tx: VecDeque<Vec<u8>>,
    encrypted_rx: VecDeque<Result<Vec<u8>, std::io::Error>>,
    plain_tx: VecDeque<Vec<u8>>,
    plain_rx: VecDeque<Event>,
    local_public_key: [u8; 32],
    is_initiator: bool,
}

impl SansIoCipher {
    fn new(state: State) -> Self {
        let is_initiator = state
            .is_initiator()
            .expect("Creating Cipher with invalid state");
        let local_public_key = state
            .get_local_public_key()
            .expect("Creating Cipher with invalid state");
        Self {
            state,
            encrypted_tx: Default::default(),
            encrypted_rx: Default::default(),
            plain_tx: Default::default(),
            plain_rx: Default::default(),
            local_public_key,
            is_initiator,
        }
    }

    #[instrument(skip_all, err)]
    fn handshake_start(&mut self, payload: &[u8]) -> Result<(), std::io::Error> {
        match replace(&mut self.state, State::Invalid) {
            State::InitiatorIkStart(s) => {
                let (s2, out) = s.write_msg(Some(payload))?;
                self.encrypted_tx.push_back(out);
                self.state = State::InitiatorIkSent(s2);
                Ok(())
            }
            _e => todo!("{_e:?}"),
        }
    }

    #[instrument(skip_all, err)]
    /// Encrypt outgoing messages, and decrypt encomming messages.
    /// This also processes messages to complete the handshake.
    fn poll_encrypt_decrypt(&mut self) -> Result<Option<()>, std::io::Error> {
        trace!(
            state =? self.state,
            plain_tx = self.plain_tx.len(),
            plain_rx = self.plain_rx.len(),
            enc_tx = self.encrypted_tx.len(),
            enc_rx = self.encrypted_rx.len(),
            "poll_encrypt_decrypt"
        );

        match replace(&mut self.state, State::Invalid) {
            // IK Initiator: HsMsgSent -> HsDone -> EncReady
            State::InitiatorIkSent(s) => {
                let Some(msg) = self.encrypted_rx.pop_front() else {
                    self.state = State::InitiatorIkSent(s);
                    return Ok(None);
                };
                let (s2, payload) = s.read_msg(&msg?)?;
                // Ensure payload jumps to the front of the line
                self.plain_rx.push_front(Event::HandshakePayload(payload));

                // Send the setup message
                let (s3, setup_msg) = s2.write_msg()?;
                self.encrypted_tx.push_front(setup_msg);
                self.state = State::EncReady(s3);
                Ok(Some(()))
            }
            // IK Responder: Start -> HsDone -> EncReady
            State::RespIkStart(s) => {
                let Some(msg) = self.encrypted_rx.pop_front() else {
                    // Not ready
                    self.state = State::RespIkStart(s);
                    return Ok(None);
                };
                let (s2, payload) = s.read_msg(&msg?)?;
                // Ensure payload jumps to the front of the line
                self.plain_rx.push_front(Event::HandshakePayload(payload));
                let next_tx = self.plain_tx.pop_front();
                let (s3, [msg1, msg2]) = s2.write_msg(next_tx.as_deref())?;
                self.encrypted_tx.push_front(msg2);
                self.encrypted_tx.push_front(msg1);
                self.state = State::EncReady(s3);
                Ok(Some(()))
            }
            // IK Initiator: Start -> HsMsgSent
            State::InitiatorIkStart(s) => {
                // no handshake message.. We use first thing in plain_tx, but maybe it should be an
                // error bc we might want the payload to be handled explicitly
                let payload = self.plain_tx.pop_front();
                let (s2, out) = s.write_msg(payload.as_deref())?;
                self.encrypted_tx.push_back(out);
                self.state = State::InitiatorIkSent(s2);
                Ok(Some(()))
            }

            // XX Initiator: Start -> HsMsgSent
            State::InitiatorXxStart(s) => {
                let payload = self.plain_tx.pop_front();
                let (s2, out) = s.write_msg(payload.as_deref())?;
                self.encrypted_tx.push_back(out);
                self.state = State::InitiatorXxSent(s2);
                Ok(Some(()))
            }
            // XX Responder: Start -> RespXxReceivedFirst -> ResponderXxAwaitingFinal
            State::RespXxStart(s) => {
                let Some(msg) = self.encrypted_rx.pop_front() else {
                    self.state = State::RespXxStart(s);
                    return Ok(None);
                };
                let (s2, payload) = s.read_msg(&msg?)?;
                // Ensure payload jumps to the front of the line
                self.plain_rx.push_front(Event::HandshakePayload(payload));
                let next_tx = self.plain_tx.pop_front();
                let (s3, [msg1, _should_be_empty]) = s2.write_msg(next_tx.as_deref())?;
                debug_assert!(_should_be_empty.is_empty());
                self.encrypted_tx.push_front(msg1);

                self.state = State::RespXxAwaitingFinal(s3);
                Ok(Some(()))
            }

            // XX Initiator: HsMsgSent -> InitiatorXxFinalMsg -> InitiatorXxHsDone
            State::InitiatorXxSent(s) => {
                let Some(msg) = self.encrypted_rx.pop_front() else {
                    self.state = State::InitiatorXxSent(s);
                    return Ok(None);
                };
                let (s2, payload) = s.read_msg(&msg?)?;
                if !payload.is_empty() {
                    self.plain_rx.push_front(Event::HandshakePayload(payload));
                }
                let (s3, msg1) = s2.write_msg()?;
                let (s4, msg2) = s3.write_msg()?;
                self.encrypted_tx.push_front(msg2);
                self.encrypted_tx.push_front(msg1);

                self.state = State::EncReady(s4);
                Ok(Some(()))
            }
            // XX Responder: ResponderXxAwaitingFinal -> HsDone
            State::RespXxAwaitingFinal(s) => {
                let Some(msg) = self.encrypted_rx.pop_front() else {
                    self.state = State::RespXxAwaitingFinal(s);
                    return Ok(None);
                };
                let (s2, payload) = s.read_msg(&msg?)?;
                // Third message typically has no payload, but we handle it anyway
                if !payload.is_empty() {
                    self.plain_rx.push_front(Event::HandshakePayload(payload));
                }
                //let next_tx = self.plain_tx.pop_front();
                let (s3, msg1) = s2.write_msg()?;
                self.encrypted_tx.push_front(msg1);
                self.state = State::EncReady(s3);
                Ok(Some(()))
            }

            State::EncReady(mut s) => {
                let mut made_progress = false;
                while let Some(mut msg) = self.plain_tx.pop_front() {
                    s.push(&mut msg, &[], Tag::Message)?;
                    self.encrypted_tx.push_back(msg);
                    made_progress = true;
                }
                let Some(msg) = self.encrypted_rx.pop_front() else {
                    self.state = State::EncReady(s);
                    return Ok(made_progress.then_some(()));
                };
                self.state = State::Ready(s.read_msg(&msg?)?);
                Ok(Some(()))
            }
            State::Ready(mut s) => {
                let mut made_progress = false;

                if let Some(encrypted_result) = self.encrypted_rx.pop_front() {
                    match encrypted_result {
                        Ok(mut encrypted_msg) => {
                            let _tag = s.pull(&mut encrypted_msg, &[])?;

                            self.plain_rx.push_back(Event::Message(encrypted_msg));
                            made_progress = true;
                        }
                        Err(_e) => todo!("How should we handle an error in receiving a message?"),
                    }
                }

                // encrypt outgoing messages
                if let Some(mut plain_msg) = self.plain_tx.pop_front() {
                    s.push(&mut plain_msg, &[], Tag::Message)?;
                    self.encrypted_tx.push_back(plain_msg);
                    made_progress = true;
                }

                self.state = State::Ready(s);
                Ok(if made_progress { Some(()) } else { None })
            }
            State::Invalid => Err(IoError::other("Invalid state")),
        }
    }

    /// Do as much work as possible encrypting plaintext and decrypting ciphertext
    fn poll_all_enc_dec(&mut self) -> Result<Option<()>, IoError> {
        let mut made_progress = false;
        while self.poll_encrypt_decrypt()?.is_some() {
            made_progress = true;
        }
        Ok(made_progress.then_some(()))
    }

    // NB: vectorized version of 'get_next_sendable_message'. currently just used in tests
    #[cfg(test)]
    fn get_sendable_messages(&mut self) -> Result<Vec<Vec<u8>>, IoError> {
        self.poll_all_enc_dec()?;
        Ok(self.encrypted_tx.drain(..).collect())
    }

    fn get_next_sendable_message(&mut self) -> Result<Option<Vec<u8>>, IoError> {
        self.poll_all_enc_dec()?;
        Ok(self.encrypted_tx.pop_front())
    }

    #[cfg(test)]
    // NB: vectorized version of 'receive_next'. currently just used in tests
    fn receive_next_messages(&mut self, encrypted_messages: Vec<Vec<u8>>) {
        self.encrypted_rx
            .extend(encrypted_messages.into_iter().map(Ok));
    }

    fn receive_next(&mut self, encrypted_msg: Vec<u8>) {
        self.encrypted_rx.push_back(Ok(encrypted_msg));
    }

    fn queue_msg(&mut self, msg: Vec<u8>) {
        self.plain_tx.push_back(msg);
    }

    fn next_decrypted_message(&mut self) -> Result<Option<Event>, IoError> {
        self.poll_all_enc_dec()?;
        Ok(self.plain_rx.pop_front())
    }

    fn ready(&self) -> bool {
        matches!(self.state, State::Ready(_))
    }

    /// Get the remote peer's static public key if available.
    fn get_remote_static(&self) -> Option<[u8; PUBLIC_KEYLEN]> {
        self.state.get_remote_static()
    }

    /// Get the local public key
    fn get_local_public_key(&self) -> [u8; PUBLIC_KEYLEN] {
        self.local_public_key
    }

    /// Get the handshake hash if available (only after handshake completes).
    fn handshake_hash(&self) -> Option<&[u8]> {
        self.state.handshake_hash()
    }
}

impl Debug for SansIoCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SansIoCipher")
            .field("state", &self.state)
            .field("encrypted_tx", &self.encrypted_tx.len())
            .field("encrypted_rx", &self.encrypted_rx.len())
            .field("plain_tx", &self.plain_tx.len())
            .field("plain_rx", &self.plain_rx.len())
            .finish()
    }
}

#[derive(Debug)]
/// Encryption event
pub enum Event {
    /// Data passed through the handshake payload
    HandshakePayload(Vec<u8>),
    /// Decrypted message
    Message(Vec<u8>),
    /// Error occured in encryption
    ErrStuff(IoError),
}

/// Supertrait for duplex channel required by [`Cipher`]
pub trait CipherIo:
    Stream<Item = Result<Vec<u8>, IoError>> + Sink<Vec<u8>> + Send + Sync + Unpin + 'static
{
}

impl<T> CipherIo for T
where
    T: Stream<Item = Result<Vec<u8>, IoError>> + Sink<Vec<u8>> + Send + Sync + Unpin + 'static,
    <T as Sink<Vec<u8>>>::Error: Into<crate::Error> + std::fmt::Debug,
{
}

/// Encrypts and decrypts messages over a Noise IK handshake channel.
///
/// `Cipher` manages the full lifecycle: Noise handshake, key exchange, and
/// subsequent symmetric encryption/decryption of application messages.
///
/// It's is designed to be use with and without IO (see [`CipherIo`]). In practice it created
/// without IO, and used to set up a channel which then becomes the IO.
///
/// # Usage modes
///
/// **With IO** — Provide a [`CipherIo`] transport (a bidirectional `Stream`/`Sink`) and use
/// `Cipher` as a `Stream<Item = Event>` / `Sink<Vec<u8>>`. When messages read/write through the
/// stream/sink interface, before the handshake is ready, they'll be sent as handshake payloads.
///
/// **Without IO** — Create with `io: None` and drive the protocol manually:
/// 1. Feed incoming ciphertext with [`receive_next`](Self::receive_next).
/// 2. Pull outgoing ciphertext with [`get_next_sendable_message`](Self::get_next_sendable_message).
/// 3. Read decrypted events with [`next_decrypted_message`](Self::next_decrypted_message).
/// 4. Queue plaintext for encryption with [`queue_msg`](Self::queue_msg).
///
/// IO can also be attached later via [`set_io`](Self::set_io).
pub struct Cipher {
    io: Option<Box<dyn CipherIo<Error = std::io::Error>>>,
    inner: SansIoCipher,
}

impl Debug for Cipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cipher")
            .field("io", &"")
            .field("inner", &self.inner)
            .finish()
    }
}

impl Cipher {
    /// Create a new [`Cipher`]
    pub fn new(io: Option<Box<dyn CipherIo<Error = std::io::Error>>>, inner: SansIoCipher) -> Self {
        Self { io, inner }
    }

    fn is_initiator(&self) -> bool {
        self.inner.is_initiator
    }

    /// Create a new initiator with the specified Noise pattern
    pub fn new_dht_init_with_pattern(
        io: Option<Box<dyn CipherIo<Error = std::io::Error>>>,
        pattern: HandshakePattern,
        remote_pub_key: Option<&[u8; PUBLIC_KEYLEN]>,
        prologue: &[u8],
    ) -> Result<Self, Error> {
        let inner = match pattern {
            HandshakePattern::IK => {
                let remote_key = remote_pub_key.ok_or(Error::MissingRemoteKey)?;
                let ss = SecStream::new_initiator_ik(remote_key, prologue)?;
                SansIoCipher::new(State::InitiatorIkStart(ss))
            }
            HandshakePattern::XX => {
                if remote_pub_key.is_some() {
                    return Err(Error::UnexpectedRemoteKey);
                }
                let ss = SecStream::new_initiator_xx(prologue)?;
                SansIoCipher::new(State::InitiatorXxStart(ss))
            }
        };
        Ok(Self::new(io, inner))
    }

    /// Create a new initiator (backward compatible, uses IK pattern)
    pub fn new_dht_init(
        io: Option<Box<dyn CipherIo<Error = std::io::Error>>>,
        remote_pub_key: &[u8; PUBLIC_KEYLEN],
        prologue: &[u8],
    ) -> Result<Self, Error> {
        Self::new_dht_init_with_pattern(io, HandshakePattern::IK, Some(remote_pub_key), prologue)
    }

    /// Create a new initiator (IK pattern)
    pub fn new_init(
        io: Box<dyn CipherIo<Error = std::io::Error>>,
        state: SecStream<Initiator<IK, Start>>,
    ) -> Self {
        Self::new(Some(io), state.into())
    }

    /// Create a new responder from a private key with the specified pattern
    pub fn resp_from_private_with_pattern(
        io: Option<Box<dyn CipherIo<Error = std::io::Error>>>,
        keypair: &snow::Keypair,
        pattern: HandshakePattern,
        prologue: &[u8],
    ) -> Result<Self, Error> {
        let inner = match pattern {
            HandshakePattern::IK => {
                let ss = SecStream::new_responder_ik(keypair, prologue)?;
                SansIoCipher::new(State::RespIkStart(ss))
            }
            HandshakePattern::XX => {
                let ss = SecStream::new_responder_xx(keypair, prologue)?;
                SansIoCipher::new(State::RespXxStart(ss))
            }
        };
        Ok(Self::new(io, inner))
    }

    /// Create a new responder from a private key (backward compatible, uses IK pattern)
    pub fn resp_from_private(
        io: Option<Box<dyn CipherIo<Error = std::io::Error>>>,
        keypair: &snow::Keypair,
    ) -> Result<Self, Error> {
        Self::resp_from_private_with_pattern(io, keypair, HandshakePattern::default(), &[])
    }

    /// Create a new responder from a private key with a prologue (backward compatible, uses IK pattern)
    pub fn resp_from_private_with_prologue(
        io: Option<Box<dyn CipherIo<Error = std::io::Error>>>,
        keypair: &snow::Keypair,
        prologue: &[u8],
    ) -> Result<Self, Error> {
        Self::resp_from_private_with_pattern(io, keypair, HandshakePattern::default(), prologue)
    }

    /// Create a new responder (IK pattern)
    pub fn new_resp(
        io: Box<dyn CipherIo<Error = std::io::Error>>,
        state: SecStream<Responder<IK, Start>>,
    ) -> Self {
        Self::new(Some(io), state.into())
    }

    /// Wait for handshake to complete
    #[cfg(test)]
    pub async fn complete_handshake(&mut self) -> Result<(), IoError> {
        use futures::{SinkExt, StreamExt};

        loop {
            if !self.inner.ready() {
                use std::time::Duration;

                self.poll_encrypt_decrypt()?;
                _ = tokio::time::timeout(Duration::from_millis(100), self.flush()).await;
                if self.inner.ready() {
                    return Ok(());
                }
                let x = tokio::time::timeout(Duration::from_millis(100), self.next()).await;
                if self.inner.ready() {
                    if let Ok(Some(event)) = x {
                        self.inner.plain_rx.push_front(event);
                    }
                    return Ok(());
                }
            } else {
                return Ok(());
            }
        }
    }

    #[instrument(skip_all, err)]
    /// Start the handshake
    pub fn handshake_start(&mut self, payload: &[u8]) -> Result<(), IoError> {
        self.inner.handshake_start(payload)
    }

    /// Try to get the next encrypted message to send.
    pub fn get_next_sendable_message(&mut self) -> Result<Option<Vec<u8>>, IoError> {
        self.inner.get_next_sendable_message()
    }

    /// Manually add a received encrypted message to be decrypted.
    pub fn receive_next(&mut self, encrypted_msg: Vec<u8>) {
        self.inner.receive_next(encrypted_msg)
    }

    /// Try to get the next decrypted message.
    pub fn next_decrypted_message(&mut self) -> Result<Option<Event>, IoError> {
        self.inner.next_decrypted_message()
    }

    /// Queue a plaintext message into encrypted and sent
    pub fn queue_msg(&mut self, payload: Vec<u8>) {
        self.inner.queue_msg(payload);
    }

    fn get_io(&mut self) -> Result<&mut Box<dyn CipherIo<Error = std::io::Error>>, IoError> {
        if let Some(io) = self.io.as_mut() {
            return Ok(io);
        }
        Err(IoError::other(Error::NoIoSetError))
    }
    /// Set the IO connection for sending and receiving encrypted messages.
    pub fn set_io(&mut self, io: Box<dyn CipherIo<Error = std::io::Error>>) {
        self.io = Some(io);
    }

    #[instrument(skip_all, err)]
    /// Encrypt outgoing messages, and decrypt encomming messages.
    /// This also processes messages to complete the handshake.
    fn poll_encrypt_decrypt(&mut self) -> Result<Option<()>, IoError> {
        self.inner.poll_encrypt_decrypt()
    }

    /// pull in new incomming encrypted messages.
    #[instrument(skip_all)]
    fn poll_incoming_encrypted(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        while let Poll::Ready(Some(result)) =
            Pin::new(&mut self.get_io().expect("Missing IO")).poll_next(cx)
        {
            match result {
                Ok(_) => {
                    self.inner.encrypted_rx.push_back(result);
                }
                Err(e) => match e.kind() {
                    std::io::ErrorKind::UnexpectedEof => {
                        // this happens when nothing can be read from udx? I think?
                        return Poll::Pending;
                    }
                    std::io::ErrorKind::ConnectionReset => {
                        // idk???
                        return Poll::Pending;
                    }
                    // VENDORED FIX (listam leaf): upstream panics via todo!()
                    // on any other error kind. On ESP-IDF the async-io TCP
                    // socket transiently surfaces NotConnected (ENOTCONN) and
                    // friends mid-session — desktop/udx never produce these.
                    // Treat them exactly like ConnectionReset above: a
                    // transient "would-block", returning Pending so in-flight
                    // encrypted messages already queued this poll are NOT
                    // dropped and the session survives (the periodic resync
                    // timer re-polls). Returning Ready(()) here instead drops
                    // the read and stalls multi-block downloads; aborting via
                    // todo!() kills the whole always-on leaf thread.
                    _other => {
                        return Poll::Pending;
                    }
                },
            }
        }
        Poll::Ready(())
    }

    #[instrument(skip_all)]
    fn poll_outgoing_encrypted(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), IoError>> {
        while let Some(msg) = self.inner.encrypted_tx.pop_front() {
            match Pin::new(&mut self.get_io().unwrap()).poll_ready(cx) {
                Poll::Ready(Ok(())) => {
                    if let Err(_e) =
                        Pin::new(&mut self.get_io().expect("Missing IO")).start_send(msg)
                    {
                        return Poll::Ready(Err(IoError::other(
                            "Send failed: TODO Error should have fmt::Debug here",
                        )));
                    }
                }
                Poll::Ready(Err(_e)) => {
                    return Poll::Ready(Err(IoError::other(
                        "IO error: TODO Error should have fmt::Debug here",
                    )));
                }
                Poll::Pending => {
                    self.inner.encrypted_tx.push_front(msg);
                    return Poll::Pending;
                }
            }
        }

        match Pin::new(&mut self.get_io().expect("Missing IO")).poll_flush(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(_e)) => Poll::Ready(Err(IoError::other(
                "Flush failed: TODO Error should have fmt::Debug here",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }

    /// `true` when handshake is completed.
    pub fn ready(&self) -> bool {
        self.inner.ready()
    }

    /// Get the remote peer's static public key.
    ///
    /// For Responders this is `None` until processing reading the first handshake message
    /// For Initiators, this is always `Some(_)` because we use the IK which requires the Initator
    /// to know the Responders public key beforehand.
    pub fn get_remote_static(&self) -> Option<[u8; PUBLIC_KEYLEN]> {
        self.inner.get_remote_static()
    }

    /// Get the local public key. It is only unavailable when Cipher handshake fails.
    pub fn get_local_public_key(&self) -> [u8; PUBLIC_KEYLEN] {
        self.inner.get_local_public_key()
    }

    /// Get the handshake hash.
    ///
    /// This is a unique identifier for this encrypted session, the same on both sides.
    /// Used for capability verification in hypercore replication.
    ///
    /// Returns `None` until the handshake is complete.
    pub fn handshake_hash(&self) -> Option<&[u8]> {
        self.inner.handshake_hash()
    }
}

impl Stream for Cipher {
    type Item = Event;

    #[instrument(skip_all)]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // 1. First, try to return any ready plaintext messages
            if let Some(event) = self.inner.plain_rx.pop_front() {
                return Poll::Ready(Some(event));
            }

            // 2. Pull new encrypted data from IO into our queue
            let _ = self.poll_incoming_encrypted(cx);

            // 3. Send any pending encrypted data to IO
            match self.poll_outgoing_encrypted(cx) {
                Poll::Ready(Err(e)) => {
                    return Poll::Ready(Some(Event::ErrStuff(e)));
                }
                Poll::Pending => {
                    // IO is busy, we can't make progress on sending
                    // but we can still try to process incoming messages
                }
                Poll::Ready(Ok(())) => {
                    // Successfully sent outgoing data
                }
            }

            // 4. Process crypto operations (handshake, encrypt/decrypt)
            match self.poll_encrypt_decrypt() {
                Ok(Some(())) => {
                    // Made progress, loop again to check for more work
                    continue;
                }
                Ok(None) => {
                    // No progress made, no more work available
                    break;
                }
                Err(e) => {
                    return Poll::Ready(Some(Event::ErrStuff(e)));
                }
            }
        }

        // No messages ready and no progress can be made
        Poll::Pending
    }
}

impl Sink<Vec<u8>> for Cipher {
    type Error = IoError;

    #[instrument(skip_all)]
    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Process any pending work to make space in queues
        let _ = self.poll_incoming_encrypted(cx);

        match self.poll_outgoing_encrypted(cx) {
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => {
                // IO is busy, but we can still accept messages for queuing
                return Poll::Ready(Ok(()));
            }
            Poll::Ready(Ok(())) => {
                // IO is ready
            }
        }

        // Process crypto operations to make progress
        match self.poll_encrypt_decrypt() {
            Ok(_) => {
                // Always ready to accept more plaintext messages for queuing
                Poll::Ready(Ok(()))
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    #[instrument(skip_all)]
    fn start_send(mut self: Pin<&mut Self>, item: Vec<u8>) -> Result<(), Self::Error> {
        // Queue the plaintext message for encryption
        self.inner.plain_tx.push_back(item);
        Ok(())
    }

    #[instrument(skip_all)]
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let _always_poll_ready_but_why = self.poll_incoming_encrypted(cx);
        loop {
            // Process crypto operations to encrypt any pending plaintext
            match self.poll_encrypt_decrypt() {
                Ok(Some(())) => {
                    // Made progress, continue processing
                    continue;
                }
                Ok(None) => {
                    // No more crypto work to do
                    break;
                }
                Err(e) => return Poll::Ready(Err(e)),
            }
        }

        // Send any pending encrypted data to IO
        match self.poll_outgoing_encrypted(cx) {
            Poll::Ready(Ok(())) => {
                // Check if we have any pending plaintext that hasn't been encrypted yet
                if self.inner.plain_tx.is_empty() {
                    Poll::Ready(Ok(()))
                } else {
                    // Still have pending plaintext, not fully flushed
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    #[instrument(skip_all)]
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // First flush any pending data
        match self.as_mut().poll_flush(cx) {
            Poll::Ready(Ok(())) => {
                // Now close the underlying IO
                Pin::new(&mut self.get_io().expect("Missing IO"))
                    .poll_close(cx)
                    .map_err(|_e| {
                        IoError::other("Close failed TODO Error should have fmt::debug here")
                    })
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl CipherTrait for Cipher {
    fn remote_public_key(&self) -> Option<[u8; PUBLIC_KEYLEN]> {
        self.get_remote_static()
    }

    fn local_public_key(&self) -> [u8; PUBLIC_KEYLEN] {
        self.get_local_public_key()
    }

    fn handshake_hash(&self) -> Option<Vec<u8>> {
        self.handshake_hash().map(|h| h.to_vec())
    }

    fn is_initiator(&self) -> bool {
        self.is_initiator()
    }
}

#[cfg(test)]
mod tests {
    use crate::state_machine::hc_specific;

    use super::*;
    use futures::{SinkExt, StreamExt, channel::mpsc, join};

    // Mock IO that implements Stream + Sink for testing
    #[derive(Debug)]
    struct MockIo<S>
    where
        S: Stream<Item = Result<Vec<u8>, std::io::Error>>,
    {
        receiver: S,
        sender: mpsc::UnboundedSender<Vec<u8>>,
    }

    impl<S: Stream<Item = Result<Vec<u8>, IoError>> + Unpin> Stream for MockIo<S> {
        type Item = Result<Vec<u8>, IoError>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Pin::new(&mut self.receiver).poll_next(cx)
        }
    }

    impl<S: Stream<Item = Result<Vec<u8>, std::io::Error>>> Sink<Vec<u8>> for MockIo<S> {
        type Error = std::io::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: Vec<u8>) -> Result<(), Self::Error> {
            self.sender
                .unbounded_send(item)
                .map_err(|_| IoError::other("Send failed"))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[expect(clippy::type_complexity)]
    fn create_mock_io_pair() -> (
        MockIo<impl Stream<Item = Result<Vec<u8>, std::io::Error>>>,
        mpsc::UnboundedSender<Result<Vec<u8>, std::io::Error>>,
        mpsc::UnboundedReceiver<Vec<u8>>,
    ) {
        let (io_tx, io_rx) = mpsc::unbounded();
        let (out_tx, out_rx) = mpsc::unbounded();

        let mock_io = MockIo {
            receiver: io_rx,
            sender: out_tx,
        };

        (mock_io, io_tx, out_rx)
    }

    #[expect(clippy::type_complexity)]
    fn new_connected_secret_streams() -> (
        snow::Keypair,
        (
            SecStream<Initiator<IK, Start>>,
            SecStream<Responder<IK, Start>>,
        ),
    ) {
        let kp = hc_specific::generate_keypair().unwrap();
        let ssi = SecStream::new_initiator_ik(&kp.public.clone().try_into().unwrap(), &[]).unwrap();
        let ssr = SecStream::new_responder_ik(&kp, &[]).unwrap();
        (kp, (ssi, ssr))
    }

    fn new_connected_streams() -> (
        impl CipherIo<Error = std::io::Error>,
        impl CipherIo<Error = std::io::Error>,
    ) {
        let (left_tx, left_rx) = mpsc::unbounded();
        let res_left_rx = left_rx.map(|msg: Vec<u8>| Ok::<_, std::io::Error>(msg));

        let (right_tx, right_rx) = mpsc::unbounded();
        let res_right_rx = right_rx.map(|msg: Vec<u8>| Ok::<_, std::io::Error>(msg));

        let left = MockIo {
            sender: left_tx,
            receiver: res_right_rx,
        };
        let right = MockIo {
            sender: right_tx,
            receiver: res_left_rx,
        };
        (left, right)
    }

    fn connected_machines() -> (snow::Keypair, (Cipher, Cipher)) {
        let (kp, (lss, rss)) = new_connected_secret_streams();
        let (lio, rio) = new_connected_streams();
        let (lm, rm) = (
            Cipher::new_init(Box::new(lio), lss),
            Cipher::new_resp(Box::new(rio), rss),
        );
        (kp, (lm, rm))
    }

    // XX pattern helper functions
    #[expect(clippy::type_complexity)]
    fn new_connected_secret_streams_xx() -> (
        snow::Keypair,
        (
            SecStream<Initiator<XX, Start>>,
            SecStream<Responder<XX, Start>>,
        ),
    ) {
        let kp = hc_specific::generate_keypair().unwrap();
        let ssi = SecStream::new_initiator_xx(&[]).unwrap();
        let ssr = SecStream::new_responder_xx(&kp, &[]).unwrap();
        (kp, (ssi, ssr))
    }

    fn connected_machines_xx() -> (snow::Keypair, (Cipher, Cipher)) {
        let (kp, (_init_state, _resp_state)) = new_connected_secret_streams_xx();
        let (lio, rio) = new_connected_streams();

        let init_cipher =
            Cipher::new_dht_init_with_pattern(Some(Box::new(lio)), HandshakePattern::XX, None, &[])
                .unwrap();

        let resp_cipher = Cipher::resp_from_private_with_pattern(
            Some(Box::new(rio)),
            &kp,
            HandshakePattern::XX,
            &[],
        )
        .unwrap();

        (kp, (init_cipher, resp_cipher))
    }

    #[test]
    fn sans_io() -> Result<(), Error> {
        let (_, (lss, rss)) = new_connected_secret_streams();
        let (mut l, mut r) = (SansIoCipher::new(lss.into()), SansIoCipher::new(rss.into()));

        let lx = l.get_sendable_messages()?;
        r.receive_next_messages(lx);

        let rx = r.get_sendable_messages()?; // <-- here. r is responder
        l.receive_next_messages(rx);

        let lx = l.get_sendable_messages()?;
        r.receive_next_messages(lx);

        assert!(l.ready());
        let rx = r.get_sendable_messages()?;
        l.receive_next_messages(rx);
        assert!(r.ready());
        Ok(())
    }

    #[tokio::test]
    async fn test_complete_handshake() -> Result<(), Error> {
        let (_, (mut lm, mut rm)) = connected_machines();
        let (rl, rr) = join!(lm.complete_handshake(), rm.complete_handshake());
        rl?;
        rr?;
        assert!(lm.inner.ready());
        assert!(rm.inner.ready());
        Ok(())
    }

    #[tokio::test]
    async fn test_streams() -> Result<(), Error> {
        let (mut l, mut r) = new_connected_streams();
        let (a, b) = join!(l.send(b"yo".to_vec()), r.next());
        assert!(a.is_ok());
        assert_eq!(b.unwrap()?, b"yo".to_vec());

        let (a, b) = join!(r.send(b"yo".to_vec()), l.next());
        assert!(a.is_ok());
        assert_eq!(b.unwrap()?, b"yo".to_vec());
        Ok(())
    }
    #[tokio::test]
    async fn test_machine_io_l_to_r() -> Result<(), Error> {
        let (_, (mut lm, mut rm)) = connected_machines();

        let payload = b"Hello, World!".to_vec();
        lm.handshake_start(&payload)?;
        let (lres, rres) = join!(lm.flush(), rm.next());
        assert!(matches!(rres, Some(Event::HandshakePayload(_))));
        lres?;
        Ok(())
    }

    #[tokio::test]
    async fn test_machine_io_both_ways() -> Result<(), Error> {
        let (_, (mut lm, mut rm)) = connected_machines();

        let res = join!(lm.send(b"ltor".into()), rm.send(b"rtol".into()));
        assert_eq!((res.0?, res.1?), ((), ()));

        let (Some(lr), Some(rr)) = join!(lm.next(), rm.next()) else {
            panic!()
        };

        let (empty, rtol, ltor): (Vec<u8>, _, _) = (vec![], b"rtol".to_vec(), b"ltor".to_vec());
        assert!(matches!(lr, Event::HandshakePayload(x) if x == empty));
        assert!(matches!(rr, Event::HandshakePayload(x) if x == empty));

        let (Some(lr), Some(rr)) = join!(lm.next(), rm.next()) else {
            panic!()
        };
        assert!(matches!(lr, Event::Message(x) if x == rtol));
        assert!(matches!(rr, Event::Message(x) if x == ltor));

        Ok(())
    }
    #[tokio::test]
    async fn test_machine_sink_multiple_messages() -> Result<(), Error> {
        let (_, (mut lm, mut rm)) = connected_machines();

        let (rl, rr) = join!(lm.complete_handshake(), rm.complete_handshake());
        rl?;
        rr?;

        let mut msgs = vec![];
        for i in 0..5 {
            let msg = format!("Message {}", i).into_bytes();
            msgs.push(msg.clone());
            lm.send(msg).await?;
        }

        let mut results = vec![];
        for _ in 0..5 {
            let Event::Message(m) = rm.next().await.unwrap() else {
                panic!();
            };
            results.push(m);
        }
        assert_eq!(results, msgs);

        Ok(())
    }

    #[tokio::test]
    async fn test_machine_stream_returns_pending_when_no_data() -> Result<(), Error> {
        let remote_key = [3u8; 32];
        let initiator_state = SecStream::new_initiator_ik(&remote_key, &[])?;

        let (mock_io, _io_tx, _out_rx) = create_mock_io_pair();
        let mut machine = Cipher::new_init(Box::new(mock_io), initiator_state);

        // Test that stream returns None when no data is available
        let mut stream = Box::pin(&mut machine);

        // Use a timeout to ensure we don't wait forever
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(100), stream.next()).await;

        // Should timeout because no data is available
        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_machine_handshake_start() -> Result<(), Error> {
        let kp = hc_specific::generate_keypair().unwrap();
        let public = kp.public.try_into().unwrap();
        let initiator_state = SecStream::new_initiator_ik(&public, &[])?;

        let (mock_io, _io_tx, mut out_rx) = create_mock_io_pair();
        let mut machine = Cipher::new_init(Box::new(mock_io), initiator_state);

        // Start handshake
        let payload = b"handshake payload";
        machine.handshake_start(payload)?;

        // Should have transitioned to InitiatorSent state
        assert!(matches!(machine.inner.state, State::InitiatorIkSent(_)));

        // Should have queued encrypted handshake message
        assert!(!machine.inner.encrypted_tx.is_empty());

        // Process outgoing to send the handshake message
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let _result = machine.poll_outgoing_encrypted(&mut cx);

        // Should have sent handshake message to IO
        let sent_msg = out_rx.try_recv();
        assert!(sent_msg.is_ok());

        Ok(())
    }

    #[tokio::test]
    async fn test_machine_ready_state_processing() -> Result<(), Error> {
        // This test would require more complex setup to reach Ready state
        // For now, test that we can create a machine in different states

        let remote_key = [5u8; 32];
        let initiator_state = SecStream::new_initiator_ik(&remote_key, &[])?;

        let (mock_io, _io_tx, _out_rx) = create_mock_io_pair();
        let machine = Cipher::new_init(Box::new(mock_io), initiator_state);

        // Verify initial state
        assert!(matches!(machine.inner.state, State::InitiatorIkStart(_)));
        assert!(machine.inner.plain_tx.is_empty());
        assert!(machine.inner.plain_rx.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_machine_poll_ready_always_succeeds() -> Result<(), Error> {
        let remote_key = [6u8; 32];
        let initiator_state = SecStream::new_initiator_ik(&remote_key, &[])?;

        let (mock_io, _io_tx, _out_rx) = create_mock_io_pair();
        let mut machine = Cipher::new_init(Box::new(mock_io), initiator_state);

        // poll_ready should always succeed since we queue internally
        let mut sink = Box::pin(&mut machine);
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let ready_result = sink.as_mut().poll_ready(&mut cx);

        assert!(matches!(ready_result, Poll::Ready(Ok(()))));

        Ok(())
    }

    #[test]
    fn test_get_remote_static_sans_io() -> Result<(), Error> {
        let (kp, (init, resp)) = new_connected_secret_streams();
        let resp_pub: [u8; PUBLIC_KEYLEN] = kp.public.try_into().unwrap();
        let (mut init, mut resp) = (
            SansIoCipher::new(init.into()),
            SansIoCipher::new(resp.into()),
        );

        // Responder doesn't know remote static before handshake
        assert!(resp.get_remote_static().is_none());

        // Initiator knows the responder's public key from construction
        assert_eq!(init.get_remote_static(), Some(resp_pub));

        // Initiator sends first handshake message
        let lx = init.get_sendable_messages()?;
        resp.receive_next_messages(lx);
        // msg is queued, but not processed yet
        assert!(resp.get_remote_static().is_none(),);

        // Responder processes first message and creates response
        let rx = resp.get_sendable_messages()?;
        init.receive_next_messages(rx);

        // After processing, responder should know initiator's public key
        let resp_remote = resp.get_remote_static();
        assert!(resp_remote.is_some());

        // Complete handshake
        let lx = init.get_sendable_messages()?;
        resp.receive_next_messages(lx);
        let rx = resp.get_sendable_messages()?;
        init.receive_next_messages(rx);

        assert!(init.ready());
        assert!(resp.ready());

        // Keys remain available after handshake completion
        assert_eq!(init.get_remote_static(), Some(resp_pub));
        assert_eq!(resp.get_remote_static(), resp_remote);

        Ok(())
    }

    #[tokio::test]
    async fn test_get_remote_static_after_handshake() -> Result<(), Error> {
        let (kp, (mut lm, mut rm)) = connected_machines();
        let resp_pub: [u8; PUBLIC_KEYLEN] = kp.public.try_into().unwrap();

        // Before handshake: initiator has the key, responder does not
        assert_eq!(lm.get_remote_static(), Some(resp_pub));
        assert!(rm.get_remote_static().is_none());

        let (rl, rr) = join!(lm.complete_handshake(), rm.complete_handshake());
        rl?;
        rr?;

        // Initiator should still have responder's public key
        assert_eq!(lm.get_remote_static(), Some(resp_pub));

        // Responder should now have initiator's public key
        assert!(rm.get_remote_static().is_some());

        Ok(())
    }

    #[tokio::test]
    async fn test_handshake_hash_same_on_both_sides() -> Result<(), Error> {
        let (_, (mut lm, mut rm)) = connected_machines();

        // Before handshake: no handshake hash available
        assert!(lm.handshake_hash().is_none());
        assert!(rm.handshake_hash().is_none());

        let (rl, rr) = join!(lm.complete_handshake(), rm.complete_handshake());
        rl?;
        rr?;

        // After handshake: both sides should have the same handshake hash
        let lm_hash = lm.handshake_hash();
        let rm_hash = rm.handshake_hash();

        assert!(lm_hash.is_some(), "initiator should have handshake hash");
        assert!(rm_hash.is_some(), "responder should have handshake hash");
        assert_eq!(
            lm_hash, rm_hash,
            "handshake hash should be identical on both sides"
        );

        // Hash should be 64 bytes (BLAKE2b output)
        assert_eq!(lm_hash.unwrap().len(), 64);

        Ok(())
    }

    // ===== XX Pattern Tests =====

    #[test]
    fn sans_io_xx() -> Result<(), Error> {
        let (_, (init, resp)) = new_connected_secret_streams_xx();
        let (mut init, mut resp) = (
            SansIoCipher::new(init.into()),
            SansIoCipher::new(resp.into()),
        );

        // Round 1: Initiator -> Responder (ephemeral key)
        let init_msg1 = init.get_sendable_messages()?; // Start -> HsMsgSent
        assert_eq!(init_msg1.len(), 1);
        resp.receive_next_messages(init_msg1);

        // Round 2: Responder -> Initiator (ephemeral + static key)
        let resp_msg1 = resp.get_sendable_messages()?;
        assert_eq!(resp_msg1.len(), 1);
        init.receive_next_messages(resp_msg1); // HsMsgSent -> HsMsgSent

        // Round 3: Initiator -> Responder: two messages, static key & third handshake message
        let init_msg2 = init.get_sendable_messages()?; // HsMsgSent -> EncryptorReady
        assert_eq!(init_msg2.len(), 2);
        resp.receive_next_messages(init_msg2);

        // Round 4:
        let resp_msg2 = resp.get_sendable_messages()?;
        assert_eq!(resp_msg2.len(), 1);
        assert!(resp.ready());

        // last message is enqueud but not processed
        init.receive_next_messages(resp_msg2);
        init.poll_encrypt_decrypt()?;
        assert!(init.ready());

        Ok(())
    }

    #[tokio::test]
    async fn test_complete_handshake_xx() -> Result<(), Error> {
        let (_, (mut init, mut resp)) = connected_machines_xx();
        let (init_res, resp_res) = join!(init.complete_handshake(), resp.complete_handshake());
        init_res?;
        resp_res?;
        assert!(init.inner.ready());
        assert!(resp.inner.ready());
        Ok(())
    }

    #[test]
    fn test_get_remote_static_sans_io_xx() -> Result<(), Error> {
        let (kp, (init, resp)) = new_connected_secret_streams_xx();
        let resp_pub = kp.public.try_into().unwrap();
        let (mut init, mut resp) = (
            SansIoCipher::new(init.into()),
            SansIoCipher::new(resp.into()),
        );

        // Round 1: Initiator -> Responder (ephemeral key)
        let init_msg1 = init.get_sendable_messages()?; // Start -> HsMsgSent
        assert_eq!(init_msg1.len(), 1);
        resp.receive_next_messages(init_msg1);

        // Round 2: Responder -> Initiator (ephemeral + static key)
        let resp_msg1 = resp.get_sendable_messages()?;
        assert_eq!(resp_msg1.len(), 1);
        init.receive_next_messages(resp_msg1); // HsMsgSent -> HsMsgSent

        // Round 3: Initiator -> Responder: two messages, static key & third handshake message
        let init_msg2 = init.get_sendable_messages()?; // HsMsgSent -> EncryptorReady
        assert_eq!(init_msg2.len(), 2);
        resp.receive_next_messages(init_msg2);

        // Round 4:
        let resp_msg2 = resp.get_sendable_messages()?;
        assert_eq!(resp_msg2.len(), 1);
        assert!(resp.ready());

        // last message is enqueud but not processed
        init.receive_next_messages(resp_msg2);
        init.poll_encrypt_decrypt()?;
        assert!(init.ready());

        // After handshake: both sides should know each other's keys
        assert_eq!(init.get_remote_static(), Some(resp_pub));
        assert!(resp.get_remote_static().is_some());

        Ok(())
    }

    #[tokio::test]
    async fn test_get_remote_static_after_handshake_xx() -> Result<(), Error> {
        let (kp, (mut init, mut resp)) = connected_machines_xx();
        let resp_pub: [u8; PUBLIC_KEYLEN] = kp.public.try_into().unwrap();

        // Before handshake: neither side has the other's key (XX pattern)
        assert!(init.get_remote_static().is_none());
        assert!(resp.get_remote_static().is_none());

        let (init_res, resp_res) = join!(init.complete_handshake(), resp.complete_handshake());
        init_res?;
        resp_res?;

        // After handshake: both should know each other's keys
        assert_eq!(init.get_remote_static(), Some(resp_pub));
        assert!(resp.get_remote_static().is_some());

        Ok(())
    }

    #[tokio::test]
    async fn test_handshake_hash_same_on_both_sides_xx() -> Result<(), Error> {
        let (_, (mut init, mut resp)) = connected_machines_xx();

        let (init_res, resp_res) = join!(init.complete_handshake(), resp.complete_handshake());
        init_res?;
        resp_res?;

        // After handshake: both sides should have the same handshake hash
        let init_hash = init.handshake_hash();
        let resp_hash = resp.handshake_hash();

        assert!(init_hash.is_some(), "initiator should have handshake hash");
        assert!(resp_hash.is_some(), "responder should have handshake hash");
        assert_eq!(
            init_hash, resp_hash,
            "handshake hash should be identical on both sides"
        );

        // Hash should be 64 bytes (BLAKE2b output)
        assert_eq!(init_hash.unwrap().len(), 64);

        Ok(())
    }

    #[tokio::test]
    async fn test_xx_message_exchange() -> Result<(), Error> {
        let (_, (mut init, mut resp)) = connected_machines_xx();

        // Complete handshake
        let (init_res, resp_res) = join!(init.complete_handshake(), resp.complete_handshake());
        init_res?;
        resp_res?;

        // Test bidirectional message exchange
        let msg1 = b"Hello from initiator".to_vec();
        let msg2 = b"Hello from responder".to_vec();

        init.send(msg1.clone()).await?;
        resp.send(msg2.clone()).await?;

        let recv1 = resp.next().await;
        let recv2 = init.next().await;

        assert!(matches!(recv1, Some(Event::Message(m)) if m == msg1));
        assert!(matches!(recv2, Some(Event::Message(m)) if m == msg2));

        Ok(())
    }
}
