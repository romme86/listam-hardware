/*!
Types shared between `hypercore` and `hypercore-protocol`
*/
#![warn(
    unreachable_pub,
    missing_debug_implementations,
    missing_docs,
    redundant_lifetimes,
    unsafe_code,
    non_local_definitions,
    clippy::needless_pass_by_value,
    clippy::needless_pass_by_ref_mut,
    clippy::enum_glob_use
)]

use blake2::{
    Blake2b, Blake2bMac, Digest,
    digest::{FixedOutput, generic_array::GenericArray, typenum::U32},
};
use byteorder::{BigEndian, WriteBytesExt};
use compact_encoding::{
    CompactEncoding, EncodingError, EncodingErrorKind, FixedWidthEncoding, VecEncodable, as_array,
    decode_usize, encoded_size_usize, map_decode, map_encode, sum_encoded_size, take_array,
    to_encoded_bytes, write_array,
};
use ed25519_dalek::VerifyingKey;
use merkle_tree_stream::{Node as NodeTrait, NodeKind, NodeParts};
use pretty_hash::fmt as pretty_fmt;

use std::{
    cmp::Ordering,
    convert::AsRef,
    fmt::{self, Display},
    mem,
};
// https://en.wikipedia.org/wiki/Merkle_tree#Second_preimage_attack
const LEAF_TYPE: [u8; 1] = [0x00];
const PARENT_TYPE: [u8; 1] = [0x01];
const ROOT_TYPE: [u8; 1] = [0x02];
const HYPERCORE: [u8; 9] = *b"hypercore";

/// These the output of, see `hash_namespace` test below for how they are produced
/// https://github.com/holepunchto/hypercore/blob/cf08b72f14ed7d9ef6d497ebb3071ee0ae20967e/lib/caps.js#L16
pub const TREE: [u8; 32] = [
    0x9F, 0xAC, 0x70, 0xB5, 0xC, 0xA1, 0x4E, 0xFC, 0x4E, 0x91, 0xC8, 0x33, 0xB2, 0x4, 0xE7, 0x5B,
    0x8B, 0x5A, 0xAD, 0x8B, 0x58, 0x81, 0xBF, 0xC0, 0xAD, 0xB5, 0xEF, 0x38, 0xA3, 0x27, 0x5B, 0x9C,
];

/// Namespace mixed into manifest hashes, `caps.MANIFEST` in Javascript.
pub const MANIFEST: [u8; 32] = [
    0xE6, 0x4B, 0x71, 0x08, 0xEA, 0xCC, 0xE4, 0x7C, 0xFC, 0x61, 0xAC, 0x85, 0x05, 0x68, 0xF5,
    0x5F, 0x8B, 0x15, 0xB8, 0x2E, 0xC5, 0xED, 0x78, 0xC4, 0xEC, 0x59, 0x7B, 0x03, 0x6E, 0x2A,
    0x14, 0x98,
];

/// Default signer namespace, `caps.DEFAULT_NAMESPACE` in Javascript.
pub const DEFAULT_NAMESPACE: [u8; 32] = [
    0x41, 0x44, 0xEE, 0xA5, 0x31, 0xE4, 0x83, 0xD5, 0x4E, 0x0C, 0x14, 0xF4, 0xCA, 0x68, 0xE0,
    0x64, 0x4F, 0x35, 0x53, 0x43, 0xFF, 0x6F, 0xCB, 0x0F, 0x00, 0x52, 0x00, 0xE1, 0x2C, 0xD7,
    0x47, 0xCB,
];

pub(crate) type Blake2bResult = GenericArray<u8, U32>;
type Blake2b256 = Blake2b<U32>;

/// `BLAKE2b` hash.
#[derive(Debug, Clone, PartialEq)]
pub struct Hash {
    hash: Blake2bResult,
}

impl Hash {
    /// Hash a `Leaf` node.
    #[expect(dead_code)]
    pub(crate) fn from_leaf(data: &[u8]) -> Self {
        let size = u64_as_be(data.len() as u64);

        let mut hasher = Blake2b256::new();
        hasher.update(LEAF_TYPE);
        hasher.update(size);
        hasher.update(data);

        Self {
            hash: hasher.finalize(),
        }
    }

    /// Hash two `Leaf` nodes hashes together to form a `Parent` hash.
    #[expect(dead_code)]
    pub(crate) fn from_hashes(left: &Node, right: &Node) -> Self {
        let (node1, node2) = if left.index <= right.index {
            (left, right)
        } else {
            (right, left)
        };

        let size = u64_as_be(node1.length + node2.length);

        let mut hasher = Blake2b256::new();
        hasher.update(PARENT_TYPE);
        hasher.update(size);
        hasher.update(node1.hash());
        hasher.update(node2.hash());

        Self {
            hash: hasher.finalize(),
        }
    }

    /// Hash a public key. Useful to find the key you're looking for on a public
    /// network without leaking the key itself.
    #[expect(dead_code)]
    pub(crate) fn for_discovery_key(public_key: VerifyingKey) -> Self {
        let mut hasher =
            Blake2bMac::<U32>::new_with_salt_and_personal(public_key.as_bytes(), &[], &[]).unwrap();
        blake2::digest::Update::update(&mut hasher, &HYPERCORE);
        Self {
            hash: hasher.finalize_fixed(),
        }
    }

    /// Hash a vector of `Root` nodes.
    // Called `crypto.tree()` in the JS implementation.
    #[expect(dead_code)]
    pub(crate) fn from_roots(roots: &[impl AsRef<Node>]) -> Self {
        let mut hasher = Blake2b256::new();
        hasher.update(ROOT_TYPE);

        for node in roots {
            let node = node.as_ref();
            hasher.update(node.hash());
            hasher.update(u64_as_be(node.index()));
            hasher.update(u64_as_be(node.len()));
        }

        Self {
            hash: hasher.finalize(),
        }
    }

    /// Returns a byte slice of this `Hash`'s contents.
    pub fn as_bytes(&self) -> &[u8] {
        self.hash.as_slice()
    }

    // NB: The following methods mirror Javascript naming in
    // https://github.com/mafintosh/hypercore-crypto/blob/master/index.js
    // for v10 that use LE bytes.

