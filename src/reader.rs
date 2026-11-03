//! Streaming reader for the contents of one archive entry.

use std::io::{self, ErrorKind, Read};
use std::ops::Range;

use crate::crc32::Crc32;
use crate::format::Method;

/// Compressed (or stored) input for a [`Decoding`]: the bytes not yet consumed.
///
/// Implemented for borrowed slices, used by [`EntryReader`], and for [`OwnedSource`], which
/// keeps its own handle to the archive data so a decoder can live as long as a `ZipFS` file.
pub(crate) trait Source {
    fn remaining(&self) -> &[u8];
    fn consume(&mut self, count: usize);
}

impl Source for &[u8] {
    fn remaining(&self) -> &[u8] {
        self
    }

    fn consume(&mut self, count: usize) {
        *self = &self[count..];
    }
}

/// Input that owns a handle to the archive bytes, plus the range still to be read.
#[cfg_attr(not(feature = "lunchbox"), allow(dead_code))]
pub(crate) struct OwnedSource<D> {
    data: D,
    range: Range<usize>,
}

#[cfg_attr(not(feature = "lunchbox"), allow(dead_code))]
impl<D: AsRef<[u8]>> OwnedSource<D> {
    /// `range` must lie within `data`.
    pub(crate) fn new(data: D, range: Range<usize>) -> Self {
        Self { data, range }
    }
}

impl<D: AsRef<[u8]>> Source for OwnedSource<D> {
    fn remaining(&self) -> &[u8] {
        &self.data.as_ref()[self.range.clone()]
    }

    fn consume(&mut self, count: usize) {
        self.range.start += count;
    }
}

/// Adapts a [`Source`] to [`Read`], for decoders that pull their input.
struct SourceReader<S>(S);

impl<S: Source> Read for SourceReader<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.0.remaining();
        let count = remaining.len().min(buf.len());
        buf[..count].copy_from_slice(&remaining[..count]);
        self.0.consume(count);
        Ok(count)
    }
}

/// Reads (and decompresses) one entry, checking its size and CRC-32.
///
/// Output is capped at the uncompressed size declared in the central directory: a stream that
/// would produce more fails with [`ErrorKind::InvalidData`] as soon as it does, so a small,
/// highly compressed entry with a forged size cannot expand without bound. At the end of the
/// stream the total size and the CRC-32 are verified.
pub struct EntryReader<'a>(Decoding<&'a [u8]>);

impl<'a> EntryReader<'a> {
    /// Creates a reader over an entry's raw data.
    pub(crate) fn new(
        raw: &'a [u8],
        method: Method,
        uncompressed_size: u64,
        crc32: u32,
    ) -> io::Result<Self> {
        Decoding::new(raw, method, uncompressed_size, crc32).map(Self)
    }
}

impl Read for EntryReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

/// Incremental decoding of one entry from a [`Source`], with the checks of [`EntryReader`].
///
/// Memory stays bounded by the decoder's working set (the Deflate window, or the Zstandard
/// window as far as output has been produced), not by the size of the entry.
pub(crate) struct Decoding<S: Source> {
    decoder: Decoder<S>,
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

enum Decoder<S: Source> {
    Stored(SourceReader<S>),
    #[cfg(feature = "deflate")]
    Deflate {
        state: Box<miniz_oxide::inflate::stream::InflateState>,
        input: S,
    },
    #[cfg(feature = "zstd")]
    Zstd(Box<ruzstd::decoding::StreamingDecoder<SourceReader<S>, ruzstd::decoding::FrameDecoder>>),
}

impl<S: Source> Decoding<S> {
    pub(crate) fn new(
        input: S,
        method: Method,
        uncompressed_size: u64,
        crc32: u32,
    ) -> io::Result<Self> {
        let decoder = match method {
            Method::Stored => Decoder::Stored(SourceReader(input)),
            #[cfg(feature = "deflate")]
            Method::Deflate => Decoder::Deflate {
                state: miniz_oxide::inflate::stream::InflateState::new_boxed(
                    miniz_oxide::DataFormat::Raw,
                ),
                input,
            },
            // ruzstd rejects frames whose window exceeds 128 MiB and grows its ring buffer as
            // output is produced rather than allocating the window up front. Its streaming
            // decoder decodes only as far as the bytes requested, and `read` asks for at most
            // the declared size plus one byte, so decoder memory stays within the declared size
            // plus one block (128 KiB). The window is not capped at the entry size: streaming
            // encoders use the level's default window (2-8 MiB) even for small files.
            #[cfg(feature = "zstd")]
            Method::Zstd => Decoder::Zstd(Box::new(
                ruzstd::decoding::StreamingDecoder::new(SourceReader(input)).map_err(|err| {
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

impl<S: Source> Read for Decoding<S> {
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
fn inflate<S: Source>(
    state: &mut miniz_oxide::inflate::stream::InflateState,
    input: &mut S,
    out: &mut [u8],
) -> io::Result<usize> {
    use miniz_oxide::{MZError, MZFlush, MZStatus};

    loop {
        let result =
            miniz_oxide::inflate::stream::inflate(state, input.remaining(), out, MZFlush::None);
        input.consume(result.bytes_consumed);
        if result.bytes_written > 0 {
            return Ok(result.bytes_written);
        }
        match result.status {
            Ok(MZStatus::StreamEnd) => return Ok(0),
            Ok(_) | Err(MZError::Buf) if result.bytes_consumed > 0 => {}
            Ok(_) | Err(MZError::Buf) if input.remaining().is_empty() => {
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

/// Upper bound for the up-front allocation when streaming an entry, so a forged size in the
/// central directory cannot trigger a huge allocation before any data has been read.
const MAX_PREALLOCATION: u64 = 16 * 1024 * 1024;

/// Decompresses a whole entry, in one call when its declared size is at most `one_shot_limit`
/// and plausible, and through the streaming [`EntryReader`] otherwise; size and CRC-32 are
/// verified either way.
pub(crate) fn decompress(
    raw: &[u8],
    method: Method,
    uncompressed_size: u64,
    crc32: u32,
    one_shot_limit: u64,
) -> io::Result<Vec<u8>> {
    if uncompressed_size <= one_shot_limit && fits_one_shot(raw.len() as u64, uncompressed_size) {
        return decompress_to_vec(raw, method, uncompressed_size, crc32);
    }
    let capacity = uncompressed_size.min(MAX_PREALLOCATION) as usize;
    let mut out = Vec::with_capacity(capacity);
    EntryReader::new(raw, method, uncompressed_size, crc32)?.read_to_end(&mut out)?;
    Ok(out)
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
            // `decode_all` fills the fixed-size slice in place; it cannot grow the buffer.
            .decode_all(raw, out.as_mut_slice())
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
