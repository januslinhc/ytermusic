use std::{fmt, io};

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use super::protocol::{MpvMessage, MpvRequest, ProtocolDiagnostic, decode_line};

pub trait AsyncReadWrite: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

pub type DecodedFrame = Result<MpvMessage, ProtocolDiagnostic>;

const READ_CHUNK_BYTES: usize = 8_192;

pub struct MpvTransport {
    stream: Box<dyn AsyncReadWrite>,
    max_line_bytes: usize,
    line: Vec<u8>,
    discarding_oversized: bool,
    read_chunk: Box<[u8]>,
    read_start: usize,
    read_end: usize,
    poisoned: bool,
}

impl MpvTransport {
    /// Creates a bounded newline-delimited transport.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] when the maximum line length is
    /// zero.
    pub fn new(stream: Box<dyn AsyncReadWrite>, max_line_bytes: usize) -> io::Result<Self> {
        if max_line_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mpv maximum line length must be greater than zero",
            ));
        }
        Ok(Self {
            stream,
            max_line_bytes,
            line: Vec::with_capacity(max_line_bytes.min(4_096)),
            discarding_oversized: false,
            read_chunk: vec![0; READ_CHUNK_BYTES].into_boxed_slice(),
            read_start: 0,
            read_end: 0,
            poisoned: false,
        })
    }

    /// Writes and flushes exactly one newline-delimited request.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe I/O error if serialization, writing, or flushing
    /// fails. Once writing starts, cancellation or an I/O failure poisons the
    /// transport unless the complete frame is written and flushed; a poisoned
    /// transport rejects all later operations rather than risking concatenated
    /// JSON frames.
    pub async fn send(&mut self, request: &MpvRequest) -> io::Result<()> {
        self.ensure_usable()?;
        let encoded = request.to_json_line().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("could not encode mpv request: {error}"),
            )
        })?;
        self.poisoned = true;
        self.stream.write_all(encoded.as_bytes()).await?;
        self.stream.flush().await?;
        self.poisoned = false;
        Ok(())
    }

    /// Reads the next newline-delimited message or diagnostic.
    ///
    /// Oversized frames are discarded through their newline and yield exactly
    /// one diagnostic, so the following valid frame remains readable. Storage
    /// never grows beyond `max_line_bytes`.
    ///
    /// # Errors
    ///
    /// Returns an I/O error only when reading the local transport fails.
    pub async fn receive_next_frame(&mut self) -> io::Result<Option<DecodedFrame>> {
        self.ensure_usable()?;
        loop {
            if self.read_start < self.read_end {
                if let Some(frame) = self.consume_buffered_chunk() {
                    return Ok(Some(frame));
                }
                continue;
            }

            let count = self.stream.read(&mut self.read_chunk).await?;
            if count == 0 {
                return Ok(self.finish_eof());
            }
            self.read_start = 0;
            self.read_end = count;
        }
    }

    fn consume_buffered_chunk(&mut self) -> Option<DecodedFrame> {
        let newline_offset = self.read_chunk[self.read_start..self.read_end]
            .iter()
            .position(|byte| *byte == b'\n');
        let segment_end = newline_offset.map_or(self.read_end, |offset| self.read_start + offset);
        self.consume_segment(self.read_start, segment_end);
        self.read_start = newline_offset.map_or(self.read_end, |_| segment_end + 1);

        newline_offset.map(|_| self.finish_line())
    }

    fn consume_segment(&mut self, start: usize, end: usize) {
        if self.discarding_oversized {
            return;
        }
        let segment_len = end - start;
        let available = self.max_line_bytes - self.line.len();
        let copy_len = segment_len.min(available);
        self.line
            .extend_from_slice(&self.read_chunk[start..start + copy_len]);
        if segment_len > available {
            self.discarding_oversized = true;
        }
    }

    fn finish_line(&mut self) -> DecodedFrame {
        if self.discarding_oversized {
            self.discarding_oversized = false;
            self.line.clear();
            Err(ProtocolDiagnostic::Oversized {
                max_bytes: self.max_line_bytes,
            })
        } else {
            let line = std::mem::take(&mut self.line);
            self.line = Vec::with_capacity(self.max_line_bytes.min(4_096));
            decode_line(&line)
        }
    }

    fn finish_eof(&mut self) -> Option<DecodedFrame> {
        if self.discarding_oversized {
            self.discarding_oversized = false;
            self.line.clear();
            Some(Err(ProtocolDiagnostic::Oversized {
                max_bytes: self.max_line_bytes,
            }))
        } else if self.line.is_empty() {
            None
        } else {
            let bytes_read = self.line.len();
            self.line.clear();
            Some(Err(ProtocolDiagnostic::UnexpectedEof { bytes_read }))
        }
    }

    fn ensure_usable(&self) -> io::Result<()> {
        if self.poisoned {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "mpv transport is unusable after an interrupted write",
            ))
        } else {
            Ok(())
        }
    }
}

impl fmt::Debug for MpvTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MpvTransport")
            .field("max_line_bytes", &self.max_line_bytes)
            .field("buffered_bytes", &self.line.len())
            .field("discarding_oversized", &self.discarding_oversized)
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}
