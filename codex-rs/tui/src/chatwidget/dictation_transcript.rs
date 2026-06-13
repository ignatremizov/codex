use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

#[derive(Default)]
pub(super) struct OrderedDictationTranscript {
    text: String,
    pending: BTreeMap<u64, ChunkTranscriptResult>,
    next_sequence: u64,
    final_sequence: Option<u64>,
    flushed: bool,
}

enum ChunkTranscriptResult {
    Complete(String),
    Failed,
}

impl OrderedDictationTranscript {
    pub(super) fn complete(&mut self, sequence: u64, text: String) {
        self.insert(sequence, ChunkTranscriptResult::Complete(text));
    }

    pub(super) fn fail(&mut self, sequence: u64) {
        self.insert(sequence, ChunkTranscriptResult::Failed);
    }

    pub(super) fn set_final_sequence(&mut self, final_sequence: Option<u64>) {
        self.flushed = true;
        self.final_sequence = final_sequence;
    }

    pub(super) fn is_finished(&self) -> bool {
        if !self.flushed {
            return false;
        }
        match self.final_sequence {
            Some(final_sequence) => self.next_sequence > final_sequence,
            None => true,
        }
    }

    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(super) fn render_with_meter(&self, meter_text: &str) -> String {
        if self.text.is_empty() {
            meter_text.to_string()
        } else {
            format!("{} {meter_text}", self.text)
        }
    }

    fn insert(&mut self, sequence: u64, result: ChunkTranscriptResult) {
        if sequence < self.next_sequence {
            return;
        }
        match self.pending.entry(sequence) {
            Entry::Vacant(entry) => {
                entry.insert(result);
            }
            Entry::Occupied(_) => return,
        }
        self.drain_ready();
    }

    fn drain_ready(&mut self) {
        while let Some(result) = self.pending.remove(&self.next_sequence) {
            match result {
                ChunkTranscriptResult::Complete(text) => self.append_text(&text),
                ChunkTranscriptResult::Failed => {}
            }
            self.next_sequence = self.next_sequence.saturating_add(/*rhs*/ 1);
        }
    }

    fn append_text(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let needs_separator = !self.text.is_empty()
            && !self
                .text
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        if needs_separator {
            self.text.push(' ');
        }
        self.text.push_str(text);
    }
}

#[cfg(test)]
#[path = "dictation_transcript_tests.rs"]
mod tests;
