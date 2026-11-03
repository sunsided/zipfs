//! Streaming reader for the contents of one archive entry.

use std::io::{self, ErrorKind, Read};

use crate::crc32::Crc32;
use crate::format::Method;

/// Reads (and decompresses) one entry, checking its size and CRC-32.
///
/// Output is capped at the uncompressed size declared in the central directory: a stream that
/// would produce more fails with [`ErrorKind::InvalidData`] as soon as it does, so a small,
/// highly compressed entry with a forged size cannot expand without bound. At the end of the
/// stream the total size and the CRC-32 are verified.
pub struct EntryReader<'a> {
    decoder: Decoder<'a>,
    /// Declared uncompressed bytes not yet returned.
    remaining: u64,
    crc: Crc32,
    expected_crc: u32,
    state: State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Reading,
    Done,
    Failed,
}

enum Decoder<'a> {
    Stored(&'a [u8]),
    #[cfg(feature = "deflate")]
    Deflate {
        state: Box<miniz_oxide::inflate::stream::InflateState>,
        input: &'a [u8],
    },
    #[cfg(feature = "zstd")]
    Zstd(Box<ruzstd::decoding::StreamingDecoder<&'a [u8], ruzstd::decoding::FrameDecoder>>),
}

impl<'a> EntryReader<'a> {
    /// Creates a reader over an entry's raw data.
    pub(crate) fn new(
        raw: &'a [u8],
        method: Method,
        uncompressed_size: u64,
        crc32: u32,
    ) -> io::Result<Self> {
        let decoder = match method {
            Method::Stored => Decoder::Stored(raw),
            #[cfg(feature = "deflate")]
            Method::Deflate => Decoder::Deflate {
                state: miniz_oxide::inflate::stream::InflateState::new_boxed(
                    miniz_oxide::DataFormat::Raw,
                ),
                input: raw,
            },
            // ruzstd rejects frames whose window exceeds 128 MiB and grows its ring buffer as
            // output is produced rather than allocating the window up front. Because `read`
            // asks for at most the declared size plus one byte, decoder memory stays within
            // the declared size plus one block (128 KiB). The window is not capped at the
            // entry size: streaming encoders use the level's default window (2-8 MiB) even for
            // small files.
            #[cfg(feature = "zstd")]
            Method::Zstd => Decoder::Zstd(Box::new(
                ruzstd::decoding::StreamingDecoder::new(raw).map_err(|err| {
                    io::Error::new(ErrorKind::InvalidData, format!("invalid zstd data: {err}"))
                })?,
            )),
            other => return Err(unsupported_method(other)),
        };
        Ok(Self {
            decoder,
            remaining: uncompressed_size,
            crc: Crc32::new(),
            expected_crc: crc32,
            state: State::Reading,
        })
    }

    fn fail(&mut self, kind: ErrorKind, message: &str) -> io::Error {
        self.state = State::Failed;
        io::Error::new(kind, message.to_string())
    }
}

impl Read for EntryReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.state {
            State::Done => return Ok(0),
            State::Failed => {
                return Err(io::Error::other("zip entry reader failed earlier"));
            }
            State::Reading => {}
        }
        if buf.is_empty() {
            return Ok(0);
        }

        // Let the decoder produce at most one byte more than declared: enough to detect an
        // overrun without decompressing further.
        let limit = usize::try_from(self.remaining.saturating_add(1))
            .map_or(buf.len(), |limit| limit.min(buf.len()));
        let out = &mut buf[..limit];

        let written = match &mut self.decoder {
            Decoder::Stored(input) => input.read(out)?,
            #[cfg(feature = "deflate")]
            Decoder::Deflate { state, input } => match inflate(state, input, out) {
                Ok(written) => written,
                Err(err) => {
                    self.state = State::Failed;
                    return Err(err);
                }
            },
            #[cfg(feature = "zstd")]
            Decoder::Zstd(decoder) => match decoder.read(out) {
                Ok(written) => written,
                Err(err) => {
                    self.state = State::Failed;
                    return Err(io::Error::new(ErrorKind::InvalidData, err));
                }
            },
        };

        if written as u64 > self.remaining {
            return Err(self.fail(
                ErrorKind::InvalidData,
                "zip entry decompresses to more than its declared size",
            ));
        }
        if written > 0 {
            self.remaining -= written as u64;
            self.crc.update(&out[..written]);
            return Ok(written);
        }

        // End of the stream: the size and checksum must match the central directory.
        if self.remaining != 0 {
            return Err(self.fail(
                ErrorKind::UnexpectedEof,
                "zip entry ended before its declared size",
            ));
        }
        if self.crc.finish() != self.expected_crc {
            return Err(self.fail(ErrorKind::InvalidData, "zip entry CRC-32 mismatch"));
        }
        self.state = State::Done;
        Ok(0)
    }
}

