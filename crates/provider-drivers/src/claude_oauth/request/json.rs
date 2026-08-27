use std::ops::Range;

pub(super) fn object_member(
    body: &[u8],
    object_start: usize,
    wanted: &str,
) -> Option<Range<usize>> {
    let mut pos = skip_ws(body, object_start);
    if body.get(pos) != Some(&b'{') {
        return None;
    }
    pos += 1;
    loop {
        pos = skip_ws(body, pos);
        if body.get(pos) == Some(&b'}') {
            return None;
        }
        let key = string_range(body, pos)?;
        let decoded: String = serde_json::from_slice(&body[key.clone()]).ok()?;
        pos = skip_ws(body, key.end);
        if body.get(pos) != Some(&b':') {
            return None;
        }
        let value_start = skip_ws(body, pos + 1);
        let value_end = skip_value(body, value_start)?;
        if decoded == wanted {
            return Some(value_start..value_end);
        }
        pos = skip_ws(body, value_end);
        match body.get(pos) {
            Some(b',') => pos += 1,
            Some(b'}') => return None,
            _ => return None,
        }
    }
}

pub(super) fn unique_object_member(
    body: &[u8],
    object_start: usize,
    wanted: &str,
) -> Option<Range<usize>> {
    let mut pos = skip_ws(body, object_start);
    if body.get(pos) != Some(&b'{') {
        return None;
    }
    pos += 1;
    let mut found = None;
    loop {
        pos = skip_ws(body, pos);
        if body.get(pos) == Some(&b'}') {
            return found;
        }
        let key = string_range(body, pos)?;
        let decoded: String = serde_json::from_slice(&body[key.clone()]).ok()?;
        pos = skip_ws(body, key.end);
        if body.get(pos) != Some(&b':') {
            return None;
        }
        let value_start = skip_ws(body, pos + 1);
        let value_end = skip_value(body, value_start)?;
        if decoded == wanted {
            if found.is_some() {
                return None;
            }
            found = Some(value_start..value_end);
        }
        pos = skip_ws(body, value_end);
        match body.get(pos) {
            Some(b',') => pos += 1,
            Some(b'}') => return found,
            _ => return None,
        }
    }
}

pub(super) fn array_item(body: &[u8], array_start: usize, wanted: usize) -> Option<Range<usize>> {
    let mut pos = skip_ws(body, array_start);
    if body.get(pos) != Some(&b'[') {
        return None;
    }
    pos += 1;
    for index in 0..=wanted {
        let start = skip_ws(body, pos);
        let end = skip_value(body, start)?;
        if index == wanted {
            return Some(start..end);
        }
        pos = skip_ws(body, end);
        if body.get(pos) != Some(&b',') {
            return None;
        }
        pos += 1;
    }
    None
}

pub(super) fn skip_value(body: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    match body.get(pos)? {
        b'"' => string_range(body, pos).map(|range| range.end),
        b'{' | b'[' => {
            let open = body[pos];
            let close = if open == b'{' { b'}' } else { b']' };
            let mut depth = 0_usize;
            while pos < body.len() {
                match body[pos] {
                    b'"' => pos = string_range(body, pos)?.end,
                    byte if byte == open => {
                        depth += 1;
                        pos += 1;
                    }
                    byte if byte == close => {
                        depth = depth.checked_sub(1)?;
                        pos += 1;
                        if depth == 0 {
                            return Some(pos);
                        }
                    }
                    _ => pos += 1,
                }
            }
            None
        }
        _ => {
            while body.get(pos).is_some_and(|byte| {
                !matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n')
            }) {
                pos += 1;
            }
            (pos > start).then_some(pos)
        }
    }
}

pub(super) fn string_range(body: &[u8], start: usize) -> Option<Range<usize>> {
    if body.get(start) != Some(&b'"') {
        return None;
    }
    let mut pos = start + 1;
    while let Some(byte) = body.get(pos) {
        match byte {
            b'\\' => pos += 2,
            b'"' => return Some(start..pos + 1),
            _ => pos += 1,
        }
    }
    None
}

pub(super) fn skip_ws(body: &[u8], mut pos: usize) -> usize {
    while body
        .get(pos)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    {
        pos += 1;
    }
    pos
}

pub(super) fn splice(body: &[u8], range: Range<usize>, value: &[u8]) -> Vec<u8> {
    let mut updated = Vec::with_capacity(body.len() - range.len() + value.len());
    updated.extend_from_slice(&body[..range.start]);
    updated.extend_from_slice(value);
    updated.extend_from_slice(&body[range.end..]);
    updated
}
