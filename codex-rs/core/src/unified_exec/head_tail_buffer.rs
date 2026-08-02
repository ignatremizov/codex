use crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use crate::unified_exec::format_output_omission_marker;
use std::collections::VecDeque;

// Exact replay-gap boundaries are useful only while their surrounding output remains retained.
// Keep their memory and rendering cost bounded independently of the output byte cap.
const MAX_UPSTREAM_OMISSION_BOUNDARIES: usize = 1024;
const MAX_UPSTREAM_OMISSION_BOUNDARIES_PER_REGION: usize = MAX_UPSTREAM_OMISSION_BOUNDARIES / 2;

#[derive(Debug)]
#[cfg_attr(test, derive(Eq, PartialEq))]
struct UpstreamOmission {
    output_bytes_before: usize,
    omitted_bytes: usize,
}

/// A capped buffer that preserves a stable prefix ("head") and suffix ("tail"),
/// dropping the middle once it exceeds the configured maximum. The buffer is
/// symmetric meaning 50% of the capacity is allocated to the head and 50% is
/// allocated to the tail.
#[derive(Debug)]
#[cfg_attr(test, derive(Eq, PartialEq))]
pub(crate) struct HeadTailBuffer {
    max_bytes: usize,
    head_budget: usize,
    tail_budget: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    truncated_bytes: usize,
    total_output_bytes: usize,
    middle_upstream_omitted_bytes: usize,
    unpositioned_upstream_omitted_bytes: usize,
    upstream_omissions: Vec<UpstreamOmission>,
}

impl Default for HeadTailBuffer {
    fn default() -> Self {
        Self::new(UNIFIED_EXEC_OUTPUT_MAX_BYTES)
    }
}

impl HeadTailBuffer {
    /// Create a new buffer that retains at most `max_bytes` of output.
    ///
    /// The retained output is split across a prefix ("head") and suffix ("tail")
    /// budget, dropping bytes from the middle once the limit is exceeded.
    pub(crate) fn new(max_bytes: usize) -> Self {
        let head_budget = max_bytes / 2;
        let tail_budget = max_bytes.saturating_sub(head_budget);
        Self {
            max_bytes,
            head_budget,
            tail_budget,
            head: Vec::new(),
            tail: VecDeque::new(),
            truncated_bytes: 0,
            total_output_bytes: 0,
            middle_upstream_omitted_bytes: 0,
            unpositioned_upstream_omitted_bytes: 0,
            upstream_omissions: Vec::new(),
        }
    }

    // Used for tests.
    #[allow(dead_code)]
    /// Total bytes currently retained by the buffer (head + tail).
    pub(crate) fn retained_bytes(&self) -> usize {
        self.head.len().saturating_add(self.tail.len())
    }

    // Used for tests.
    #[allow(dead_code)]
    /// Total bytes omitted by the size cap or an upstream replay gap.
    pub(crate) fn omitted_bytes(&self) -> usize {
        self.truncated_bytes
            .saturating_add(self.upstream_omitted_bytes())
    }

    /// Total bytes observed by the buffer, including bytes omitted by the cap.
    pub(crate) fn total_bytes(&self) -> usize {
        self.total_output_bytes
            .saturating_add(self.upstream_omitted_bytes())
    }

    /// Append a chunk of bytes to the buffer.
    ///
    /// Bytes are first added to the head until the head budget is full; any
    /// remaining bytes are added to the tail, with older tail bytes being
    /// dropped to preserve the tail budget.
    pub(crate) fn push_chunk(&mut self, chunk: Vec<u8>) {
        if chunk.is_empty() {
            return;
        }
        self.total_output_bytes = self.total_output_bytes.saturating_add(chunk.len());
        if self.max_bytes == 0 {
            self.truncated_bytes = self.truncated_bytes.saturating_add(chunk.len());
            self.compact_middle_upstream_omissions();
            return;
        }

        // Fill the head budget first, then keep a capped tail.
        let remaining_head = self.head_budget.saturating_sub(self.head.len());
        let head_len = remaining_head.min(chunk.len());
        if head_len > 0 {
            self.head.extend_from_slice(&chunk[..head_len]);
        }
        self.push_to_tail(&chunk[head_len..]);
        self.compact_middle_upstream_omissions();
    }

    /// Record an upstream replay gap at the current append boundary.
    pub(crate) fn push_upstream_omission(&mut self, omitted_bytes: usize) {
        if omitted_bytes == 0 {
            return;
        }
        if let Some(last) = self.upstream_omissions.last_mut()
            && last.output_bytes_before == self.total_output_bytes
        {
            last.omitted_bytes = last.omitted_bytes.saturating_add(omitted_bytes);
            return;
        }
        self.upstream_omissions.push(UpstreamOmission {
            output_bytes_before: self.total_output_bytes,
            omitted_bytes,
        });
        self.compact_upstream_omissions();
    }