    /// Hash data
    pub fn data(data: &[u8]) -> Self {
        let size =
            (|| Ok::<_, EncodingError>(to_encoded_bytes!((data.len() as u64).as_fixed_width())))()
                .expect("Encoding u64 should not fail");

        let mut hasher = Blake2b256::new();
        hasher.update(LEAF_TYPE);
        hasher.update(&size);
        hasher.update(data);

        Self {
            hash: hasher.finalize(),
        }
    }

    /// Hash a parent
    pub fn parent(left: &Node, right: &Node) -> Self {
        let (node1, node2) = if left.index <= right.index {
            (left, right)
        } else {
            (right, left)
        };

        let len = node1.length + node2.length;
        let size: Box<[u8]> =
            (|| Ok::<_, EncodingError>(to_encoded_bytes!(len.as_fixed_width())))()
                .expect("Encoding u64 should not fail");

        let mut hasher = Blake2b256::new();
        hasher.update(PARENT_TYPE);
        hasher.update(&size);
        hasher.update(node1.hash());
        hasher.update(node2.hash());

        Self {
            hash: hasher.finalize(),
        }
    }

    /// Hash a tree
    pub fn tree(roots: &[impl AsRef<Node>]) -> Self {
        let mut hasher = Blake2b256::new();
        hasher.update(ROOT_TYPE);

        for node in roots {
            let node = node.as_ref();
            let buffer = (|| {
                Ok::<_, EncodingError>(to_encoded_bytes!(
                    node.index().as_fixed_width(),
                    node.len().as_fixed_width()
                ))
            })()
            .expect("Encoding u64 should not fail");

            hasher.update(node.hash());
            hasher.update(&buffer[..8]);
            hasher.update(&buffer[8..]);
        }

        Self {
            hash: hasher.finalize(),
        }
    }
}

fn u64_as_be(n: u64) -> [u8; 8] {
    let mut size = [0u8; mem::size_of::<u64>()];
    size.as_mut().write_u64::<BigEndian>(n).unwrap();
    size
}

impl std::ops::Deref for Hash {
    type Target = Blake2bResult;

    fn deref(&self) -> &Self::Target {
        &self.hash
    }
}

impl std::ops::DerefMut for Hash {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.hash
    }
}

/// Nodes of the Merkle Tree that are persisted to disk.
// TODO: replace `hash: Vec<u8>` with `hash: Hash`. This requires patching /
// rewriting the Blake2b crate to support `.from_bytes()` to serialize from
// disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// This node's index in the Merkle tree
    pub index: u64,
    /// Hash of the data in this node
    // TODO make this [u8; 32] like:
    // https://github.com/holepunchto/hypercore/blob/d21ebdeca1b27eb4c2232f8af17d5ae939ee97f2/lib/messages.js#L246
    pub hash: Vec<u8>,
    /// Number of bytes in this [`Node::data`]
    pub length: u64,
    /// Index of this nodes parent
    pub(crate) parent: u64,
    /// Hypercore's data. Can be receieved after the rest of the node, so it's optional.
    pub(crate) data: Option<Vec<u8>>,
    /// If blank
    pub blank: bool,
}

impl Node {
    /// Create a new instance.
    // TODO: ensure sizes are correct.
    pub fn new(index: u64, hash: Vec<u8>, length: u64) -> Self {
        let mut blank = true;
        for byte in &hash {
            if *byte != 0 {
                blank = false;
                break;
            }
        }
        Self {
            index,
            hash,
            length,
            parent: flat_tree::parent(index),
            data: Some(Vec::with_capacity(0)),
            blank,
        }
    }

    /// Creates a new blank node
    pub fn new_blank(index: u64) -> Self {
        Self {
            index,
            hash: vec![0, 32],
            length: 0,
            parent: 0,
            data: None,
            blank: true,
        }
    }
}

impl NodeTrait for Node {
    #[inline]
    fn index(&self) -> u64 {
        self.index
    }

    #[inline]
    fn hash(&self) -> &[u8] {
        &self.hash
    }

    #[inline]
    fn len(&self) -> u64 {
        self.length
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.length == 0
    }

    #[inline]
    fn parent(&self) -> u64 {
        self.parent
    }
}

impl AsRef<Node> for Node {
    #[inline]
    fn as_ref(&self) -> &Self {
        self
    }
}

impl Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Node {{ index: {}, hash: {}, length: {} }}",
            self.index,
            pretty_fmt(&self.hash).unwrap(),
            self.length
        )
    }
}

impl PartialOrd for Node {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Node {
    fn cmp(&self, other: &Self) -> Ordering {
        self.index.cmp(&other.index)
    }
}

impl From<NodeParts<Hash>> for Node {
    fn from(parts: NodeParts<Hash>) -> Self {
        let partial = parts.node();
        let data = match partial.data() {
            NodeKind::Leaf(data) => Some(data.clone()),
            NodeKind::Parent => None,
        };
        let hash: Vec<u8> = parts.hash().as_bytes().into();
        let mut blank = true;
        for byte in &hash {
            if *byte != 0 {
                blank = false;
                break;
            }
        }

        Node {
            index: partial.index(),
            parent: partial.parent,
            length: partial.len(),
            hash,
            data,
            blank,
        }
    }
}

// ----------------------------------------------------------------------------------
//  The types from hypercore
// ----------------------------------------------------------------------------------
#[derive(Debug, Clone, PartialEq)]
/// Request of a DataBlock or DataHash from peer
pub struct RequestBlock {
    /// Hypercore index
    pub index: u64,
    /// TODO: document
    pub nodes: u64,
}

#[derive(Debug, Clone, PartialEq)]
/// Request for a DataUpgrade from peer
pub struct RequestUpgrade {
    /// Hypercore start index
    pub start: u64,
    /// Length of elements
    pub length: u64,
}

#[derive(Debug, Clone, PartialEq)]
/// Proof generated from corresponding requests
pub struct Proof {
    /// Fork
    pub fork: u64,
    /// Data block.
    pub block: Option<DataBlock>,
    /// Data hash
    pub hash: Option<DataHash>,
    /// Data seek
    pub seek: Option<DataSeek>,
    /// Data updrade
    pub upgrade: Option<DataUpgrade>,
}

#[derive(Debug, Clone, PartialEq)]
/// Request of a DataSeek from peer
pub struct RequestSeek {
    /// TODO: document
    pub bytes: u64,
    /// Seek padding, added to the wire format in hypercore 11.
    pub padding: u64,
}

#[derive(Debug, Clone, PartialEq)]
/// TODO: Document
pub struct DataUpgrade {
    /// Starting block of this upgrade response
    pub start: u64,
    /// Number of blocks in this upgrade response
    pub length: u64,
    /// The nodes of the merkle tree
    pub nodes: Vec<Node>,
    /// TODO: Document
    pub additional_nodes: Vec<Node>,
    /// TODO: Document
    pub signature: Vec<u8>,
}
#[derive(Debug, Clone, PartialEq)]
/// Block of data to peer
pub struct DataBlock {
    /// Hypercore index
    pub index: u64,
    /// Data block value in bytes
    pub value: Vec<u8>,
    /// Nodes of the merkle tree
    pub nodes: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq)]
