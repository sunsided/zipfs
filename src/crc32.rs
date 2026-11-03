//! CRC-32 (IEEE 802.3, reflected, polynomial `0xEDB88320`), as used by the zip format.
//!
//! With the `crc32fast` feature (on by default) checksums are computed by the `crc32fast`
//! crate, which uses hardware instructions where available. Otherwise a portable slicing-by-16
//! implementation is used, which processes 16 bytes per step with independent table lookups.

/// Slicing-by-16 lookup tables: `TABLES[0]` is the classic byte-at-a-time table, and
/// `TABLES[k][b]` is the CRC of byte `b` followed by `k` zero bytes.
#[cfg_attr(feature = "crc32fast", allow(dead_code))]
const TABLES: [[u32; 256]; 16] = {
    let mut tables = [[0u32; 256]; 16];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
            bit += 1;
        }
        tables[0][i] = crc;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut k = 1;
        while k < 16 {
            let previous = tables[k - 1][i];
            tables[k][i] = (previous >> 8) ^ tables[0][(previous & 0xFF) as usize];
            k += 1;
        }
        i += 1;
    }
    tables
};

/// Updates a (pre-inverted) CRC state with `data` using slicing-by-16.
#[cfg_attr(feature = "crc32fast", allow(dead_code))]
fn update_slicing(mut crc: u32, data: &[u8]) -> u32 {
    let mut chunks = data.chunks_exact(16);
    for chunk in &mut chunks {
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(chunk);
        let first = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) ^ crc;
        bytes[..4].copy_from_slice(&first.to_le_bytes());
        crc = 0;
        for (index, &byte) in bytes.iter().enumerate() {
            crc ^= TABLES[15 - index][usize::from(byte)];
        }
    }
    for &byte in chunks.remainder() {
        crc = TABLES[0][((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

/// Incremental CRC-32 hasher.
#[derive(Clone)]
pub(crate) struct Crc32 {
    #[cfg(feature = "crc32fast")]
    hasher: crc32fast::Hasher,
    #[cfg(not(feature = "crc32fast"))]
    state: u32,
}

impl Crc32 {
    pub(crate) fn new() -> Self {
        Self {
            #[cfg(feature = "crc32fast")]
            hasher: crc32fast::Hasher::new(),
            #[cfg(not(feature = "crc32fast"))]
            state: !0,
        }
    }

    pub(crate) fn update(&mut self, data: &[u8]) {
        #[cfg(feature = "crc32fast")]
        self.hasher.update(data);
        #[cfg(not(feature = "crc32fast"))]
        {
            self.state = update_slicing(self.state, data);
        }
    }

    pub(crate) fn finish(&self) -> u32 {
        #[cfg(feature = "crc32fast")]
        return self.hasher.clone().finalize();
        #[cfg(not(feature = "crc32fast"))]
        return !self.state;
    }
}

/// CRC-32 of a complete buffer.
pub(crate) fn checksum(data: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(data);
    crc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bit-by-bit reference implementation.
    fn reference(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    0xEDB8_8320 ^ (crc >> 1)
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    fn pseudo_random(len: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    #[test]
    fn check_value() {
        // The standard CRC-32 check value.
        assert_eq!(checksum(b"123456789"), 0xCBF4_3926);
        assert_eq!(checksum(b""), 0);
    }

    #[test]
    fn slicing_matches_reference_for_all_lengths_and_alignments() {
        let data = pseudo_random(300);
        for start in 0..17 {
            for end in start..data.len() {
                let slice = &data[start..end];
                assert_eq!(
                    !update_slicing(!0, slice),
                    reference(slice),
                    "{start}..{end}"
                );
            }
        }
    }

    #[test]
    fn active_backend_matches_reference() {
        let data = pseudo_random(100_000);
        assert_eq!(checksum(&data), reference(&data));
    }

    #[test]
    fn incremental_matches_one_shot() {
        let data = pseudo_random(1000);
        for split in [0, 1, 7, 15, 16, 17, 500, 999, 1000] {
            let mut crc = Crc32::new();
            crc.update(&data[..split]);
            crc.update(&data[split..]);
            assert_eq!(crc.finish(), checksum(&data), "split at {split}");
        }
    }
}
