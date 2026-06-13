//! A pending recording owns one numeric element, never a text-matched placeholder.
//! Ordered text is inserted before it as ordinary editable text. Finishing removes only
//! that element; neither transcripts nor cancellation submit or clear the user's draft.

use super::ChatComposer;

impl ChatComposer {
    pub(crate) fn begin_dictation(&mut self) -> u64 {
        self.dismiss_sparkle();
        if let Some(pasted) = self.draft.paste_burst.flush_before_modified_input() {
            self.apply_paste(pasted);
        }
        let id = self.draft.textarea.insert_element("[Dictation: preparing]");
        self.dictation_element = Some(id);
        id
    }

    pub(crate) fn has_dictation_element(&self, id: u64) -> bool {
        self.dictation_element == Some(id)
            && self
                .draft
                .textarea
                .text_element_snapshots()
                .iter()
                .any(|item| item.id == id)
    }

    pub(crate) fn update_dictation(&mut self, id: u64, text: &str, label: &str) -> bool {
        if !self.has_dictation_element(id) {
            return false;
        }
        let Some(element) = self
            .draft
            .textarea
            .text_element_snapshots()
            .into_iter()
            .find(|item| item.id == id)
        else {
            return false;
        };
        if !text.is_empty() {
            self.draft.textarea.insert_str_at(element.range.start, text);
        }
        self.draft.textarea.replace_element_id(id, label)
    }

    pub(crate) fn finish_dictation(&mut self, id: u64) {
        if self.dictation_element != Some(id) {
            return;
        }
        if let Some(element) = self
            .draft
            .textarea
            .text_element_snapshots()
            .into_iter()
            .find(|item| item.id == id)
        {
            self.draft.textarea.replace_range(element.range, "");
        }
        self.dictation_element = None;
    }
}

#[cfg(test)]
#[path = "dictation_tests.rs"]
mod tests;
