//! omp RPC protocol v2 framing (harness.md "Framing (protocol v2)"): one JSON object per stdout
//! line; oversized objects arrive as an uninterrupted `rpc_chunk` sequence that is validated,
//! reassembled, strictly UTF-8-decoded, and parsed as one object.

use base64::Engine as _;
use serde_json::{Map, Value};

/// Hard ceiling for a reassembled frame regardless of what `ready` advertises.
pub const REASSEMBLY_CEILING: usize = 64 * 1024 * 1024;

#[derive(Debug)]
struct Chunking {
    id: String,
    count: u64,
    next: u64,
    byte_length: usize,
    data: Vec<u8>,
}

#[derive(Debug)]
pub struct FrameDecoder {
    max_reassembled: usize,
    chunk: Option<Chunking>,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new(REASSEMBLY_CEILING)
    }
}

impl FrameDecoder {
    pub fn new(max_reassembled: usize) -> Self {
        Self { max_reassembled: max_reassembled.min(REASSEMBLY_CEILING), chunk: None }
    }

    /// `min(maxReassembledFrameBytes, 64 MiB)` from the `ready` frame.
    pub fn set_max_reassembled(&mut self, max: usize) {
        self.max_reassembled = max.min(REASSEMBLY_CEILING);
    }

    /// One physical line → a complete frame, or `None` while a chunk sequence is in progress.
    pub fn push(&mut self, line: &[u8]) -> Result<Option<Map<String, Value>>, String> {
        let frame = parse_object(line)?;
        if frame.get("type").and_then(Value::as_str) != Some("rpc_chunk") {
            if let Some(c) = &self.chunk {
                return Err(format!("rpc_chunk sequence {} interrupted by another frame at index {}", c.id, c.next));
            }
            return Ok(Some(frame));
        }
        let id = frame.get("chunkId").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or("rpc_chunk without chunkId")?;
        let index = frame.get("index").and_then(Value::as_u64).ok_or("rpc_chunk without index")?;
        let count = frame.get("count").and_then(Value::as_u64).filter(|c| *c >= 1).ok_or("rpc_chunk without a positive count")?;
        let byte_length = frame.get("byteLength").and_then(Value::as_u64).ok_or("rpc_chunk without byteLength")?;
        let data = frame.get("data").and_then(Value::as_str).ok_or("rpc_chunk without data")?;
        let byte_length = usize::try_from(byte_length).map_err(|_| "rpc_chunk byteLength out of range")?;
        match &self.chunk {
            None => {
                if index != 0 {
                    return Err(format!("rpc_chunk {id} starts at index {index}"));
                }
                if byte_length > self.max_reassembled {
                    return Err(format!("rpc_chunk {id} byteLength {byte_length} exceeds the reassembly limit {}", self.max_reassembled));
                }
                self.chunk = Some(Chunking { id: id.to_owned(), count, next: 0, byte_length, data: Vec::new() });
            }
            Some(c) => {
                if c.id != id {
                    return Err(format!("rpc_chunk {id} interleaved with {}", c.id));
                }
                if c.count != count || c.byte_length != byte_length {
                    return Err(format!("rpc_chunk {id} changed count/byteLength mid-sequence"));
                }
                if index != c.next {
                    return Err(format!("rpc_chunk {id} index {index}, expected {}", c.next));
                }
            }
        }
        let Some(c) = self.chunk.as_mut() else { return Err("rpc_chunk state lost".into()) };
        let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| format!("rpc_chunk {id} data: {e}"))?;
        if c.data.len() + bytes.len() > c.byte_length {
            return Err(format!("rpc_chunk {id} exceeds byteLength {}", c.byte_length));
        }
        c.data.extend_from_slice(&bytes);
        c.next += 1;
        if c.next < c.count {
            return Ok(None);
        }
        let done = self.chunk.take().ok_or("rpc_chunk state lost")?;
        if done.data.len() != done.byte_length {
            return Err(format!("rpc_chunk {} reassembled {} bytes, byteLength {}", done.id, done.data.len(), done.byte_length));
        }
        let text = std::str::from_utf8(&done.data).map_err(|e| format!("rpc_chunk {} is not UTF-8: {e}", done.id))?;
        parse_object(text.as_bytes()).map(Some)
    }

    /// `Err` if the stream ended inside a chunk sequence.
    pub fn finish(&self) -> Result<(), String> {
        match &self.chunk {
            Some(c) => Err(format!("stdout ended inside rpc_chunk sequence {} at index {}", c.id, c.next)),
            None => Ok(()),
        }
    }
}

