use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;

use serde::de::DeserializeOwned;

const READ_CHUNK_SIZE: usize = 64 * 1024;
const MAX_JSONL_RECORD_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug)]
pub enum ScanOutcome<T> {
    /// The record was valid JSON and deserialized as the requested type.
    Parsed(T),
    /// The record was not valid JSON for the requested type.
    #[allow(dead_code)]
    Rejected(serde_json::Error),
}

/// Read-only scanner for newline-delimited JSON records, starting from the end.
pub struct ReverseJsonlScanner<R> {
    reader: R,
    next_chunk_end: u64,
    chunk_position: usize,
    chunk: Vec<u8>,
    record_reversed: Vec<u8>,
    max_record_bytes: usize,
}

impl<R> ReverseJsonlScanner<R>
where
    R: Read + Seek,
{
    pub fn new(mut reader: R) -> io::Result<Self> {
        let end = reader.seek(SeekFrom::End(0))?;
        Self::new_at(reader, end)
    }

    /// Creates a reverse scanner whose logical end is the given byte offset.
    ///
    /// This lets callers scan a frozen JSONL prefix without reading records appended after that
    /// prefix was captured.
    pub fn new_at(reader: R, end_byte_offset: u64) -> io::Result<Self> {
        Self::new_before_offset_with_limit(reader, end_byte_offset, MAX_JSONL_RECORD_BYTES)
    }

    pub(crate) fn new_before_offset(reader: R, offset: u64) -> io::Result<Self> {
        Self::new_before_offset_with_limit(reader, offset, MAX_JSONL_RECORD_BYTES)
    }

    fn new_before_offset_with_limit(
        mut reader: R,
        offset: u64,
        max_record_bytes: usize,
    ) -> io::Result<Self> {
        let end = reader.seek(SeekFrom::End(0))?;
        if offset > end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "reverse JSONL scan offset exceeds the reader length",
            ));
        }
        Ok(Self {
            reader,
            next_chunk_end: offset,
            chunk_position: 0,
            chunk: vec![0; READ_CHUNK_SIZE],
            record_reversed: Vec::new(),
            max_record_bytes,
        })
    }

    /// Scans the next nonblank record.
    ///
    /// I/O failures are returned as [`Err`]. Invalid JSON records are returned as
    /// [`ScanOutcome::Rejected`], and the scanner remains usable.
    pub fn scan_next<T>(&mut self) -> io::Result<Option<ScanOutcome<T>>>
    where
        T: DeserializeOwned,
    {
        loop {
            if self.chunk_position == 0 {
                if self.next_chunk_end == 0 {
                    return Ok(self.finish_record());
                }

                let read_size = usize::try_from(self.next_chunk_end.min(READ_CHUNK_SIZE as u64))
                    .map_err(io::Error::other)?;
                self.next_chunk_end -= read_size as u64;
                self.reader.seek(SeekFrom::Start(self.next_chunk_end))?;
                self.reader.read_exact(&mut self.chunk[..read_size])?;
                self.chunk_position = read_size;
            }

            let chunk = &self.chunk[..self.chunk_position];
            if let Some(newline_position) = chunk.iter().rposition(|byte| *byte == b'\n') {
                self.append_reversed_chunk_fragment(newline_position + 1, self.chunk_position)?;
                self.chunk_position = newline_position;
                if let Some(outcome) = self.finish_record() {
                    return Ok(Some(outcome));
                }
            } else {
                self.append_reversed_chunk_fragment(/*start*/ 0, self.chunk_position)?;
                self.chunk_position = 0;
            }
        }
    }

    fn append_reversed_chunk_fragment(&mut self, start: usize, end: usize) -> io::Result<()> {
        let fragment_len = end.saturating_sub(start);
        if fragment_len
            > self
                .max_record_bytes
                .saturating_sub(self.record_reversed.len())
        {
            let max_record_bytes = self.max_record_bytes;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("reverse JSONL record exceeds the {max_record_bytes} byte limit"),
            ));
        }
        self.record_reversed
            .extend(self.chunk[start..end].iter().rev().copied());
        Ok(())
    }

    fn finish_record<T>(&mut self) -> Option<ScanOutcome<T>>
    where
        T: DeserializeOwned,
    {
        self.record_reversed.reverse();
        let outcome = if self.record_reversed.iter().all(u8::is_ascii_whitespace) {
            None
        } else {
            Some(match serde_json::from_slice::<T>(&self.record_reversed) {
                Ok(value) => ScanOutcome::Parsed(value),
                Err(error) => ScanOutcome::Rejected(error),
            })
        };
        self.record_reversed.clear();
        outcome
    }
}

#[cfg(test)]
#[path = "reverse_jsonl_scanner_tests.rs"]
mod tests;