    /// Snapshot the retained output as a list of chunks.
    ///
    /// The returned chunks are ordered as: head chunks first, then tail chunks.
    /// Omitted bytes are not represented in the snapshot.
    pub(crate) fn snapshot_chunks(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(2);
        if !self.head.is_empty() {
            out.push(self.head.clone());
        }
        if !self.tail.is_empty() {
            out.push(self.tail.iter().copied().collect());
        }
        out
    }

    /// Return the retained output as a single byte vector.
    ///
    /// The output is formed by concatenating head chunks, then tail chunks.
    /// Omitted bytes are not represented in the returned value.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.retained_bytes());
        out.extend_from_slice(&self.head);
        out.extend(self.tail.iter().copied());
        out
    }

    /// Return retained output with an explicit marker for bytes truncated by
    /// this buffer. Upstream gap markers are inserted at their append boundary.
    pub(crate) fn to_bytes_with_omission_marker(&self) -> Vec<u8> {
        if self.truncated_bytes == 0
            && self.middle_upstream_omitted_bytes == 0
            && self.unpositioned_upstream_omitted_bytes == 0
            && self.upstream_omissions.is_empty()
        {
            return self.to_bytes();
        }

        let tail = self.tail.iter().copied().collect::<Vec<_>>();
        let tail_start = self.total_output_bytes.saturating_sub(tail.len());
        let head_end = self.head.len();
        let mut head_omissions = Vec::new();
        let mut tail_omissions = Vec::new();
        let mut middle_omitted_bytes = self
            .truncated_bytes
            .saturating_add(self.middle_upstream_omitted_bytes);

        for omission in &self.upstream_omissions {
            if omission.output_bytes_before <= head_end {
                head_omissions.push((omission.output_bytes_before, omission.omitted_bytes));
            } else if self.truncated_bytes > 0 && omission.output_bytes_before < tail_start {
                middle_omitted_bytes = middle_omitted_bytes.saturating_add(omission.omitted_bytes);
            } else {
                tail_omissions.push((
                    omission.output_bytes_before.saturating_sub(tail_start),
                    omission.omitted_bytes,
                ));
            }
        }

        let mut out = Vec::with_capacity(self.retained_bytes());
        if self.unpositioned_upstream_omitted_bytes > 0 {
            append_unpositioned_omission_notice(&mut out, self.unpositioned_upstream_omitted_bytes);
        }
        append_bytes_with_omissions(&mut out, &self.head, &head_omissions);
        if middle_omitted_bytes > 0 {
            append_omission_marker(&mut out, middle_omitted_bytes);
        }
        append_bytes_with_omissions(&mut out, &tail, &tail_omissions);
        out
    }

    /// Drain the retained output and omission metadata, resetting this buffer's
    /// contents while preserving its configured capacity.
    pub(crate) fn drain(&mut self) -> Self {
        Self {
            max_bytes: self.max_bytes,
            head_budget: self.head_budget,
            tail_budget: self.tail_budget,
            head: std::mem::take(&mut self.head),
            tail: std::mem::take(&mut self.tail),
            truncated_bytes: std::mem::take(&mut self.truncated_bytes),
            total_output_bytes: std::mem::take(&mut self.total_output_bytes),
            middle_upstream_omitted_bytes: std::mem::take(&mut self.middle_upstream_omitted_bytes),
            unpositioned_upstream_omitted_bytes: std::mem::take(
                &mut self.unpositioned_upstream_omitted_bytes,
            ),
            upstream_omissions: std::mem::take(&mut self.upstream_omissions),
        }
    }

    /// Append retained output from another buffer and preserve any omissions it
    /// already recorded.
    pub(crate) fn push_buffer(&mut self, mut buffer: Self) {
        let output_offset = self.total_output_bytes;
        let buffer_truncated_bytes = buffer.truncated_bytes;
        let buffer_total_output_bytes = buffer.total_output_bytes;
        self.push_chunk(std::mem::take(&mut buffer.head));
        self.push_chunk(buffer.tail.drain(..).collect());
        self.truncated_bytes = self.truncated_bytes.saturating_add(buffer_truncated_bytes);
        self.total_output_bytes = output_offset.saturating_add(buffer_total_output_bytes);
        self.middle_upstream_omitted_bytes = self
            .middle_upstream_omitted_bytes
            .saturating_add(buffer.middle_upstream_omitted_bytes);
        self.unpositioned_upstream_omitted_bytes = self
            .unpositioned_upstream_omitted_bytes
            .saturating_add(buffer.unpositioned_upstream_omitted_bytes);
        for omission in buffer.upstream_omissions.drain(..) {
            let output_bytes_before = output_offset.saturating_add(omission.output_bytes_before);
            if let Some(last) = self.upstream_omissions.last_mut()
                && last.output_bytes_before == output_bytes_before
            {
                last.omitted_bytes = last.omitted_bytes.saturating_add(omission.omitted_bytes);
            } else {
                self.upstream_omissions.push(UpstreamOmission {
                    output_bytes_before,
                    omitted_bytes: omission.omitted_bytes,
                });
            }
        }
        self.compact_upstream_omissions();
    }

    fn push_to_tail(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        if self.tail_budget == 0 {
            self.truncated_bytes = self.truncated_bytes.saturating_add(chunk.len());
            return;
        }

        if chunk.len() >= self.tail_budget {
            // This single chunk is larger than the whole tail budget. Keep only the last
            // tail_budget bytes and drop everything else.
            let start = chunk.len().saturating_sub(self.tail_budget);
            let kept = &chunk[start..];
            let dropped = chunk.len().saturating_sub(kept.len());
            self.truncated_bytes = self
                .truncated_bytes
                .saturating_add(self.tail.len())
                .saturating_add(dropped);
            self.tail.clear();
            self.tail.extend(kept);
            return;
        }

        self.tail.extend(chunk);
        self.trim_tail_to_budget();
    }

    fn trim_tail_to_budget(&mut self) {
        let excess = self.tail.len().saturating_sub(self.tail_budget);
        if excess > 0 {
            drop(self.tail.drain(..excess));
            self.truncated_bytes = self.truncated_bytes.saturating_add(excess);
        }
    }

    fn upstream_omitted_bytes(&self) -> usize {
        self.upstream_omissions.iter().fold(
            self.middle_upstream_omitted_bytes
                .saturating_add(self.unpositioned_upstream_omitted_bytes),
            |total, omission| total.saturating_add(omission.omitted_bytes),
        )
    }

    fn compact_upstream_omissions(&mut self) {
        if self.upstream_omissions.is_empty() {
            return;
        }

        self.compact_middle_upstream_omissions();
        let head_end = self.head.len();
        let head_omission_count = self
            .upstream_omissions
            .partition_point(|omission| omission.output_bytes_before <= head_end);
        let unpositioned_head_bytes = discard_excess_retained_omissions(
            &mut self.upstream_omissions,
            /*start*/ 0,
            head_omission_count,
            MAX_UPSTREAM_OMISSION_BOUNDARIES_PER_REGION,
        );
        self.unpositioned_upstream_omitted_bytes = self
            .unpositioned_upstream_omitted_bytes
            .saturating_add(unpositioned_head_bytes);

        let head_omission_count = self
            .upstream_omissions
            .partition_point(|omission| omission.output_bytes_before <= head_end);
        let omission_count = self.upstream_omissions.len();
        let unpositioned_tail_bytes = discard_excess_retained_omissions(
            &mut self.upstream_omissions,
            head_omission_count,
            omission_count,
            MAX_UPSTREAM_OMISSION_BOUNDARIES_PER_REGION,
        );
        self.unpositioned_upstream_omitted_bytes = self
            .unpositioned_upstream_omitted_bytes
            .saturating_add(unpositioned_tail_bytes);
    }

    fn compact_middle_upstream_omissions(&mut self) {
        if self.truncated_bytes == 0 || self.upstream_omissions.is_empty() {
            return;
        }

        let head_end = self.head.len();
        let tail_start = self.total_output_bytes.saturating_sub(self.tail.len());
        let middle_start = self
            .upstream_omissions
            .partition_point(|omission| omission.output_bytes_before <= head_end);
        let middle_end = self
            .upstream_omissions
            .partition_point(|omission| omission.output_bytes_before < tail_start);
        if middle_start == middle_end {
            return;
        }

        let newly_compacted_bytes = self
            .upstream_omissions
            .drain(middle_start..middle_end)
            .fold(0usize, |total, omission| {
                total.saturating_add(omission.omitted_bytes)
            });
        self.middle_upstream_omitted_bytes = self
            .middle_upstream_omitted_bytes
            .saturating_add(newly_compacted_bytes);
    }
}