/// Data hash to peer
pub struct DataHash {
    /// Hypercore index
    pub index: u64,
    /// TODO: document
    pub nodes: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq)]
/// TODO: Document
pub struct DataSeek {
    /// TODO: Document
    pub bytes: u64,
    /// TODO: Document
    pub nodes: Vec<Node>,
}

impl CompactEncoding for Node {
    fn encoded_size(&self) -> Result<usize, EncodingError> {
        Ok(sum_encoded_size!(self.index, self.length) + 32)
    }

    fn encode<'a>(&self, buffer: &'a mut [u8]) -> Result<&'a mut [u8], EncodingError> {
        let hash = as_array::<32>(&self.hash)?;
        Ok(map_encode!(buffer, self.index, self.length, hash))
    }

    fn decode(buffer: &[u8]) -> Result<(Self, &[u8]), EncodingError>
    where
        Self: Sized,
    {
        let ((index, length, hash), rest) = map_decode!(buffer, [u64, u64, [u8; 32]]);
        Ok((Node::new(index, hash.to_vec(), length), rest))
    }
}

impl VecEncodable for Node {
    fn vec_encoded_size(vec: &[Self]) -> Result<usize, EncodingError>
    where
        Self: Sized,
    {
        let mut out = encoded_size_usize(vec.len());
        for x in vec {
            out += x.encoded_size()?;
        }
        Ok(out)
    }
}

impl CompactEncoding for RequestBlock {
    fn encoded_size(&self) -> Result<usize, EncodingError> {
        Ok(sum_encoded_size!(self.index, self.nodes))
    }

    fn encode<'a>(&self, buffer: &'a mut [u8]) -> Result<&'a mut [u8], EncodingError> {
        Ok(map_encode!(buffer, self.index, self.nodes))
    }

    fn decode(buffer: &[u8]) -> Result<(Self, &[u8]), EncodingError>
    where
        Self: Sized,
    {
        let ((index, nodes), rest) = map_decode!(buffer, [u64, u64]);
        Ok((RequestBlock { index, nodes }, rest))
    }
}

impl CompactEncoding for RequestSeek {
    fn encoded_size(&self) -> Result<usize, EncodingError> {
        Ok(sum_encoded_size!(self.bytes, self.padding))
    }

    fn encode<'a>(&self, buffer: &'a mut [u8]) -> Result<&'a mut [u8], EncodingError> {
        Ok(map_encode!(buffer, self.bytes, self.padding))
    }

    fn decode(buffer: &[u8]) -> Result<(Self, &[u8]), EncodingError>
    where
        Self: Sized,
    {
        let ((bytes, padding), rest) = map_decode!(buffer, [u64, u64]);
        Ok((RequestSeek { bytes, padding }, rest))
    }
}

impl CompactEncoding for RequestUpgrade {
    fn encoded_size(&self) -> Result<usize, EncodingError> {
        Ok(sum_encoded_size!(self.start, self.length))
    }

    fn encode<'a>(&self, buffer: &'a mut [u8]) -> Result<&'a mut [u8], EncodingError> {
        Ok(map_encode!(buffer, self.start, self.length))
    }

    fn decode(buffer: &[u8]) -> Result<(Self, &[u8]), EncodingError>
    where
        Self: Sized,
    {
        let ((start, length), rest) = map_decode!(buffer, [u64, u64]);
        Ok((RequestUpgrade { start, length }, rest))
    }
}

impl CompactEncoding for DataBlock {
    fn encoded_size(&self) -> Result<usize, EncodingError> {
        Ok(sum_encoded_size!(self.index, self.value, self.nodes))
    }

    fn encode<'a>(&self, buffer: &'a mut [u8]) -> Result<&'a mut [u8], EncodingError> {
        Ok(map_encode!(buffer, self.index, self.value, self.nodes))
    }

    fn decode(buffer: &[u8]) -> Result<(Self, &[u8]), EncodingError>
    where
        Self: Sized,
    {
        let ((index, value, nodes), rest) = map_decode!(buffer, [u64, Vec<u8>, Vec<Node>]);
        Ok((
            DataBlock {
                index,
                value,
                nodes,
            },
            rest,
        ))
    }
}

impl CompactEncoding for DataHash {
    fn encoded_size(&self) -> Result<usize, EncodingError> {
        Ok(sum_encoded_size!(self.index, self.nodes))
    }

    fn encode<'a>(&self, buffer: &'a mut [u8]) -> Result<&'a mut [u8], EncodingError> {
        Ok(map_encode!(buffer, self.index, self.nodes))
    }

    fn decode(buffer: &[u8]) -> Result<(Self, &[u8]), EncodingError>
    where
        Self: Sized,
    {
        let ((index, nodes), rest) = map_decode!(buffer, [u64, Vec<Node>]);
        Ok((DataHash { index, nodes }, rest))
    }
}

impl CompactEncoding for DataSeek {
    fn encoded_size(&self) -> Result<usize, EncodingError> {
        Ok(sum_encoded_size!(self.bytes, self.nodes))
    }

