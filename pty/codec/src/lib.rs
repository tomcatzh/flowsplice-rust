//! Bounded raw Snappy blocks using the vendored official 1.2.2 implementation.
//! No framing, checksums or transport policy are added here. Authentication of
//! literal corruption belongs to the authenticated business transport.
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    InvalidData,
    OutputTooLarge,
    AllocationFailed,
}
impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidData => "invalid Snappy block",
            Self::OutputTooLarge => "Snappy output exceeds limit",
            Self::AllocationFailed => "Snappy buffer allocation failed",
        })
    }
}
impl std::error::Error for CodecError {}

unsafe extern "C" {
    fn flowsplice_snappy_length(input: *const u8, size: usize, length: *mut usize) -> i32;
    fn flowsplice_snappy_decode(
        input: *const u8,
        size: usize,
        output: *mut u8,
        capacity: usize,
    ) -> i32;
    #[cfg(feature = "encode")]
    fn flowsplice_snappy_encode(
        input: *const u8,
        size: usize,
        output: *mut u8,
        capacity: usize,
        written: *mut usize,
    ) -> i32;
}

fn buffer(size: usize) -> Result<Vec<u8>, CodecError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(size)
        .map_err(|_| CodecError::AllocationFailed)?;
    output.resize(size, 0);
    Ok(output)
}

/// Decode a complete raw block, refusing its declared size before allocation.
/// An encoded empty block is `[0]`; empty encoded input is invalid.
///
/// # Errors
/// Returns [`CodecError::InvalidData`] for malformed or trailing input,
/// [`CodecError::OutputTooLarge`] when the declared output exceeds the cap,
/// or [`CodecError::AllocationFailed`] if the output buffer cannot be allocated.
pub fn decompress(input: &[u8], max_output_bytes: usize) -> Result<Vec<u8>, CodecError> {
    let mut length = 0;
    // SAFETY: immutable slice and valid writable size value; native call retains nothing.
    if unsafe { flowsplice_snappy_length(input.as_ptr(), input.len(), &raw mut length) } != 0 {
        return Err(CodecError::InvalidData);
    }
    if length > max_output_bytes {
        return Err(CodecError::OutputTooLarge);
    }
    let mut output = buffer(length)?;
    // SAFETY: length was validated and output contains exactly length writable bytes.
    // Empty vectors still provide non-null pointers; Snappy writes zero bytes then.
    if unsafe { flowsplice_snappy_decode(input.as_ptr(), input.len(), output.as_mut_ptr(), length) }
        != 0
    {
        return Err(CodecError::InvalidData);
    }
    Ok(output)
}

/// Encode using official Snappy Level 1; available only to sending business code.
///
/// # Errors
/// Returns [`CodecError::OutputTooLarge`] when the input length or output capacity
/// cannot be represented, [`CodecError::AllocationFailed`] on allocation failure,
/// or [`CodecError::InvalidData`] if the native encoder rejects its parameters.
#[cfg(feature = "encode")]
pub fn compress(input: &[u8]) -> Result<Vec<u8>, CodecError> {
    if input.len() > u32::MAX as usize {
        return Err(CodecError::OutputTooLarge);
    }
    let capacity = input
        .len()
        .checked_add(input.len() / 6)
        .and_then(|n| n.checked_add(32))
        .ok_or(CodecError::OutputTooLarge)?;
    let mut output = buffer(capacity)?;
    let mut written = 0;
    // SAFETY: output has Snappy's overflow-checked maximum capacity; input is valid.
    if unsafe {
        flowsplice_snappy_encode(
            input.as_ptr(),
            input.len(),
            output.as_mut_ptr(),
            capacity,
            &raw mut written,
        )
    } != 0
        || written > capacity
    {
        return Err(CodecError::InvalidData);
    }
    output.truncate(written);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    // Official RawCompress Level 1 output for "hello" (length + literal).
    const HELLO: &[u8] = b"\x05\x10hello";
    #[test]
    fn fixed_fixture_and_exact_cap() -> Result<(), CodecError> {
        assert_eq!(decompress(HELLO, 5)?, b"hello");
        assert_eq!(decompress(HELLO, 4), Err(CodecError::OutputTooLarge));
        assert_eq!(decompress(&[0], 0)?, b"");
        Ok(())
    }
    #[test]
    fn malformed_and_trailing_input() {
        for input in [
            b"".as_slice(),
            &[0x80],
            &[5, 16, b'h'],
            &[1, 1, 0],
            &[0, 0],
            b"\x05\x10hello\x00",
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0],
        ] {
            assert_eq!(
                decompress(input, 1024),
                Err(CodecError::InvalidData),
                "{input:?}"
            );
        }
        assert_eq!(
            decompress(&[0xff, 0xff, 0xff, 0xff, 0x0f], 1024),
            Err(CodecError::OutputTooLarge)
        );
        let mut corrupt = HELLO.to_vec();
        corrupt[1] = 0xff;
        assert_eq!(decompress(&corrupt, 1024), Err(CodecError::InvalidData));
    }
    #[cfg(feature = "encode")]
    #[test]
    fn diverse_round_trips() -> Result<(), CodecError> {
        let mut seed = 0x1234_5678_u32;
        let random: Vec<u8> = (0..131_072)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed.to_le_bytes()[0]
            })
            .collect();
        for input in [
            vec![],
            (0..=255).collect(),
            "终端🦀\0\r\n".repeat(100).into_bytes(),
            vec![b'x'; 131_072],
            random,
        ] {
            let compressed = compress(&input)?;
            assert_eq!(decompress(&compressed, input.len())?, input);
            if !input.is_empty() {
                assert_eq!(
                    decompress(&compressed, input.len() - 1),
                    Err(CodecError::OutputTooLarge)
                );
                assert!(decompress(&compressed[..compressed.len() - 1], input.len()).is_err());
            }
        }
        assert_eq!(compress(b"hello")?, HELLO);
        Ok(())
    }
}