fn discard_excess_retained_omissions(
    omissions: &mut Vec<UpstreamOmission>,
    start: usize,
    end: usize,
    max_boundaries: usize,
) -> usize {
    let region_len = end.saturating_sub(start);
    if region_len <= max_boundaries {
        return 0;
    }

    let discard_end = start.saturating_add(region_len.saturating_sub(max_boundaries));
    omissions
        .drain(start..discard_end)
        .fold(0usize, |total, omission| {
            total.saturating_add(omission.omitted_bytes)
        })
}

fn append_bytes_with_omissions(output: &mut Vec<u8>, bytes: &[u8], omissions: &[(usize, usize)]) {
    let mut copied = 0;
    for (position, omitted_bytes) in omissions {
        let position = (*position).min(bytes.len());
        output.extend_from_slice(&bytes[copied..position]);
        append_omission_marker(output, *omitted_bytes);
        copied = position;
    }
    output.extend_from_slice(&bytes[copied..]);
}

fn append_omission_marker(output: &mut Vec<u8>, omitted_bytes: usize) {
    output.push(b'\n');
    output.extend_from_slice(format_output_omission_marker(omitted_bytes).as_bytes());
    output.push(b'\n');
}

fn append_unpositioned_omission_notice(output: &mut Vec<u8>, omitted_bytes: usize) {
    output.extend_from_slice(
        format!(
            "Warning: {omitted_bytes} bytes were omitted at multiple locations in retained process output.\n"
        )
        .as_bytes(),
    );
}

#[cfg(test)]
#[path = "head_tail_buffer_tests.rs"]
mod tests;