    fn encode<'a>(&self, buffer: &'a mut [u8]) -> Result<&'a mut [u8], EncodingError> {
        Ok(map_encode!(buffer, self.bytes, self.nodes))
    }

    fn decode(buffer: &[u8]) -> Result<(Self, &[u8]), EncodingError>
    where
        Self: Sized,
    {
        let ((bytes, nodes), rest) = map_decode!(buffer, [u64, Vec<Node>]);
        Ok((DataSeek { bytes, nodes }, rest))
    }
}

// from:
// https://github.com/holepunchto/hypercore/blob/d21ebdeca1b27eb4c2232f8af17d5ae939ee97f2/lib/messages.js#L394
impl CompactEncoding for DataUpgrade {
    fn encoded_size(&self) -> Result<usize, EncodingError> {
        Ok(sum_encoded_size!(
            self.start,
            self.length,
            self.nodes,
            self.additional_nodes,
            self.signature
        ))
    }

    fn encode<'a>(&self, buffer: &'a mut [u8]) -> Result<&'a mut [u8], EncodingError> {
        Ok(map_encode!(
            buffer,
            self.start,
            self.length,
            self.nodes,
            self.additional_nodes,
            self.signature
        ))
    }

    fn decode(buffer: &[u8]) -> Result<(Self, &[u8]), EncodingError>
    where
        Self: Sized,
    {
        let ((start, length, nodes, additional_nodes, signature), rest) =
            map_decode!(buffer, [u64, u64, Vec<Node>, Vec<Node>, Vec<u8>]);
        Ok((
            DataUpgrade {
                start,
                length,
                nodes,
                additional_nodes,
                signature,
            },
            rest,
        ))
    }
}

// ----------------------------------------------------------------------------------
//  v11 manifests and multisig signatures
//  Ported from hypercore (JS) lib/messages.js, lib/verifier.js, lib/caps.js
// ----------------------------------------------------------------------------------

const MANIFEST_PATCH: u8 = 0b0000_0001;
const MANIFEST_PROLOGUE: u8 = 0b0000_0010;
const MANIFEST_LINKED: u8 = 0b0000_0100;
const MANIFEST_USER_DATA: u8 = 0b0000_1000;

/// A signer declared in a [`Manifest`]. Only ed25519 signatures and blake2b
/// hashing exist upstream, so the algorithm tags are implicit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestSigner {
    /// Namespace mixed into this signer's signable (v0) or recorded for context (v1+).
    pub namespace: [u8; 32],
    /// Ed25519 public key of the signer.
    pub public_key: [u8; 32],
}

/// Prologue of a v1+ manifest: a pre-agreed tree head that needs no signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestPrologue {
    /// Tree hash of the prologue.
    pub hash: [u8; 32],
    /// Length of the prologue.
    pub length: u64,
}

/// Hypercore v11 manifest: declares how a core's tree signatures are verified.
/// For version >= 1 the core's key is [`manifest_hash`] of the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Manifest version (0 = compat, 1 = standard, 2 = adds linked/userData).
    pub version: u64,
    /// Whether signature patches are allowed (multisig only).
    pub allow_patch: bool,
    /// How many signers must have signed.
    pub quorum: u64,
    /// Declared signers.
    pub signers: Vec<ManifestSigner>,
    /// Optional prologue.
    pub prologue: Option<ManifestPrologue>,
    /// Optional linked core keys (version >= 2).
    pub linked: Option<Vec<[u8; 32]>>,
    /// Optional user data (version >= 2).
    pub user_data: Option<Vec<u8>>,
}

impl Manifest {
    /// The manifest corestore creates by default for a single writer:
    /// version 1, quorum 1, one ed25519 signer under the default namespace.
    pub fn default_signer(public_key: [u8; 32]) -> Self {
        Manifest {
            version: 1,
            allow_patch: false,
            quorum: 1,
            signers: vec![ManifestSigner {
                namespace: DEFAULT_NAMESPACE,
                public_key,
            }],
            prologue: None,
            linked: None,
            user_data: None,
        }
    }
}

/// `0` tag shared by the `hashes` ("blake2b") and `signatures` ("ed25519") enums.
fn known_algorithm_tag(buffer: &[u8], what: &str) -> Result<(), EncodingError> {
    let (tag, _) = u64::decode(buffer)?;
    if tag != 0 {
        return Err(EncodingError::new(
            EncodingErrorKind::InvalidData,
            &format!("Unknown {what} id: {tag}"),
        ));
    }
    Ok(())
}

impl CompactEncoding for ManifestSigner {
    fn encoded_size(&self) -> Result<usize, EncodingError> {
        Ok(1 + 32 + 32) // signature enum + namespace + public key
    }

    fn encode<'a>(&self, buffer: &'a mut [u8]) -> Result<&'a mut [u8], EncodingError> {
        let rest = 0u64.encode(buffer)?; // 'ed25519'
        let rest = write_array(&self.namespace, rest)?;
        write_array(&self.public_key, rest)
    }

    fn decode(buffer: &[u8]) -> Result<(Self, &[u8]), EncodingError>
    where
        Self: Sized,
    {
        known_algorithm_tag(buffer, "signature")?;
        let (_, rest) = u64::decode(buffer)?;
        let (namespace, rest) = take_array::<32>(rest)?;
        let (public_key, rest) = take_array::<32>(rest)?;
        Ok((
            ManifestSigner {
                namespace,
                public_key,
            },
            rest,
        ))
    }
}

impl VecEncodable for ManifestSigner {
    fn vec_encoded_size(vec: &[Self]) -> Result<usize, EncodingError>
    where
        Self: Sized,
    {
        let mut out = encoded_size_usize(vec.len());
        for x in vec {
            out += x.encoded_size()?;
        }
        Ok(out)
    }
}

impl CompactEncoding for ManifestPrologue {
    fn encoded_size(&self) -> Result<usize, EncodingError> {
        Ok(32 + self.length.encoded_size()?)
    }

