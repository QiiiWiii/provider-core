use std::ops::Range;

use provider_core::ProviderError;
use xxhash_rust::xxh64::xxh64;

use super::json::{array_item, object_member, skip_ws, splice};
use super::{internal, invalid};

const CCH_SEED: u64 = 0x4D659218E32A3268;

pub(super) fn ensure_and_sign_cch(
    mut body: Vec<u8>,
    fallback_billing: Option<&str>,
) -> Result<Vec<u8>, ProviderError> {
    let system = match object_member(&body, 0, "system") {
        Some(system) => system,
        None => {
            let Some(fallback_billing) = fallback_billing else {
                return Ok(body);
            };
            body = append_system(&body, fallback_billing)?;
            object_member(&body, 0, "system").ok_or_else(|| internal("system vanished"))?
        }
    };
    if !first_system_block_is_billing(&body, &system) {
        let fallback_billing =
            fallback_billing.ok_or_else(|| invalid("Claude OAuth billing header is missing"))?;
        body = prepend_billing(&body, system, fallback_billing)?;
    }
    let system = object_member(&body, 0, "system").ok_or_else(|| internal("system vanished"))?;
    let first = array_item(&body, system.start, 0)
        .ok_or_else(|| invalid("Claude OAuth system profile is invalid"))?;
    let text = object_member(&body, first.start, "text")
        .ok_or_else(|| invalid("Claude OAuth billing header is missing"))?;
    let mut billing: String = serde_json::from_slice(&body[text.clone()])
        .map_err(|_| invalid("Claude OAuth billing header is invalid"))?;
    if !billing.starts_with("x-anthropic-billing-header:") {
        return Err(invalid("Claude OAuth billing header is missing"));
    }
    if !billing.contains(" cch=") {
        let entrypoint = billing
            .find("cc_entrypoint=")
            .and_then(|start| billing[start..].find(';').map(|end| start + end + 1))
            .ok_or_else(|| invalid("Claude OAuth billing entrypoint is invalid"))?;
        billing.insert_str(entrypoint, " cch=00000;");
        let encoded = serde_json::to_vec(&billing)
            .map_err(|_| internal("failed to encode Claude OAuth billing header"))?;
        body = splice(&body, text, &encoded);
    }
    let system = object_member(&body, 0, "system").ok_or_else(|| internal("system vanished"))?;
    let first = array_item(&body, system.start, 0).ok_or_else(|| internal("system vanished"))?;
    let text =
        object_member(&body, first.start, "text").ok_or_else(|| internal("billing vanished"))?;
    let relative = body[text.clone()]
        .windows(4)
        .position(|window| window == b"cch=")
        .ok_or_else(|| invalid("Claude OAuth billing CCH is missing"))?;
    let digits = text.start + relative + 4;
    if body.get(digits + 5) != Some(&b';') {
        return Err(invalid("Claude OAuth billing CCH is invalid"));
    }
    body[digits..digits + 5].copy_from_slice(b"00000");
    let normalized = normalize_cch(&body)?;
    let signature = format!("{:05x}", xxh64(&normalized, CCH_SEED) & 0xFFFFF);
    body[digits..digits + 5].copy_from_slice(signature.as_bytes());
    Ok(body)
}

fn first_system_block_is_billing(body: &[u8], system: &Range<usize>) -> bool {
    array_item(body, system.start, 0)
        .and_then(|first| object_member(body, first.start, "text"))
        .and_then(|text| serde_json::from_slice::<String>(&body[text]).ok())
        .is_some_and(|text| text.starts_with("x-anthropic-billing-header:"))
}

fn billing_block(billing: &str) -> Result<Vec<u8>, ProviderError> {
    serde_json::to_vec(&serde_json::json!({ "type": "text", "text": billing }))
        .map_err(|_| internal("failed to encode Claude OAuth billing block"))
}

fn append_system(body: &[u8], billing: &str) -> Result<Vec<u8>, ProviderError> {
    let end = body
        .iter()
        .rposition(|byte| *byte == b'}')
        .ok_or_else(|| invalid("Claude OAuth request body is invalid"))?;
    let prefix = body[..end].iter().rfind(|byte| !byte.is_ascii_whitespace());
    let separator = if prefix == Some(&b'{') {
        b"".as_slice()
    } else {
        b",".as_slice()
    };
    let block = billing_block(billing)?;
    let mut member = Vec::with_capacity(block.len() + 12);
    member.extend_from_slice(separator);
    member.extend_from_slice(b"\"system\":[");
    member.extend_from_slice(&block);
    member.push(b']');
    Ok(splice(body, end..end, &member))
}

fn prepend_billing(
    body: &[u8],
    system: Range<usize>,
    billing: &str,
) -> Result<Vec<u8>, ProviderError> {
    let block = billing_block(billing)?;
    match body.get(skip_ws(body, system.start)) {
        Some(b'[') => {
            let insert_at = skip_ws(body, system.start) + 1;
            let empty = body.get(skip_ws(body, insert_at)) == Some(&b']');
            let mut prefix = block;
            if !empty {
                prefix.push(b',');
            }
            Ok(splice(body, insert_at..insert_at, &prefix))
        }
        Some(b'"') => {
            let text: String = serde_json::from_slice(&body[system.clone()])
                .map_err(|_| invalid("Claude OAuth system text is invalid"))?;
            let original = billing_block(&text)?;
            let mut array = Vec::with_capacity(block.len() + original.len() + 3);
            array.push(b'[');
            array.extend_from_slice(&block);
            array.push(b',');
            array.extend_from_slice(&original);
            array.push(b']');
            Ok(splice(body, system, &array))
        }
        _ => Err(invalid("Claude OAuth system profile is invalid")),
    }
}

