use compact_encoding::{
    CompactEncoding, EncodingError, map_decode, map_encode, sum_encoded_size, take_array,
    take_array_mut, write_array,
};

use crate::common::BitfieldUpdate;
use hypercore_schema::Node;

/// Entry tree upgrade
#[derive(Debug)]
pub(crate) struct EntryTreeUpgrade {
    pub(crate) fork: u64,
    pub(crate) ancestors: u64,
    pub(crate) length: u64,
    pub(crate) signature: Box<[u8]>,
}

impl CompactEncoding for EntryTreeUpgrade {
    fn encoded_size(&self) -> Result<usize, EncodingError> {
        Ok(sum_encoded_size!(
            self.fork,
            self.ancestors,
            self.length,
            self.signature
        ))
    }

    fn encode<'a>(&self, buffer: &'a mut [u8]) -> Result<&'a mut [u8], EncodingError> {
        Ok(map_encode!(
            buffer,
            self.fork,
            self.ancestors,
            self.length,
            self.signature
        ))
    }

    fn decode(buffer: &[u8]) -> Result<(Self, &[u8]), EncodingError>
    where
        Self: Sized,
    {
        let ((fork, ancestors, length, signature), rest) =
            map_decode!(buffer, [u64, u64, u64, Box<[u8]>]);
        Ok((
            Self {
                fork,
                ancestors,
                length,
                signature,
            },
            rest,
        ))
    }
}

impl CompactEncoding for BitfieldUpdate {
    fn encoded_size(&self) -> Result<usize, EncodingError> {
        Ok(1 + sum_encoded_size!(self.start, self.length))
    }

    fn encode<'a>(&self, buffer: &'a mut [u8]) -> Result<&'a mut [u8], EncodingError> {
        let drop = if self.drop { 1 } else { 0 };
        let rest = write_array(&[drop], buffer)?;
        Ok(map_encode!(rest, self.start, self.length))
    }

    fn decode(buffer: &[u8]) -> Result<(Self, &[u8]), EncodingError>
    where
        Self: Sized,
    {
        let ([flags], rest) = take_array::<1>(buffer)?;
        let ((start, length), rest) = map_decode!(rest, [u64, u64]);
        Ok((
            BitfieldUpdate {
                drop: flags & 1 == 1,
                start,
                length,
            },
            rest,
        ))
    }
}

/// Oplog Entry
#[derive(Debug)]
pub(crate) struct Entry {
    // TODO: This is a keyValueArray in JS
    pub(crate) user_data: Vec<String>,
    pub(crate) tree_nodes: Vec<Node>,
    pub(crate) tree_upgrade: Option<EntryTreeUpgrade>,
    pub(crate) bitfield: Option<BitfieldUpdate>,
}

impl CompactEncoding for Entry {
    fn encoded_size(&self) -> Result<usize, EncodingError> {
        let mut out = 1; // flags
        if !self.user_data.is_empty() {
            out += self.user_data.encoded_size()?;
        }
        if !self.tree_nodes.is_empty() {
            out += self.tree_nodes.encoded_size()?;
        }
        if let Some(tree_upgrade) = &self.tree_upgrade {
            out += tree_upgrade.encoded_size()?;
        }
        if let Some(bitfield) = &self.bitfield {
            out += bitfield.encoded_size()?;
        }
        Ok(out)
    }