    fn encode<'a>(&self, buffer: &'a mut [u8]) -> Result<&'a mut [u8], EncodingError> {
        let rest = write_array(&self.hash, buffer)?;
        self.length.encode(rest)
    }

    fn decode(buffer: &[u8]) -> Result<(Self, &[u8]), EncodingError>
    where
        Self: Sized,
    {
        let (hash, rest) = take_array::<32>(buffer)?;
        let (length, rest) = u64::decode(rest)?;
        Ok((ManifestPrologue { hash, length }, rest))
    }
}

impl CompactEncoding for Manifest {
    fn encoded_size(&self) -> Result<usize, EncodingError> {
        if self.version == 0 {
            return Ok(1 // version
                + 1 // hash enum
                + 1 // type
                + match (&self.prologue, self.signers.len()) {
                    (Some(_), 0) => 32,
                    (None, 1) if self.quorum == 1 && !self.allow_patch => {
                        self.signers[0].encoded_size()?
                    }
                    _ => {
                        1 // flags
                        + self.quorum.encoded_size()?
                        + ManifestSigner::vec_encoded_size(&self.signers)?
                    }
                });
        }
        let mut out = 1 // version
            + 1 // flags
            + 1 // hash enum
            + self.quorum.encoded_size()?
            + ManifestSigner::vec_encoded_size(&self.signers)?;
        if let Some(prologue) = &self.prologue {
            out += prologue.encoded_size()?;
        }
        if let Some(linked) = &self.linked {
            out += encoded_size_usize(linked.len()) + 32 * linked.len();
        }
        if let Some(user_data) = &self.user_data {
            out += user_data.encoded_size()?;
        }
        Ok(out)
    }

    fn encode<'a>(&self, buffer: &'a mut [u8]) -> Result<&'a mut [u8], EncodingError> {
        let rest = self.version.encode(buffer)?;
        if self.version == 0 {
            let rest = 0u64.encode(rest)?; // 'blake2b'
            return match (&self.prologue, self.signers.len()) {
                (Some(prologue), 0) => {
                    let rest = 0u64.encode(rest)?; // type 0
                    write_array(&prologue.hash, rest)
                }
                (None, 1) if self.quorum == 1 && !self.allow_patch => {
                    let rest = 1u64.encode(rest)?; // type 1
                    self.signers[0].encode(rest)
                }
                _ => {
                    let rest = 2u64.encode(rest)?; // type 2
                    let rest = (if self.allow_patch { 1u64 } else { 0u64 }).encode(rest)?;
                    let rest = self.quorum.encode(rest)?;
                    self.signers.encode(rest)
                }
            };
        }
        let mut flags: u8 = 0;
        if self.allow_patch {
            flags |= MANIFEST_PATCH;
        }
        if self.prologue.is_some() {
            flags |= MANIFEST_PROLOGUE;
        }
        if self.linked.is_some() {
            flags |= MANIFEST_LINKED;
        }
        if self.user_data.is_some() {
            flags |= MANIFEST_USER_DATA;
        }
        let rest = write_array(&[flags], rest)?;
        let rest = 0u64.encode(rest)?; // 'blake2b'
        let rest = self.quorum.encode(rest)?;
        let mut rest = self.signers.encode(rest)?;
        if let Some(prologue) = &self.prologue {
            rest = prologue.encode(rest)?;
        }
        if let Some(linked) = &self.linked {
            rest = (linked.len() as u64).encode(rest)?;
            for key in linked {
                rest = write_array(key, rest)?;
            }
        }
        if let Some(user_data) = &self.user_data {
            rest = user_data.encode(rest)?;
        }
        Ok(rest)
    }

    fn decode(buffer: &[u8]) -> Result<(Self, &[u8]), EncodingError>
    where
        Self: Sized,
    {
        let (version, rest) = u64::decode(buffer)?;
        if version == 0 {
            known_algorithm_tag(rest, "hash")?;
            let (_, rest) = u64::decode(rest)?;
            let (manifest_type, rest) = u64::decode(rest)?;
            return match manifest_type {
                0 => {
                    let (hash, rest) = take_array::<32>(rest)?;
                    Ok((
                        Manifest {
                            version: 0,
                            allow_patch: false,
                            quorum: 0,
                            signers: vec![],
                            prologue: Some(ManifestPrologue { hash, length: 0 }),
                            linked: None,
                            user_data: None,
                        },
                        rest,
                    ))
                }
                1 => {
                    let (signer, rest) = ManifestSigner::decode(rest)?;
                    Ok((
                        Manifest {
                            version: 0,
                            allow_patch: false,
                            quorum: 1,
                            signers: vec![signer],
                            prologue: None,
                            linked: None,
                            user_data: None,
                        },
                        rest,
                    ))
                }
                2 => {
                    let (flags, rest) = u64::decode(rest)?;
                    let (quorum, rest) = u64::decode(rest)?;
                    let (signers, rest) = <Vec<ManifestSigner>>::decode(rest)?;
                    Ok((
                        Manifest {
                            version: 0,
                            allow_patch: flags & 1 != 0,
                            quorum,
                            signers,
                            prologue: None,
                            linked: None,
                            user_data: None,
                        },
                        rest,
                    ))
                }
                t => Err(EncodingError::new(
                    EncodingErrorKind::InvalidData,
                    &format!("Unknown type: {t}"),
                )),
            };
        }
        if version > 2 {
            return Err(EncodingError::new(
                EncodingErrorKind::InvalidData,
                &format!("Unknown version: {version}"),
            ));
        }
        let ([flags], rest) = take_array::<1>(rest)?;
        known_algorithm_tag(rest, "hash")?;
        let (_, rest) = u64::decode(rest)?;
        let (quorum, rest) = u64::decode(rest)?;
        let (signers, mut rest) = <Vec<ManifestSigner>>::decode(rest)?;
        let prologue = if flags & MANIFEST_PROLOGUE != 0 {
            let (prologue, new_rest) = ManifestPrologue::decode(rest)?;
            rest = new_rest;
            Some(prologue)
        } else {
            None
        };
        let linked = if flags & MANIFEST_LINKED != 0 {
            let (len, mut new_rest) = decode_usize(rest)?;
            let mut keys = Vec::with_capacity(len);
            for _ in 0..len {
                let (key, r) = take_array::<32>(new_rest)?;
                keys.push(key);
                new_rest = r;
            }
            rest = new_rest;
            Some(keys)
        } else {
            None
        };
        let user_data = if flags & MANIFEST_USER_DATA != 0 {
            let (data, new_rest) = <Vec<u8>>::decode(rest)?;
            rest = new_rest;
            Some(data)
        } else {
            None
        };
        Ok((
            Manifest {
                version,
                allow_patch: flags & MANIFEST_PATCH != 0,
                quorum,
                signers,
                prologue,
                linked,
                user_data,
            },
            rest,
        ))
    }
}