#[derive(Clone)]
struct Member {
    start: usize,
    end: usize,
    comma_before: Option<usize>,
    comma_after: Option<usize>,
    excluded: bool,
}

struct Scanner<'a> {
    body: &'a [u8],
    pos: usize,
    edits: Vec<Range<usize>>,
}

fn normalize_cch(body: &[u8]) -> Result<Vec<u8>, ProviderError> {
    let mut scanner = Scanner {
        body,
        pos: 0,
        edits: Vec::new(),
    };
    scanner.parse_value(true)?;
    scanner.skip_whitespace();
    if scanner.pos != body.len() {
        return Err(invalid("Claude OAuth body has trailing JSON"));
    }
    scanner.edits.sort_by_key(|edit| edit.start);
    let mut normalized = Vec::with_capacity(body.len());
    let mut last = 0;
    for edit in scanner.edits {
        if edit.start < last || edit.end > body.len() {
            return Err(internal("Claude OAuth CCH edits overlap"));
        }
        normalized.extend_from_slice(&body[last..edit.start]);
        last = edit.end;
    }
    normalized.extend_from_slice(&body[last..]);
    Ok(normalized)
}

impl Scanner<'_> {
    fn parse_value(&mut self, collect: bool) -> Result<(), ProviderError> {
        self.skip_whitespace();
        match self.body.get(self.pos) {
            Some(b'{') => self.parse_object(collect),
            Some(b'[') => self.parse_array(collect),
            Some(b'"') => self.parse_string().map(|_| ()),
            Some(_) => {
                let start = self.pos;
                while self.body.get(self.pos).is_some_and(|byte| {
                    !matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n')
                }) {
                    self.pos += 1;
                }
                if self.pos == start {
                    return Err(invalid("Claude OAuth JSON value is invalid"));
                }
                Ok(())
            }
            None => Err(invalid("Claude OAuth JSON value is missing")),
        }
    }

    fn parse_object(&mut self, collect: bool) -> Result<(), ProviderError> {
        self.pos += 1;
        self.skip_whitespace();
        if self.consume(b'}') {
            return Ok(());
        }
        let mut members = Vec::new();
        let mut comma_before = None;
        loop {
            self.skip_whitespace();
            let member_start = self.pos;
            let key = self.parse_string()?;
            self.skip_whitespace();
            if !self.consume(b':') {
                return Err(invalid("Claude OAuth JSON object is invalid"));
            }
            self.skip_whitespace();
            let excluded = collect
                && matches!(
                    &self.body[key.clone()],
                    b"\"max_tokens\"" | b"\"fallbacks\"" | b"\"fallback_credit_token\""
                );
            if collect
                && &self.body[key.clone()] == b"\"model\""
                && self.body.get(self.pos) == Some(&b'"')
            {
                let value = self.parse_string()?;
                self.edits.push(value.start + 1..value.end - 1);
            } else {
                self.parse_value(collect && !excluded)?;
            }
            let member_end = self.pos;
            self.skip_whitespace();
            let comma_after = self.consume(b',').then_some(self.pos - 1);
            members.push(Member {
                start: member_start,
                end: member_end,
                comma_before,
                comma_after,
                excluded,
            });
            if let Some(comma) = comma_after {
                comma_before = Some(comma);
                continue;
            }
            if !self.consume(b'}') {
                return Err(invalid("Claude OAuth JSON object is invalid"));
            }
            break;
        }
        if collect {
            self.exclude_members(&members);
        }
        Ok(())
    }

    fn parse_array(&mut self, collect: bool) -> Result<(), ProviderError> {
        self.pos += 1;
        self.skip_whitespace();
        if self.consume(b']') {
            return Ok(());
        }
        loop {
            self.parse_value(collect)?;
            self.skip_whitespace();
            if self.consume(b',') {
                continue;
            }
            if !self.consume(b']') {
                return Err(invalid("Claude OAuth JSON array is invalid"));
            }
            return Ok(());
        }
    }

    fn parse_string(&mut self) -> Result<Range<usize>, ProviderError> {
        if self.body.get(self.pos) != Some(&b'"') {
            return Err(invalid("Claude OAuth JSON string is invalid"));
        }
        let start = self.pos;
        self.pos += 1;
        while let Some(byte) = self.body.get(self.pos) {
            match byte {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    return Ok(start..self.pos);
                }
                _ => self.pos += 1,
            }
        }
        Err(invalid("Claude OAuth JSON string is unterminated"))
    }

    fn exclude_members(&mut self, members: &[Member]) {
        let mut start = 0;
        while start < members.len() {
            if !members[start].excluded {
                start += 1;
                continue;
            }
            let mut end = start;
            while end + 1 < members.len() && members[end + 1].excluded {
                end += 1;
            }
            let range = if end + 1 < members.len() {
                members[start].start..members[end].comma_after.unwrap_or(members[end].end) + 1
            } else if start > 0 && end > start {
                members[start].start..members[end].end
            } else if start > 0 {
                members[start].comma_before.unwrap_or(members[start].start)..members[end].end
            } else {
                members[start].start..members[end].end
            };
            self.edits.push(range);
            start = end + 1;
        }
    }

    fn skip_whitespace(&mut self) {
        while self
            .body
            .get(self.pos)
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.pos += 1;
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.body.get(self.pos) != Some(&byte) {
            return false;
        }
        self.pos += 1;
        true
    }
}
