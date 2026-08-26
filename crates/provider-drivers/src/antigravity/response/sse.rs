use bytes::{Bytes, BytesMut};

use super::MAX_PENDING_FRAME;

pub(super) fn frame_data(frame: &[u8]) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    for line in frame.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(value) = line
            .strip_prefix(b"data: ")
            .or_else(|| line.strip_prefix(b"data:"))
        else {
            continue;
        };
        if !data.is_empty() {
            data.push(b'\n');
        }
        data.extend_from_slice(value);
    }
    (!data.is_empty()).then_some(data)
}

#[derive(Default)]
pub(super) struct SseDecoder {
    buffer: BytesMut,
}

impl SseDecoder {
    pub(super) fn push(&mut self, chunk: &[u8]) -> Result<Vec<Bytes>, ()> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(end) = frame_end(&self.buffer) {
            let frame = self.buffer.split_to(end);
            frames.push(frame.freeze());
        }
        if self.buffer.len() > MAX_PENDING_FRAME {
            self.buffer.clear();
            return Err(());
        }
        Ok(frames)
    }

    pub(super) fn finish(&mut self) -> Option<Bytes> {
        (!self.buffer.is_empty()).then(|| self.buffer.split().freeze())
    }
}

fn frame_end(buffer: &[u8]) -> Option<usize> {
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(lf), Some(crlf)) => Some(lf.min(crlf) + 2),
        (Some(end), None) => Some(end + 2),
        (None, Some(end)) => Some(end + 4),
        (None, None) => None,
    }
}
