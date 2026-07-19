pub(super) struct JsonSpan {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) kind: JsonSpanKind,
}

pub(super) enum JsonSpanKind {
    Object(Vec<(String, JsonSpan)>),
    Array(Vec<JsonSpan>),
    String,
    Scalar,
}

impl JsonSpan {
    pub(super) fn object_value(&self, key: &str) -> Option<&Self> {
        let JsonSpanKind::Object(fields) = &self.kind else {
            return None;
        };
        fields
            .iter()
            .find_map(|(field, value)| (field == key).then_some(value))
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

pub(super) fn parse_json_spans(json: &[u8]) -> Result<JsonSpan, &'static str> {
    JsonSpanParser::parse(json)
}

struct JsonSpanParser<'a> {
    json: &'a [u8],
    offset: usize,
}

impl<'a> JsonSpanParser<'a> {
    fn parse(json: &'a [u8]) -> Result<JsonSpan, &'static str> {
        let mut parser = Self { json, offset: 0 };
        parser.skip_whitespace();
        let value = parser.parse_value()?;
        parser.skip_whitespace();
        if parser.offset != json.len() {
            return Err("unexpected trailing data");
        }
        Ok(value)
    }

    fn parse_value(&mut self) -> Result<JsonSpan, &'static str> {
        self.skip_whitespace();
        let start = self.offset;
        let kind = match self.json.get(self.offset) {
            Some(b'{') => self.parse_object()?,
            Some(b'[') => self.parse_array()?,
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

    fn parse_object(&mut self) -> Result<JsonSpanKind, &'static str> {
        self.offset = self.offset.saturating_add(1);
        self.skip_whitespace();
        let mut fields = Vec::new();
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
            fields.push((key, self.parse_value()?));
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

    fn parse_array(&mut self) -> Result<JsonSpanKind, &'static str> {
        self.offset = self.offset.saturating_add(1);
        self.skip_whitespace();
        let mut items = Vec::new();
        if self.consume(b']') {
            return Ok(JsonSpanKind::Array(items));
        }
        loop {
            items.push(self.parse_value()?);
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