fn parse_object(bytes: &[u8]) -> Result<Map<String, Value>, String> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err("frame is not a JSON object".into()),
        Err(e) => Err(format!("unparseable frame: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chunks(id: &str, payload: &str, size: usize) -> Vec<Vec<u8>> {
        let bytes = payload.as_bytes();
        let parts: Vec<&[u8]> = bytes.chunks(size).collect();
        parts
            .iter()
            .enumerate()
            .map(|(i, p)| {
                serde_json::to_vec(&json!({
                    "type": "rpc_chunk", "chunkId": id, "index": i, "count": parts.len(),
                    "byteLength": bytes.len(), "data": base64::engine::general_purpose::STANDARD.encode(p),
                }))
                .unwrap()
            })
            .collect()
    }

    fn big_frame() -> String {
        json!({"type": "response", "id": "h-4", "command": "get_state", "success": true, "data": {"systemPrompt": ["é".repeat(3000)]}})
            .to_string()
    }

    #[test]
    fn plain_frames_pass_through() {
        let mut d = FrameDecoder::default();
        let f = d.push(br#"{"type":"agent_start"}"#).unwrap().unwrap();
        assert_eq!(f["type"], "agent_start");
        assert!(d.push(b"[1,2]").is_err());
        assert!(d.push(b"not json").is_err());
    }

    #[test]
    fn chunk_reassembly_happy_path() {
        let payload = big_frame();
        let mut d = FrameDecoder::default();
        let lines = chunks("rpc-1", &payload, 1000);
        assert!(lines.len() > 3);
        for line in &lines[..lines.len() - 1] {
            assert_eq!(d.push(line).unwrap(), None);
        }
        let frame = d.push(lines.last().unwrap()).unwrap().unwrap();
        assert_eq!(Value::Object(frame), serde_json::from_str::<Value>(&payload).unwrap());
        assert!(d.finish().is_ok());
        // The decoder is reusable after a complete sequence.
        assert!(d.push(br#"{"type":"session_settled"}"#).unwrap().is_some());
    }

    #[test]
    fn interleaved_frame_is_rejected() {
        let lines = chunks("rpc-1", &big_frame(), 1000);
        let mut d = FrameDecoder::default();
        d.push(&lines[0]).unwrap();
        let err = d.push(br#"{"type":"agent_start"}"#).unwrap_err();
        assert!(err.contains("interrupted"), "{err}");
        let other = chunks("rpc-2", &big_frame(), 1000);
        let mut d = FrameDecoder::default();
        d.push(&lines[0]).unwrap();
        assert!(d.push(&other[1]).unwrap_err().contains("interleaved"));
    }

    #[test]
    fn missing_index_is_rejected() {
        let lines = chunks("rpc-1", &big_frame(), 1000);
        let mut d = FrameDecoder::default();
        d.push(&lines[0]).unwrap();
        assert!(d.push(&lines[2]).unwrap_err().contains("expected 1"));
        let mut d = FrameDecoder::default();
        assert!(d.push(&lines[1]).unwrap_err().contains("starts at index 1"));
    }

    #[test]
    fn wrong_byte_length_is_rejected() {
        let payload = big_frame();
        let mut lines: Vec<Value> = chunks("rpc-1", &payload, 1000).iter().map(|l| serde_json::from_slice(l).unwrap()).collect();
        for l in &mut lines {
            l["byteLength"] = json!(payload.len() + 5);
        }
        let mut d = FrameDecoder::default();
        let mut result = Ok(None);
        for l in &lines {
            result = d.push(&serde_json::to_vec(l).unwrap());
            if result.is_err() {
                break;
            }
        }
        assert!(result.unwrap_err().contains("byteLength"));
        // Too short a byteLength fails as soon as the data overflows it.
        let mut short: Vec<Value> = chunks("rpc-1", &payload, 1000).iter().map(|l| serde_json::from_slice(l).unwrap()).collect();
        for l in &mut short {
            l["byteLength"] = json!(10);
        }
        let mut d = FrameDecoder::default();
        assert!(d.push(&serde_json::to_vec(&short[0]).unwrap()).unwrap_err().contains("exceeds byteLength"));
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let raw: Vec<u8> = vec![b'{', b'"', b'a', b'"', b':', b'"', 0xff, 0xfe, b'"', b'}'];
        let line = serde_json::to_vec(&json!({
            "type": "rpc_chunk", "chunkId": "rpc-1", "index": 0, "count": 1, "byteLength": raw.len(),
            "data": base64::engine::general_purpose::STANDARD.encode(&raw),
        }))
        .unwrap();
        assert!(FrameDecoder::default().push(&line).unwrap_err().contains("UTF-8"));
    }

    #[test]
    fn over_reassembly_limit_is_rejected() {
        let payload = big_frame();
        let lines = chunks("rpc-1", &payload, 1000);
        let mut d = FrameDecoder::new(payload.len() - 1);
        assert!(d.push(&lines[0]).unwrap_err().contains("reassembly limit"));
        // The advertised limit can never raise the 64 MiB ceiling.
        let mut d = FrameDecoder::default();
        d.set_max_reassembled(usize::MAX);
        let huge = serde_json::to_vec(&json!({
            "type": "rpc_chunk", "chunkId": "x", "index": 0, "count": 2, "byteLength": REASSEMBLY_CEILING + 1, "data": "",
        }))
        .unwrap();
        assert!(d.push(&huge).unwrap_err().contains("reassembly limit"));
    }

    #[test]
    fn interrupted_sequence_at_eof() {
        let lines = chunks("rpc-1", &big_frame(), 1000);
        let mut d = FrameDecoder::default();
        d.push(&lines[0]).unwrap();
        assert!(d.finish().unwrap_err().contains("ended inside"));
    }
}