/// One proof inside a [`MultiSignature`] (v1 encoding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultisigProof {
    /// Index of the signer in the manifest's signer list.
    pub signer: u64,
    /// Raw ed25519 signature.
    pub signature: [u8; 64],
    /// Length of the patch upgrade, 0 for none.
    pub patch: u64,
}

impl CompactEncoding for MultisigProof {
    fn encoded_size(&self) -> Result<usize, EncodingError> {
        Ok(self.signer.encoded_size()? + 64 + self.patch.encoded_size()?)
    }

    fn encode<'a>(&self, buffer: &'a mut [u8]) -> Result<&'a mut [u8], EncodingError> {
        let rest = self.signer.encode(buffer)?;
        let rest = write_array(&self.signature, rest)?;
        self.patch.encode(rest)
    }

    fn decode(buffer: &[u8]) -> Result<(Self, &[u8]), EncodingError>
    where
        Self: Sized,
    {
        let (signer, rest) = u64::decode(buffer)?;
        let (signature, rest) = take_array::<64>(rest)?;
        let (patch, rest) = u64::decode(rest)?;
        Ok((
            MultisigProof {
                signer,
                signature,
                patch,
            },
            rest,
        ))
    }
}

impl VecEncodable for MultisigProof {
    fn vec_encoded_size(vec: &[Self]) -> Result<usize, EncodingError>
    where
        Self: Sized,
    {
        let mut out = encoded_size_usize(vec.len());
        for x in vec {
            out += x.encoded_size()?;
        }
        Ok(out)
    }
}

/// Signature container used for version >= 1 manifests. Even single-signer
/// cores wrap their ed25519 signature in this encoding on the wire and in
/// the oplog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiSignature {
    /// Signature proofs, one per participating signer.
    pub proofs: Vec<MultisigProof>,
    /// Shared patch nodes referenced by the proofs.
    pub patch: Vec<Node>,
}

impl CompactEncoding for MultiSignature {
    fn encoded_size(&self) -> Result<usize, EncodingError> {
        Ok(MultisigProof::vec_encoded_size(&self.proofs)? + Node::vec_encoded_size(&self.patch)?)
    }

    fn encode<'a>(&self, buffer: &'a mut [u8]) -> Result<&'a mut [u8], EncodingError> {
        let rest = self.proofs.encode(buffer)?;
        self.patch.encode(rest)
    }

    fn decode(buffer: &[u8]) -> Result<(Self, &[u8]), EncodingError>
    where
        Self: Sized,
    {
        let (proofs, rest) = <Vec<MultisigProof>>::decode(buffer)?;
        let (patch, rest) = <Vec<Node>>::decode(rest)?;
        Ok((MultiSignature { proofs, patch }, rest))
    }
}

/// Hash a manifest to derive the core key of a version >= 1 core.
/// `manifestHash` in Javascript.
pub fn manifest_hash(manifest: &Manifest) -> Result<[u8; 32], EncodingError> {
    let size = 32 + manifest.encoded_size()?;
    let mut buffer = vec![0u8; size];
    buffer[..32].copy_from_slice(&MANIFEST);
    manifest.encode(&mut buffer[32..])?;
    let mut hasher = Blake2b256::new();
    hasher.update(&buffer);
    let mut out = [0u8; 32];
    out.copy_from_slice(hasher.finalize().as_slice());
    Ok(out)
}

/// Signable buffer for version >= 1 manifest signatures:
/// `caps.treeSignable(manifestHash, treeHash, length, fork)` in Javascript.
pub fn tree_signable_v1(
    manifest_hash: &[u8; 32],
    tree_hash: &[u8; 32],
    length: u64,
    fork: u64,
) -> [u8; 112] {
    let mut out = [0u8; 112];
    out[0..32].copy_from_slice(&TREE);
    out[32..64].copy_from_slice(manifest_hash);
    out[64..96].copy_from_slice(tree_hash);
    out[96..104].copy_from_slice(&length.to_le_bytes());
    out[104..112].copy_from_slice(&fork.to_le_bytes());
    out
}

