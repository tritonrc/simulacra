//! Binary AWS Event Stream frame decoder.
//!
//! Bedrock converse-stream responses are framed in the AWS Event Stream
//! binary format (`application/vnd.amazon.eventstream`), NOT plain text SSE.
//! Each frame carries SigV4-unrelated headers (`:message-type`, `:event-type`,
//! `:content-type`) and a JSON payload. This module decodes frames off a byte
//! buffer that may arrive in arbitrary chunks.
//!
//! CRC-32 fields (prelude + message) are read past but not verified — TLS
//! guarantees integrity on the wire, and verification would pull in a crc32
//! dependency for no behavioral gain.

/// One decoded event-stream frame.
#[derive(Debug, Clone)]
pub(crate) struct ParsedFrame {
    /// Value of the `:message-type` header (e.g. `"event"`, `"error"`).
    pub message_type: Option<String>,
    /// Value of the `:event-type` header (e.g. `"contentBlockDelta"`).
    pub event_type: Option<String>,
    /// Parsed JSON payload.
    pub payload: serde_json::Value,
}

/// Stateful decoder that buffers raw bytes and yields complete frames.
#[derive(Default)]
pub(crate) struct BedrockEventStreamDecoder {
    buffer: Vec<u8>,
}

impl BedrockEventStreamDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed bytes and return all frames that became complete as a result.
    pub(crate) fn push_bytes(
        &mut self,
        bytes: &[u8],
    ) -> Result<Vec<ParsedFrame>, simulacra_types::ProviderError> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some(frame) = try_take_frame(&mut self.buffer)? {
            frames.push(frame);
        }
        Ok(frames)
    }
}

/// Attempt to parse exactly one frame from the front of `buf`.
///
/// Returns `Ok(None)` if the buffer does not yet contain a complete frame.
fn try_take_frame(
    buf: &mut Vec<u8>,
) -> Result<Option<ParsedFrame>, simulacra_types::ProviderError> {
    // Prelude: total_length (4) + headers_length (4) + prelude_crc (4) = 12 bytes.
    if buf.len() < 12 {
        return Ok(None);
    }
    let total_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if total_len < 16 {
        return Err(simulacra_types::ProviderError::Other(format!(
            "bedrock event stream: malformed total length {total_len}"
        )));
    }
    if buf.len() < total_len {
        // Incomplete frame; wait for more bytes.
        return Ok(None);
    }

    let headers_len = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    if 12 + headers_len + 4 > total_len {
        return Err(simulacra_types::ProviderError::Other(
            "bedrock event stream: headers length exceeds message".to_owned(),
        ));
    }

    let frame: Vec<u8> = buf.drain(..total_len).collect();
    // [0..12)  = prelude (lengths + prelude crc, ignored)
    // [12..12+headers_len) = headers
    // [12+headers_len .. total_len-4) = payload
    // [total_len-4 .. total_len) = message crc (ignored)
    let headers_bytes = &frame[12..12 + headers_len];
    let payload_bytes = &frame[12 + headers_len..total_len - 4];

    let headers = parse_headers(headers_bytes)?;
    let message_type = headers
        .iter()
        .find(|(k, _)| k == ":message-type")
        .map(|(_, v)| v.clone());
    let event_type = headers
        .iter()
        .find(|(k, _)| k == ":event-type")
        .map(|(_, v)| v.clone());

    let payload = if payload_bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(payload_bytes).unwrap_or(serde_json::Value::Null)
    };

    Ok(Some(ParsedFrame {
        message_type,
        event_type,
        payload,
    }))
}

/// Parse the headers section. Returns `(name, value)` pairs; only string /
/// byte-array values are materialized (all metadata headers we need are
/// strings), other value types are skipped using their known widths.
fn parse_headers(bytes: &[u8]) -> Result<Vec<(String, String)>, simulacra_types::ProviderError> {
    let mut headers = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let name_len = bytes[i] as usize;
        i += 1;
        if i + name_len > bytes.len() {
            return Err(simulacra_types::ProviderError::Other(
                "bedrock event stream: truncated header name".to_owned(),
            ));
        }
        let name = std::str::from_utf8(&bytes[i..i + name_len])
            .map_err(|e| simulacra_types::ProviderError::Other(format!("header name utf8: {e}")))?
            .to_owned();
        i += name_len;
        if i >= bytes.len() {
            return Err(simulacra_types::ProviderError::Other(
                "bedrock event stream: truncated header value type".to_owned(),
            ));
        }
        let value_type = bytes[i];
        i += 1;
        let value = read_value(value_type, bytes, &mut i)?;
        headers.push((name, value));
    }
    Ok(headers)
}