/// Runs the raw-deflate state machine until it writes output or reaches the end of the stream.
///
/// Returns the number of bytes written; 0 means the stream ended at its final block. Input that
/// runs out before that is [`ErrorKind::UnexpectedEof`], and input the decoder cannot make
/// progress on is [`ErrorKind::InvalidData`], so a truncated stream is never mistaken for a
/// complete one even if it already produced the declared number of bytes.
#[cfg(feature = "deflate")]
fn inflate(
    state: &mut miniz_oxide::inflate::stream::InflateState,
    input: &mut &[u8],
    out: &mut [u8],
) -> io::Result<usize> {
    use miniz_oxide::{MZError, MZFlush, MZStatus};

    loop {
        let result = miniz_oxide::inflate::stream::inflate(state, input, out, MZFlush::None);
        *input = &input[result.bytes_consumed..];
        if result.bytes_written > 0 {
            return Ok(result.bytes_written);
        }
        match result.status {
            Ok(MZStatus::StreamEnd) => return Ok(0),
            Ok(_) | Err(MZError::Buf) if result.bytes_consumed > 0 => {}
            Ok(_) | Err(MZError::Buf) if input.is_empty() => {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "deflate stream is truncated",
                ));
            }
            Ok(_) | Err(MZError::Buf) => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "deflate stream makes no progress",
                ));
            }
            Err(_) => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "corrupt deflate stream",
                ));
            }
        }
    }
}

/// Deflate cannot expand data by more than about 1032:1. Declared sizes beyond that are
/// impossible for Deflate and implausible in general, so they are not allocated up front.
const MAX_ONE_SHOT_RATIO: u64 = 1032;

/// Whether an entry is small enough relative to its compressed size to be decompressed in one
/// call into a buffer of its declared size.
///
/// One-shot decoding allocates the declared size before reading any data, so a forged size
/// must not cause a large allocation (on wasm, memory growth is committed). Entries above the
/// bound are read with the streaming [`EntryReader`], which grows its buffer with the output.
pub(crate) fn fits_one_shot(compressed_size: u64, uncompressed_size: u64) -> bool {
    uncompressed_size <= compressed_size.saturating_mul(MAX_ONE_SHOT_RATIO)
}

/// Decompresses a whole entry in one call into a buffer of exactly its declared size, then
/// verifies the size and CRC-32.
///
/// The buffer size doubles as the output cap: a stream that would produce more fails. Errors
/// match [`EntryReader`]: [`ErrorKind::InvalidData`] for corrupt data, excess output or a
/// checksum mismatch, [`ErrorKind::UnexpectedEof`] for truncated data.
pub(crate) fn decompress_to_vec(
    raw: &[u8],
    method: Method,
    uncompressed_size: u64,
    crc32: u32,
) -> io::Result<Vec<u8>> {
    let len = usize::try_from(uncompressed_size).map_err(|_| {
        io::Error::new(
            ErrorKind::InvalidData,
            "zip entry is too large for this platform",
        )
    })?;
    let mut out = vec![0u8; len];
    let written = match method {
        #[cfg(feature = "deflate")]
        Method::Deflate => {
            use miniz_oxide::inflate::TINFLStatus;
            miniz_oxide::inflate::decompress_slice_iter_to_slice(
                &mut out,
                std::iter::once(raw),
                false,
                true,
            )
            .map_err(|status| match status {
                TINFLStatus::HasMoreOutput => io::Error::new(
                    ErrorKind::InvalidData,
                    "zip entry decompresses to more than its declared size",
                ),
                TINFLStatus::FailedCannotMakeProgress => {
                    io::Error::new(ErrorKind::UnexpectedEof, "deflate stream is truncated")
                }
                _ => io::Error::new(ErrorKind::InvalidData, "corrupt deflate stream"),
            })?
        }
        #[cfg(feature = "zstd")]
        Method::Zstd => ruzstd::decoding::FrameDecoder::new()
            .decode_all(raw, &mut out)
            .map_err(|err| {
                io::Error::new(ErrorKind::InvalidData, format!("invalid zstd data: {err}"))
            })?,
        Method::Stored => {
            if raw.len() != len {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "stored entry size does not match its declared size",
                ));
            }
            out.copy_from_slice(raw);
            len
        }
        other => return Err(unsupported_method(other)),
    };
    if written != len {
        return Err(io::Error::new(
            ErrorKind::UnexpectedEof,
            "zip entry ended before its declared size",
        ));
    }
    if crate::crc32::checksum(&out) != crc32 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "zip entry CRC-32 mismatch",
        ));
    }
    Ok(out)
}

pub(crate) fn unsupported_method(method: Method) -> io::Error {
    let hint = match method {
        Method::Deflate => "enable the `deflate` feature of zipfs".to_string(),
        Method::Zstd => "enable the `zstd` feature of zipfs".to_string(),
        other => format!("compression method {} is not supported", other.id()),
    };
    io::Error::new(
        ErrorKind::Unsupported,
        format!(
            "zip entry is compressed with {method:?}; {hint}, or repack the archive with stored (uncompressed) entries"
        ),
    )
}

/// Whether this build can decompress `method`.
pub(crate) fn is_supported(method: Method) -> bool {
    match method {
        Method::Stored => true,
        Method::Deflate => cfg!(feature = "deflate"),
        Method::Zstd => cfg!(feature = "zstd"),
        Method::Other(_) => false,
    }
}