/// Verify an assembled [`MultiSignature`] against a version >= 1 manifest.
/// Signature patches are not supported (rejected as invalid); listam cores
/// have `allowPatch: false` so this covers all single-signer and plain
/// quorum multisig cores.
pub fn verify_manifest_signature(
    manifest: &Manifest,
    manifest_hash: &[u8; 32],
    tree_hash: &[u8; 32],
    length: u64,
    fork: u64,
    assembled_signature: &[u8],
) -> bool {
    use ed25519_dalek::{Signature, Verifier as _};

    if manifest.version == 0 || manifest.quorum == 0 {
        return false;
    }
    let Ok((multisig, _)) = MultiSignature::decode(assembled_signature) else {
        return false;
    };
    if (multisig.proofs.len() as u64) < manifest.quorum {
        return false;
    }
    let signable = tree_signable_v1(manifest_hash, tree_hash, length, fork);
    let mut tried = vec![false; manifest.signers.len()];
    for proof in multisig.proofs.iter().take(manifest.quorum as usize) {
        // Patched (partial) signatures would need tree reconstruction; reject.
        if proof.patch != 0 {
            return false;
        }
        let signer_index = proof.signer as usize;
        if signer_index >= manifest.signers.len() || tried[signer_index] {
            return false;
        }
        tried[signer_index] = true;
        let signer = &manifest.signers[signer_index];
        let Ok(public_key) = VerifyingKey::from_bytes(&signer.public_key) else {
            return false;
        };
        let signature = Signature::from_bytes(&proof.signature);
        if public_key.verify(&signable, &signature).is_err() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    use self::data_encoding::HEXLOWER;
    use data_encoding;

    fn hash_with_extra_byte(data: &[u8], byte: u8) -> Box<[u8]> {
        let mut hasher = Blake2b256::new();
        hasher.update(data);
        hasher.update([byte]);
        let hash = hasher.finalize();
        hash.as_slice().into()
    }

    fn hex_bytes(hex: &str) -> Vec<u8> {
        HEXLOWER.decode(hex.as_bytes()).unwrap()
    }

    fn check_hash(hash: Hash, hex: &str) {
        assert_eq!(hash.as_bytes(), &hex_bytes(hex)[..]);
    }

    #[test]
    fn leaf_hash() {
        check_hash(
            Hash::from_leaf(&[]),
            "5187b7a8021bf4f2c004ea3a54cfece1754f11c7624d2363c7f4cf4fddd1441e",
        );
        check_hash(
            Hash::from_leaf(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
            "e1001bb0bb9322b6b202b2f737dc12181b11727168d33ca48ffe361c66cd1abe",
        );
    }

    #[test]
    fn parent_hash() {
        let d1: &[u8] = &[0, 1, 2, 3, 4];
        let d2: &[u8] = &[42, 43, 44, 45, 46, 47, 48];
        let node1 = Node::new(0, Hash::from_leaf(d1).as_bytes().to_vec(), d1.len() as u64);
        let node2 = Node::new(1, Hash::from_leaf(d2).as_bytes().to_vec(), d2.len() as u64);
        check_hash(
            Hash::from_hashes(&node1, &node2),
            "6fac58578fa385f25a54c0637adaca71fdfddcea885d561f33d80c4487149a14",
        );
        check_hash(
            Hash::from_hashes(&node2, &node1),
            "6fac58578fa385f25a54c0637adaca71fdfddcea885d561f33d80c4487149a14",
        );
    }

    #[test]
    fn root_hash() {
        let d1: &[u8] = &[0, 1, 2, 3, 4];
        let d2: &[u8] = &[42, 43, 44, 45, 46, 47, 48];
        let node1 = Node::new(0, Hash::from_leaf(d1).as_bytes().to_vec(), d1.len() as u64);
        let node2 = Node::new(1, Hash::from_leaf(d2).as_bytes().to_vec(), d2.len() as u64);
        check_hash(
            Hash::from_roots(&[&node1, &node2]),
            "2d117e0bb15c6e5236b6ce764649baed1c41890da901a015341503146cc20bcd",
        );
        check_hash(
            Hash::from_roots(&[&node2, &node1]),
            "9826c8c2d28fc309cce73a4b6208e83e5e4b0433d2369bfbf8858272153849f1",
        );
    }

    #[test]
    fn discovery_key_hashing() -> Result<(), ed25519_dalek::SignatureError> {
        let public_key = VerifyingKey::from_bytes(&[
            119, 143, 141, 149, 81, 117, 201, 46, 76, 237, 94, 79, 85, 99, 246, 155, 254, 192, 200,
            108, 198, 246, 112, 53, 44, 69, 121, 67, 102, 111, 230, 57,
        ])?;

        let expected = &[
            37, 167, 138, 168, 22, 21, 132, 126, 186, 0, 153, 93, 242, 157, 212, 29, 126, 227, 15,
            59, 1, 248, 146, 32, 159, 121, 183, 90, 87, 217, 137, 225,
        ];

        assert_eq!(Hash::for_discovery_key(public_key).as_bytes(), expected);

        Ok(())
    }

    // The following uses test data from
    // https://github.com/mafintosh/hypercore-crypto/blob/master/test.js

    #[test]
    fn hash_leaf() {
        let data = b"hello world";
        check_hash(
            Hash::data(data),
            "9f1b578fd57a4df015493d2886aec9600eef913c3bb009768c7f0fb875996308",
        );
    }

    #[test]
    fn hash_parent() {
        let data = b"hello world";
        let len = data.len() as u64;
        let node1 = Node::new(0, Hash::data(data).as_bytes().to_vec(), len);
        let node2 = Node::new(1, Hash::data(data).as_bytes().to_vec(), len);
        check_hash(
            Hash::parent(&node1, &node2),
            "3ad0c9b58b771d1b7707e1430f37c23a23dd46e0c7c3ab9c16f79d25f7c36804",
        );
    }

    #[test]
    fn hash_tree() {
        let hash: [u8; 32] = [0; 32];
        let node1 = Node::new(3, hash.to_vec(), 11);
        let node2 = Node::new(9, hash.to_vec(), 2);
        check_hash(
            Hash::tree(&[&node1, &node2]),
            "0e576a56b478cddb6ffebab8c494532b6de009466b2e9f7af9143fc54b9eaa36",
        );
    }

    // This is the rust version from
    // https://github.com/hypercore-protocol/hypercore/blob/70b271643c4e4b1e5ecae5bb579966dfe6361ff3/lib/caps.js
    // and validates that our arrays match
    #[test]
    fn hash_namespace() {
        let mut hasher = Blake2b256::new();
        hasher.update(HYPERCORE);
        let hash = hasher.finalize();
        let ns = hash.as_slice();
        let tree: Box<[u8]> = { hash_with_extra_byte(ns, 0) };
        assert_eq!(tree, TREE.into());
    }

    // -------------------------------------------------------------------
    // v11 manifest golden vectors, generated against the Javascript
    // implementation (hypercore 11.33.1 / corestore 7.10.1) by
    // hardware/leaf-peer/bridge-js/gen-vectors.mjs. See testdata/vectors.json.
    // -------------------------------------------------------------------

    const V_PUBLIC_KEY: &str = "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const V_MANIFEST_ENCODED: &str = "010000010100ababababababababababababababababababababababababababababababababea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const V_MANIFEST_HASH: &str = "2280b9e0251394553617f6a74aa85ff7d058b824434564a36065f9372b8498e0";
    const V_DEFAULT_NS_ENCODED: &str = "0100000101004144eea531e483d54e0c14f4ca68e0644f355343ff6fcb0f005200e12cd747cbea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const V_DEFAULT_NS_HASH: &str = "04dc2c70d9b3954c86f4d0ffeae59b601b9739d285d5bb1849a9f93f45d95c18";
    const V_SIGNABLE: &str = "9fac70b50ca14efc4e91c833b204e75b8b5aad8b5881bfc0adb5ef38a3275b9c2280b9e0251394553617f6a74aa85ff7d058b824434564a36065f9372b8498e0424242424242424242424242424242424242424242424242424242424242424204000000000000000000000000000000";
    const V_RAW_SIGNATURE: &str = "cf4d39136c774a8155424876c28abdb6897d4b26ee0454f259611d8ad20c93dbe3f445144c50cc7754bcd1a7471c5b6cd298ed914e086b249dd7f5c51f1af408";
    const V_ASSEMBLED: &str = "0100cf4d39136c774a8155424876c28abdb6897d4b26ee0454f259611d8ad20c93dbe3f445144c50cc7754bcd1a7471c5b6cd298ed914e086b249dd7f5c51f1af4080000";
    const V_REAL_CORE_KEY: &str = "1940c9c5e3aab9314e16117cc151ba68958845a28e97dcc4b45bc66d65f867b2";
    const V_REAL_MANIFEST_ENCODED: &str = "0100000101004144eea531e483d54e0c14f4ca68e0644f355343ff6fcb0f005200e12cd747cb18210e81bd32abfc2f88bcb275a3fb321cb06b8448c9ac73fd7cbb2c4b6eb3a7";

    fn to_arr32(bytes: &[u8]) -> [u8; 32] {
        bytes.try_into().unwrap()
    }

    #[test]
    fn manifest_decode_reencode_custom_namespace() {
        let encoded = hex_bytes(V_MANIFEST_ENCODED);
        let (manifest, rest) = Manifest::decode(&encoded).unwrap();
        assert!(rest.is_empty());
        assert_eq!(manifest.version, 1);
        assert!(!manifest.allow_patch);
        assert_eq!(manifest.quorum, 1);
        assert_eq!(manifest.signers.len(), 1);
        assert_eq!(manifest.signers[0].namespace, [0xab; 32]);
        assert_eq!(
            manifest.signers[0].public_key,
            to_arr32(&hex_bytes(V_PUBLIC_KEY))
        );
        assert_eq!(manifest.prologue, None);

        let size = manifest.encoded_size().unwrap();
        assert_eq!(size, encoded.len());
        let mut buffer = vec![0u8; size];
        manifest.encode(&mut buffer).unwrap();
        assert_eq!(buffer, encoded);
    }

    #[test]
    fn manifest_hash_matches_js() {
        let encoded = hex_bytes(V_MANIFEST_ENCODED);
        let (manifest, _) = Manifest::decode(&encoded).unwrap();
        assert_eq!(
            manifest_hash(&manifest).unwrap(),
            to_arr32(&hex_bytes(V_MANIFEST_HASH))
        );

        let default_manifest = Manifest::default_signer(to_arr32(&hex_bytes(V_PUBLIC_KEY)));
        let size = default_manifest.encoded_size().unwrap();
        let mut buffer = vec![0u8; size];
        default_manifest.encode(&mut buffer).unwrap();
        assert_eq!(buffer, hex_bytes(V_DEFAULT_NS_ENCODED));
        assert_eq!(
            manifest_hash(&default_manifest).unwrap(),
            to_arr32(&hex_bytes(V_DEFAULT_NS_HASH))
        );
    }

    #[test]
    fn real_corestore_manifest_hashes_to_core_key() {
        let encoded = hex_bytes(V_REAL_MANIFEST_ENCODED);
        let (manifest, _) = Manifest::decode(&encoded).unwrap();
        assert_eq!(manifest.signers[0].namespace, DEFAULT_NAMESPACE);
        assert_eq!(
            manifest_hash(&manifest).unwrap(),
            to_arr32(&hex_bytes(V_REAL_CORE_KEY))
        );
    }

    #[test]
    fn tree_signable_v1_matches_js() {
        let signable = tree_signable_v1(
            &to_arr32(&hex_bytes(V_MANIFEST_HASH)),
            &[0x42; 32],
            4,
            0,
        );
        assert_eq!(signable.to_vec(), hex_bytes(V_SIGNABLE));
    }

    #[test]
    fn multisig_decode_reencode() {
        let assembled = hex_bytes(V_ASSEMBLED);
        let (multisig, rest) = MultiSignature::decode(&assembled).unwrap();
        assert!(rest.is_empty());
        assert_eq!(multisig.proofs.len(), 1);
        assert_eq!(multisig.proofs[0].signer, 0);
        assert_eq!(multisig.proofs[0].patch, 0);
        assert_eq!(
            multisig.proofs[0].signature.to_vec(),
            hex_bytes(V_RAW_SIGNATURE)
        );
        assert!(multisig.patch.is_empty());

        let size = multisig.encoded_size().unwrap();
        assert_eq!(size, assembled.len());
        let mut buffer = vec![0u8; size];
        multisig.encode(&mut buffer).unwrap();
        assert_eq!(buffer, assembled);
    }

    #[test]
    fn verify_manifest_signature_with_js_vector() {
        let (manifest, _) = Manifest::decode(&hex_bytes(V_MANIFEST_ENCODED)).unwrap();
        let mh = to_arr32(&hex_bytes(V_MANIFEST_HASH));
        let assembled = hex_bytes(V_ASSEMBLED);
        assert!(verify_manifest_signature(
            &manifest, &mh, &[0x42; 32], 4, 0, &assembled
        ));
        // wrong length must fail
        assert!(!verify_manifest_signature(
            &manifest, &mh, &[0x42; 32], 5, 0, &assembled
        ));
        // tampered signature must fail
        let mut bad = assembled.clone();
        bad[10] ^= 0x01;
        assert!(!verify_manifest_signature(
            &manifest, &mh, &[0x42; 32], 4, 0, &bad
        ));
        // tampered tree hash must fail
        assert!(!verify_manifest_signature(
            &manifest, &mh, &[0x43; 32], 4, 0, &assembled
        ));
    }
}
