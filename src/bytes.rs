use crate::types::RawProof;
use super::fields::scalar::*;
use super::proof::PROOF_BYTES_SIZE;
use solana_program::program_error::{
    ProgramError,
    ProgramError::InvalidArgument,
};
use super::fields::utils::*;
use super::types::{ U256, ProofData };

pub fn contains(bytes: U256, buffer: &[u8]) -> bool {
    match find(bytes, buffer) {
        Some(_) => true,
        None => false
    }
}

pub fn not_contains(bytes: U256, buffer: &[u8]) -> bool {
    !contains(bytes, buffer)
}

pub fn find(bytes: U256, buffer: &[u8]) -> Option<usize> {
    let length = buffer.len() / 32;
    'A: for i in 0..length {
        let index = i * 32;
        if buffer[index] == bytes[0] {
            for j in 1..32 {
                if buffer[index + j] != bytes[j] { continue 'A; }
            }
            return Some(i);
        }
    }
    None
}

pub fn bytes_to_u64(bytes: &[u8]) -> u64 {
    let a: [u8; 8] = [bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]];
    u64::from_le_bytes(a)
}

pub fn bytes_to_u32(bytes: &[u8]) -> u32 {
    let a: [u8; 4] = [bytes[0], bytes[1], bytes[2], bytes[3]];
    u32::from_le_bytes(a)
}

pub fn unpack_u64(data: &[u8]) -> Result<(u64, &[u8]), ProgramError> {
    let value = data
        .get(..8)
        .and_then(|slice| slice.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or(InvalidArgument)?;

    Ok((value, &data[8..]))
}

pub fn unpack_32_bytes(data: &[u8]) -> Result<(&[u8], &[u8]), ProgramError> {
    let bytes = data.get(..32)
        .ok_or(ProgramError::InvalidInstructionData)?;

    Ok((bytes, &data[32..]))
}

pub fn unpack_u256(data: &[u8]) -> Result<(U256, &[u8]), ProgramError> {
    let (bytes, data) = unpack_32_bytes(&data)?;
    let word = vec_to_array_32(bytes.to_vec());

    Ok((word, &data))
}

pub fn unpack_limbs(data: &[u8]) -> Result<(ScalarLimbs, &[u8]), ProgramError> {
    let (bytes, data) = unpack_32_bytes(data)?;

    Ok((bytes_to_limbs(bytes), data))
}

pub fn unpack_bool(data: &[u8]) -> Result<(bool, &[u8]), ProgramError> {
    let (&byte, rest) = data.split_first().ok_or(ProgramError::InvalidInstructionData)?;

    Ok((byte == 1, rest))
}

pub fn unpack_raw_proof(data: &[u8]) -> Result<(RawProof, &[u8]), ProgramError> {
    let bytes = data.get(..PROOF_BYTES_SIZE)
        .ok_or(ProgramError::InvalidInstructionData)?;

    let proof: [u8; PROOF_BYTES_SIZE] = bytes.try_into().unwrap();

    Ok((proof, &data[PROOF_BYTES_SIZE..]))
}

pub fn deserialize_u256(data: &[u8]) -> U256 {
    let mut a = [0; 32];
    for i in 0..32 { a[i] = data[i]; }
    a
}

pub fn serialize_u256(value: U256) -> Vec<u8> {
    value.to_vec()
}

pub fn u64_to_u256(value: u64) -> U256 {
    let mut buffer = vec![0; 32];
    let bytes = value.to_le_bytes();
    for (i, &byte) in bytes.iter().enumerate() {
        buffer[i] = byte;
    }
    vec_to_array_32(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unpack_u64() {
        let d: [u8; 8] = [0b00000001, 0, 0, 0, 0, 0, 0, 0b00000000];

        let (v, data) = unpack_u64(&d).unwrap();
        assert_eq!(v, 1);
        assert_eq!(data.len(), 0);
    }

    const SIZE: usize = 256;    // Max using 256 here because of u8 then there are duplicates

    fn generate_buffer() -> Vec<u8> {
        let mut buffer = Vec::new();
        for i in 0..SIZE {
            for _ in 0..32 {
                buffer.push(i as u8);
            }
        }
        buffer
    }

    #[test]
    fn test_find() {
        let buffer = generate_buffer();

        // Finds
        for i in 0..SIZE {
            let bytes = [i as u8; 32];

            let index = find(bytes, &buffer).unwrap();
            assert_eq!(index, i);
        }

        // Doesn't find
        for i in 0..32 {
            let mut bytes = [0; 32];
            bytes[i] = 1;

            assert!(matches!(find(bytes, &buffer), None));
        }
    }

    #[test]
    fn test_contains() {
        let buffer = generate_buffer();

        // Contains
        for i in 0..SIZE {
            let bytes = [i as u8; 32];

            assert!(contains(bytes, &buffer));
            assert_eq!(not_contains(bytes, &buffer), false);
        }

        // Doesn't contain
        for i in 0..32 {
            let mut bytes = [0; 32];
            bytes[i] = 1;

            assert!(not_contains(bytes, &buffer));
            assert_eq!(contains(bytes, &buffer), false);
        }
    }
}