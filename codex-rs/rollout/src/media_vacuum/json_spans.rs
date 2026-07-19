use std::collections::HashMap;

const MAX_JSON_DEPTH: usize = 512;
const MAX_JSON_SPAN_NODES: usize = 100_000;

pub(super) struct JsonSpan {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) kind: JsonSpanKind,
}

pub(super) enum JsonSpanKind {
    Object(HashMap<String, JsonSpan>),
    Array(Vec<JsonSpan>),
    String,
    Scalar,
}

impl JsonSpan {
    pub(super) fn object_value(&self, key: &str) -> Option<&Self> {
        let JsonSpanKind::Object(fields) = &self.kind else {
            return None;
        };
        fields.get(key)
    }

    pub(super) fn as_array(&self) -> Option<&[Self]> {
        let JsonSpanKind::Array(items) = &self.kind else {
            return None;
        };
        Some(items)
    }

    pub(super) fn as_string(&self, json: &[u8]) -> Option<String> {
        matches!(self.kind, JsonSpanKind::String)
            .then(|| serde_json::from_slice(&json[self.start..self.end]).ok())
            .flatten()
    }

    pub(super) fn as_u64(&self, json: &[u8]) -> Option<u64> {
        matches!(self.kind, JsonSpanKind::Scalar)
            .then(|| serde_json::from_slice(&json[self.start..self.end]).ok())
            .flatten()
    }
}

#[cfg(test)]
#[path = "json_spans_tests.rs"]
mod tests;

pub(super) fn parse_json_spans(json: &[u8]) -> Result<JsonSpan, &'static str> {
    JsonSpanParser::parse(json)
}

struct JsonSpanParser<'a> {
    json: &'a [u8],
    offset: usize,
    nodes: usize,
}

impl<'a> JsonSpanParser<'a> {
    fn parse(json: &'a [u8]) -> Result<JsonSpan, &'static str> {
        let mut parser = Self {
            json,
            offset: 0,
            nodes: 0,
        };
        parser.skip_whitespace();
        let value = parser.parse_value(/*depth*/ 0)?;
        parser.skip_whitespace();
        if parser.offset != json.len() {
            return Err("unexpected trailing data");
        }
        Ok(value)
    }

    fn parse_value(&mut self, depth: usize) -> Result<JsonSpan, &'static str> {
        if depth > MAX_JSON_DEPTH {
            return Err("JSON nesting exceeds media-vacuum limit");
        }
        self.nodes = self.nodes.saturating_add(1);
        if self.nodes > MAX_JSON_SPAN_NODES {
            return Err("JSON node count exceeds media-vacuum limit");
        }
        self.skip_whitespace();
        let start = self.offset;
        let kind = match self.json.get(self.offset) {
            Some(b'{') => self.parse_object(depth)?,
            Some(b'[') => self.parse_array(depth)?,
            Some(b'"') => {
                self.parse_string_end()?;
                JsonSpanKind::String
            }
            Some(_) => {
                while self.json.get(self.offset).is_some_and(|byte| {
                    !byte.is_ascii_whitespace() && !matches!(byte, b',' | b']' | b'}')
                }) {
                    self.offset = self.offset.saturating_add(1);
                }
                JsonSpanKind::Scalar
            }
            None => return Err("expected JSON value"),
        };
        Ok(JsonSpan {
            start,
            end: self.offset,
            kind,
        })
    }

    fn parse_object(&mut self, depth: usize) -> Result<JsonSpanKind, &'static str> {
        self.offset = self.offset.saturating_add(1);
        self.skip_whitespace();
        let mut fields = HashMap::new();
        if self.consume(b'}') {
            return Ok(JsonSpanKind::Object(fields));
        }
        loop {
            let key_start = self.offset;
            self.parse_string_end()?;
            let key = serde_json::from_slice(&self.json[key_start..self.offset])
                .map_err(|_| "invalid object key")?;
            self.skip_whitespace();
            if !self.consume(b':') {
                return Err("expected colon after object key");
            }
            let value = self.parse_value(depth.saturating_add(1))?;
            if fields.insert(key, value).is_some() {
                return Err("duplicate object key");
            }
            self.skip_whitespace();
            if self.consume(b'}') {
                return Ok(JsonSpanKind::Object(fields));
            }
            if !self.consume(b',') {
                return Err("expected comma between object fields");
            }
            self.skip_whitespace();
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsonSpanKind, &'static str> {
        self.offset = self.offset.saturating_add(1);
        self.skip_whitespace();
        let mut items = Vec::new();
        if self.consume(b']') {
            return Ok(JsonSpanKind::Array(items));
        }
        loop {
            items.push(self.parse_value(depth.saturating_add(1))?);
            self.skip_whitespace();
            if self.consume(b']') {
                return Ok(JsonSpanKind::Array(items));
            }
            if !self.consume(b',') {
                return Err("expected comma between array items");
            }
        }
    }

    fn parse_string_end(&mut self) -> Result<(), &'static str> {
        if !self.consume(b'"') {
            return Err("expected JSON string");
        }
        while let Some(byte) = self.json.get(self.offset) {
            self.offset = self.offset.saturating_add(1);
            match byte {
                b'\\' => {
                    if self.json.get(self.offset).is_none() {
                        return Err("unterminated JSON escape");
                    }
                    self.offset = self.offset.saturating_add(1);
                }
                b'"' => return Ok(()),
                _ => {}
            }
        }
        Err("unterminated JSON string")
    }

    fn skip_whitespace(&mut self) {
        while self
            .json
            .get(self.offset)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.offset = self.offset.saturating_add(1);
        }
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.json.get(self.offset) != Some(&expected) {
            return false;
        }
        self.offset = self.offset.saturating_add(1);
        true
    }
}
