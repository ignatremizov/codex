use std::collections::BTreeMap;

/// Orders completed transcription results without altering their text.
#[derive(Debug, Default)]
pub(crate) struct OrderedTranscript {
    pending: BTreeMap<u64, Result<String, String>>,
    next_sequence: u64,
    exhausted: bool,
}

/// One contiguous transcription result released to the session coordinator.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ResolvedChunk {
    pub(crate) sequence: u64,
    pub(crate) result: Result<String, String>,
}

impl OrderedTranscript {
    /// Stores one result and releases every newly contiguous result starting at sequence zero.
    pub(crate) fn resolve(
        &mut self,
        sequence: u64,
        result: Result<String, String>,
    ) -> Vec<ResolvedChunk> {
        if self.exhausted || sequence < self.next_sequence || self.pending.contains_key(&sequence) {
            return Vec::new();
        }
        self.pending.insert(sequence, result);
        let mut resolved = Vec::new();
        while let Some(result) = self.pending.remove(&self.next_sequence) {
            let sequence = self.next_sequence;
            if self.next_sequence == u64::MAX {
                self.exhausted = true;
            } else {
                self.next_sequence += 1;
            }
            resolved.push(ResolvedChunk { sequence, result });
        }
        resolved
    }
}

#[cfg(test)]
#[path = "ordered_tests.rs"]
mod tests;
