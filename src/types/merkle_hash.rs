use serde::{Deserialize, Serialize};
use std::fmt;

use crate::error::{Result, XetError};

/// 256-bit hash value stored as [u64; 4] (little-endian)
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MerkleHash([u64; 4]);

impl MerkleHash {
    pub fn new() -> Self {
        Self([0; 4])
    }

    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        let mut result = [0u64; 4];
        for i in 0..4 {
            result[i] = u64::from_le_bytes([
                bytes[i * 8],
                bytes[i * 8 + 1],
                bytes[i * 8 + 2],
                bytes[i * 8 + 3],
                bytes[i * 8 + 4],
                bytes[i * 8 + 5],
                bytes[i * 8 + 6],
                bytes[i * 8 + 7],
            ]);
        }
        Self(result)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        unsafe { &*(self.0.as_ptr() as *const [u8; 32]) }
    }

    pub fn from_hex(hex_str: &str) -> Result<Self> {
        if hex_str.len() != 64 {
            return Err(XetError::InvalidHashFormat(format!(
                "Expected 64 hex chars, got {}",
                hex_str.len()
            )));
        }

        let bytes = hex::decode(hex_str).map_err(|e| {
            XetError::InvalidHashFormat(format!("Invalid hex: {}", e))
        })?;

        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Self::from_bytes(&arr))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.as_bytes())
    }

    pub fn first_u64(&self) -> u64 {
        self.0[0]
    }
}

impl Default for MerkleHash {
    fn default() -> Self {
        Self::new()
    }
}

impl From<[u8; 32]> for MerkleHash {
    fn from(bytes: [u8; 32]) -> Self {
        Self::from_bytes(&bytes)
    }
}

impl From<MerkleHash> for [u8; 32] {
    fn from(hash: MerkleHash) -> Self {
        *hash.as_bytes()
    }
}

impl std::ops::Rem<u64> for MerkleHash {
    type Output = u64;

    fn rem(self, rhs: u64) -> Self::Output {
        self.0[0] % rhs
    }
}

impl fmt::Debug for MerkleHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MerkleHash({})", self.to_hex())
    }
}

impl fmt::Display for MerkleHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}