/// Read a header value of the given type from `bytes` starting at absolute
/// index `i`, advancing `i` past the value. Returns the string form for
/// string (6) / byte-array (5) values, empty string for everything else
/// (the metadata headers we care about are all strings).
fn read_value(
    value_type: u8,
    bytes: &[u8],
    i: &mut usize,
) -> Result<String, simulacra_types::ProviderError> {
    match value_type {
        0 | 1 => {
            // bool / byte
            advance(bytes, i, 1)?;
        }
        2 => {
            // short
            advance(bytes, i, 2)?;
        }
        3 => {
            // int
            advance(bytes, i, 4)?;
        }
        4 => {
            // long
            advance(bytes, i, 8)?;
        }
        5 => {
            // byte array
            let len = take_len(bytes, i)? as usize;
            advance(bytes, i, len)?;
        }
        6 => {
            // string
            let len = take_len(bytes, i)? as usize;
            advance(bytes, i, len)?;
            let start = *i - len;
            let s = std::str::from_utf8(&bytes[start..*i])
                .map_err(|e| {
                    simulacra_types::ProviderError::Other(format!("header value utf8: {e}"))
                })?
                .to_owned();
            return Ok(s);
        }
        7 => {
            // timestamp
            advance(bytes, i, 8)?;
        }
        8 => {
            // uuid
            advance(bytes, i, 16)?;
        }
        other => {
            return Err(simulacra_types::ProviderError::Other(format!(
                "bedrock event stream: unknown header value type {other}"
            )));
        }
    }
    Ok(String::new())
}

fn take_len(bytes: &[u8], i: &mut usize) -> Result<u16, simulacra_types::ProviderError> {
    if *i + 2 > bytes.len() {
        return Err(simulacra_types::ProviderError::Other(
            "bedrock event stream: truncated header value length".to_owned(),
        ));
    }
    let len = u16::from_be_bytes([bytes[*i], bytes[*i + 1]]);
    *i += 2;
    Ok(len)
}

fn advance(bytes: &[u8], i: &mut usize, n: usize) -> Result<(), simulacra_types::ProviderError> {
    if *i + n > bytes.len() {
        return Err(simulacra_types::ProviderError::Other(
            "bedrock event stream: truncated header value".to_owned(),
        ));
    }
    *i += n;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a single event-stream frame for testing.
    fn frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let mut header_bytes = Vec::new();
        for (name, value) in headers {
            header_bytes.push(name.len() as u8);
            header_bytes.extend_from_slice(name.as_bytes());
            header_bytes.push(6); // string value type
            let v = value.as_bytes();
            header_bytes.extend_from_slice(&(v.len() as u16).to_be_bytes());
            header_bytes.extend_from_slice(v);
        }
        let headers_len = header_bytes.len() as u32;
        let total_len = (12 + header_bytes.len() + payload.len() + 4) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&total_len.to_be_bytes());
        out.extend_from_slice(&headers_len.to_be_bytes());
        out.extend_from_slice(&[0u8; 4]); // prelude crc (ignored)
        out.extend_from_slice(&header_bytes);
        out.extend_from_slice(payload);
        out.extend_from_slice(&[0u8; 4]); // message crc (ignored)
        out
    }

    #[test]
    fn decodes_single_text_delta_frame() {
        let payload = br#"{"contentBlockIndex":0,"delta":{"text":"Hi"}}"#;
        let f = frame(
            &[
                (":message-type", "event"),
                (":event-type", "contentBlockDelta"),
                (":content-type", "application/json"),
            ],
            payload,
        );
        let mut dec = BedrockEventStreamDecoder::new();
        let frames = dec.push_bytes(&f).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].message_type.as_deref(), Some("event"));
        assert_eq!(frames[0].event_type.as_deref(), Some("contentBlockDelta"));
        assert_eq!(frames[0].payload["delta"]["text"], "Hi");
    }

    #[test]
    fn buffers_partial_frame_until_complete() {
        let payload = br#"{"contentBlockIndex":0,"delta":{"text":"x"}}"#;
        let f = frame(
            &[
                ((":message-type"), "event"),
                (":event-type", "contentBlockDelta"),
            ],
            payload,
        );
        let mut dec = BedrockEventStreamDecoder::new();
        let split = f.len() / 2;
        assert_eq!(dec.push_bytes(&f[..split]).unwrap().len(), 0);
        let frames = dec.push_bytes(&f[split..]).unwrap();
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn decodes_multiple_frames_in_one_chunk() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&frame(
            &[
                ((":message-type"), "event"),
                (":event-type", "messageStart"),
            ],
            br#"{"role":"assistant"}"#,
        ));
        bytes.extend_from_slice(&frame(
            &[((":message-type"), "event"), (":event-type", "messageStop")],
            br#"{"stopReason":"end_turn"}"#,
        ));
        let mut dec = BedrockEventStreamDecoder::new();
        let frames = dec.push_bytes(&bytes).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event_type.as_deref(), Some("messageStart"));
        assert_eq!(frames[1].event_type.as_deref(), Some("messageStop"));
    }

    #[test]
    fn surfaces_error_frames_with_payload_message() {
        let f = frame(
            &[
                ((":message-type"), "error"),
                (":exception-type", "ValidationException"),
            ],
            br#"{"message":"bad model id"}"#,
        );
        let mut dec = BedrockEventStreamDecoder::new();
        let frames = dec.push_bytes(&f).unwrap();
        assert_eq!(frames[0].message_type.as_deref(), Some("error"));
        assert_eq!(frames[0].payload["message"], "bad model id");
    }
}
