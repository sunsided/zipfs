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

/// Runs the raw-deflate state machine until it writes output, finishes, or stops making
/// progress. Returns the number of bytes written; 0 means the stream ended.
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
            Ok(MZStatus::StreamEnd) | Err(MZError::Buf) => return Ok(0),
            Ok(_) if result.bytes_consumed == 0 => return Ok(0),
            Ok(_) => {}
            Err(_) => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "corrupt deflate stream",
                ));
            }
        }
    }
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