    fn encode<'a>(&self, buffer: &'a mut [u8]) -> Result<&'a mut [u8], EncodingError> {
        let (flag_buf, mut rest) = take_array_mut::<1>(buffer)?;
        let mut flags = 0u8;
        if !self.user_data.is_empty() {
            flags |= 1;
            rest = self.user_data.encode(rest)?;
        }
        if !self.tree_nodes.is_empty() {
            flags |= 2;
            rest = self.tree_nodes.encode(rest)?;
        }
        if let Some(tree_upgrade) = &self.tree_upgrade {
            flags |= 4;
            rest = tree_upgrade.encode(rest)?;
        }
        if let Some(bitfield) = &self.bitfield {
            flags |= 8;
            rest = bitfield.encode(rest)?;
        }
        flag_buf[0] = flags;
        Ok(rest)
    }

    fn decode(buffer: &[u8]) -> Result<(Self, &[u8]), EncodingError>
    where
        Self: Sized,
    {
        let ([flags], rest) = take_array::<1>(buffer)?;
        let (user_data, rest) = if flags & 1 != 0 {
            <Vec<String>>::decode(rest)?
        } else {
            (Default::default(), rest)
        };

        let (tree_nodes, rest) = if flags & 2 != 0 {
            <Vec<Node>>::decode(rest)?
        } else {
            (Default::default(), rest)
        };

        // NB: tree_upgrade and bitfield have their own flag bits (4 and 8);
        // `encode` sets them independently of tree_nodes. The original datrs
        // code checked `flags & 2` here for both, which only worked when every
        // entry carried nodes+upgrade+bitfield together — it overruns on an
        // entry that has, say, tree_nodes + bitfield but no upgrade (flags
        // 0x0a), as replicated data blocks do.
        let (tree_upgrade, rest) = if flags & 4 != 0 {
            let (x, rest) = EntryTreeUpgrade::decode(rest)?;
            (Some(x), rest)
        } else {
            (Default::default(), rest)
        };

        let (bitfield, rest) = if flags & 8 != 0 {
            let (x, rest) = BitfieldUpdate::decode(rest)?;
            (Some(x), rest)
        } else {
            (Default::default(), rest)
        };

        Ok((
            Self {
                user_data,
                tree_nodes,
                tree_upgrade,
                bitfield,
            },
            rest,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypercore_schema::Node;

    fn roundtrip(entry: &Entry) -> Entry {
        let mut bytes = vec![0u8; entry.encoded_size().unwrap()];
        entry.encode(&mut bytes).unwrap();
        let (decoded, rest) = Entry::decode(&bytes).unwrap();
        assert!(rest.is_empty(), "entry decode left trailing bytes");
        decoded
    }

    // Regression: tree_upgrade (flag 4) and bitfield (flag 8) must decode by
    // their own flags, independent of tree_nodes (flag 2). A replicated data
    // block produces an entry with tree_nodes + bitfield but NO upgrade
    // (flags 0x0a), which the original `flags & 2` checks mis-decoded.
    #[test]
    fn entry_nodes_and_bitfield_without_upgrade() {
        let entry = Entry {
            user_data: vec![],
            tree_nodes: vec![Node::new(0, vec![1u8; 32], 5)],
            tree_upgrade: None,
            bitfield: Some(BitfieldUpdate {
                drop: false,
                start: 0,
                length: 1,
            }),
        };
        let decoded = roundtrip(&entry);
        assert_eq!(decoded.tree_nodes.len(), 1);
        assert!(decoded.tree_upgrade.is_none(), "phantom upgrade decoded");
        assert!(decoded.bitfield.is_some(), "bitfield lost");
    }

    #[test]
    fn entry_all_fields() {
        let entry = Entry {
            user_data: vec![],
            tree_nodes: vec![Node::new(0, vec![2u8; 32], 7)],
            tree_upgrade: Some(EntryTreeUpgrade {
                fork: 0,
                ancestors: 0,
                length: 1,
                signature: vec![9u8; 64].into_boxed_slice(),
            }),
            bitfield: Some(BitfieldUpdate {
                drop: false,
                start: 0,
                length: 1,
            }),
        };
        let decoded = roundtrip(&entry);
        assert!(decoded.tree_upgrade.is_some());
        assert!(decoded.bitfield.is_some());
        assert_eq!(decoded.tree_upgrade.unwrap().signature.len(), 64);
    }

    #[test]
    fn entry_upgrade_without_nodes() {
        let entry = Entry {
            user_data: vec![],
            tree_nodes: vec![],
            tree_upgrade: Some(EntryTreeUpgrade {
                fork: 1,
                ancestors: 2,
                length: 3,
                signature: vec![7u8; 70].into_boxed_slice(),
            }),
            bitfield: None,
        };
        let decoded = roundtrip(&entry);
        assert!(decoded.tree_nodes.is_empty());
        let up = decoded.tree_upgrade.expect("upgrade lost");
        assert_eq!((up.fork, up.ancestors, up.length), (1, 2, 3));
        assert!(decoded.bitfield.is_none());
    }
}
