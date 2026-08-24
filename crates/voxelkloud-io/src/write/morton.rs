//! Morton (Z-order) interleaving for the Potree BROTLI encoding.
//!
//! The inverse of `point-data-morton.ts`, which is the one piece of the decoder
//! with a bit-exact oracle. Positions become a 16-byte record and colours an
//! 8-byte one, three components interleaved bit by bit, and the dwords are laid
//! out in an order that is not the obvious one — high word first, low word
//! second, each with its own halves swapped.
//!
//! That ordering is not a choice this code gets to make. It is what the
//! reference decoder reads, and a plausible-looking alternative produces a file
//! that decodes to a cloud shaped like static.

/// Spread the low 8 bits of `value` so bit *i* lands at position 3*i*.
///
/// The inverse of the decoder's `dealign24b`, and held to it by a test over the
/// entire input domain — 256 values in, and every 24-bit code they can produce.
#[inline]
pub fn align24b(value: u32) -> u32 {
    let mut x = value & 0xff;
    x = (x | (x << 8)) & 0x00f00f;
    x = (x | (x << 4)) & 0x0c30c3;
    x = (x | (x << 2)) & 0x249249;
    x
}

/// Extract every third bit from 24 interleaved bits, yielding 8.
///
/// Present so the encoder can be tested against its own inverse rather than
/// against a comment.
#[inline]
pub fn dealign24b(code: u32) -> u32 {
    let mut x = code;
    x = ((x & 0x208208) >> 2) | (x & 0x041041);
    x = ((x & 0x0c00c0) >> 4) | (x & 0x003003);
    x = ((x & 0x00f000) >> 8) | (x & 0x0000_000f);
    x & 0xff
}

/// Interleave one byte of each component into a 24-bit code.
#[inline]
fn interleave(x: u32, y: u32, z: u32) -> u32 {
    align24b(x) | (align24b(y) << 1) | (align24b(z) << 2)
}

/// One 16-byte morton position record.
///
/// The four 24-bit codes carry the four bytes of each component, least
/// significant first. They are packed into four dwords written in the order
/// `mc1, mc0, mc3, mc2` — the decoder reads the high word from bytes 0..8 and
/// the low word from 8..16, and within each takes the low dword first.
pub fn encode_position(x: u32, y: u32, z: u32) -> [u8; 16] {
    let a = interleave(x & 0xff, y & 0xff, z & 0xff);
    let b = interleave((x >> 8) & 0xff, (y >> 8) & 0xff, (z >> 8) & 0xff);
    let c = interleave((x >> 16) & 0xff, (y >> 16) & 0xff, (z >> 16) & 0xff);
    let d = interleave((x >> 24) & 0xff, (y >> 24) & 0xff, (z >> 24) & 0xff);

    let mc3 = a | ((b & 0xff) << 24);
    let mc2 = b >> 8;
    let mc1 = c | ((d & 0xff) << 24);
    let mc0 = d >> 8;

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&mc1.to_le_bytes());
    out[4..8].copy_from_slice(&mc0.to_le_bytes());
    out[8..12].copy_from_slice(&mc3.to_le_bytes());
    out[12..16].copy_from_slice(&mc2.to_le_bytes());
    out
}

/// One 8-byte morton colour record. Same structure, two codes and 16 bits a
/// channel.
pub fn encode_color(r: u16, g: u16, b: u16) -> [u8; 8] {
    let low = interleave(u32::from(r) & 0xff, u32::from(g) & 0xff, u32::from(b) & 0xff);
    let high = interleave(
        u32::from(r >> 8),
        u32::from(g >> 8),
        u32::from(b >> 8),
    );

    let mc1 = low | ((high & 0xff) << 24);
    let mc0 = high >> 8;

    let mut out = [0u8; 8];
    out[0..4].copy_from_slice(&mc1.to_le_bytes());
    out[4..8].copy_from_slice(&mc0.to_le_bytes());
    out
}

/// Read one 16-byte morton position record.
///
/// The inverse of [`encode_position`], and not only a test fixture: reading an
/// existing BROTLI cloud — which is what `voxelkloud optimize` does before it
/// re-encodes one — needs exactly this.
pub fn decode_position(bytes: &[u8; 16]) -> [u32; 3] {
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let mc0 = u32_at(4);
    let mc1 = u32_at(0);
    let mc2 = u32_at(12);
    let mc3 = u32_at(8);

    let a = mc3 & 0x00ff_ffff;
    let b = (mc3 >> 24) | (mc2 << 8);
    let c = mc1 & 0x00ff_ffff;
    let d = (mc1 >> 24) | (mc0 << 8);

    let component = |shift: u32| {
        dealign24b(a >> shift)
            | (dealign24b(b >> shift) << 8)
            | (dealign24b(c >> shift) << 16)
            | (dealign24b(d >> shift) << 24)
    };
    [component(0), component(1), component(2)]
}

/// Read one 8-byte morton colour record.
pub fn decode_color(bytes: &[u8; 8]) -> [u16; 3] {
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let mc0 = u32_at(4);
    let mc1 = u32_at(0);
    let a = mc1 & 0x00ff_ffff;
    let b = (mc1 >> 24) | (mc0 << 8);
    let component = |shift: u32| (dealign24b(a >> shift) | (dealign24b(b >> shift) << 8)) as u16;
    [component(0), component(1), component(2)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_is_the_inverse_of_dealign_over_the_whole_domain() {
        for value in 0..=255u32 {
            assert_eq!(dealign24b(align24b(value)), value, "value {value}");
        }
    }

    #[test]
    fn a_position_survives_the_round_trip() {
        // Including the 32-bit extremes: the reference decoder had a guard that
        // silently discarded the top five bits, so a coordinate above 2^27 is
        // exactly the case that has to be exercised.
        let cases = [
            [0u32, 0, 0],
            [1, 2, 3],
            [255, 256, 65535],
            [1 << 27, 0, 0],
            [u32::MAX, u32::MAX, u32::MAX],
            [0xdead_beef, 0x0bad_c0de, 0x1234_5678],
        ];
        for [x, y, z] in cases {
            let encoded = encode_position(x, y, z);
            assert_eq!(decode_position(&encoded), [x, y, z], "{x} {y} {z}");
        }
    }

    #[test]
    fn a_colour_survives_the_round_trip() {
        for [r, g, b] in [
            [0u16, 0, 0],
            [255, 256, 65535],
            [1, 2, 3],
            [65535, 0, 32768],
        ] {
            let encoded = encode_color(r, g, b);
            assert_eq!(decode_color(&encoded), [r, g, b], "{r} {g} {b}");
        }
    }

    /// The bits actually land where the format says: component 0 in every third
    /// bit starting at 0, component 1 at 1, component 2 at 2.
    #[test]
    fn the_interleave_is_every_third_bit() {
        let code = interleave(0xff, 0, 0);
        assert_eq!(code, 0x249249);
        assert_eq!(interleave(0, 0xff, 0), 0x249249 << 1);
        assert_eq!(interleave(0, 0, 0xff), 0x249249 << 2);
    }
}
