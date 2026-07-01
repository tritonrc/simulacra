use super::FetchResponse;

/// Try to parse an HTTP response body as a `FetchResponse` JSON envelope of
/// the shape `{"status": u16, "headers": [[name, value], ...], "body": "<base64>"}`.
/// Returns `None` if any field is missing, malformed, or the body is not
/// valid base64. Used so that fixtures emitting `FetchResponse`-shaped JSON
/// can surface upstream status/headers to the module without the test
/// having to operate at the bare HTTP wire level.
pub(crate) fn parse_fetch_envelope(bytes: &[u8]) -> Option<FetchResponse> {
    use base64::Engine;
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let object = value.as_object()?;
    let status = object.get("status")?.as_u64()?;
    if status > u16::MAX as u64 {
        return None;
    }
    let status = status as u16;
    let headers_value = object.get("headers")?.as_array()?;
    let mut headers = Vec::with_capacity(headers_value.len());
    for header in headers_value {
        let pair = header.as_array()?;
        if pair.len() != 2 {
            return None;
        }
        let name = pair[0].as_str()?.to_string();
        let value = pair[1].as_str()?.to_string();
        headers.push((name, value));
    }
    let body_b64 = object.get("body")?.as_str()?;
    let body = base64::engine::general_purpose::STANDARD
        .decode(body_b64)
        .ok()?;
    Some(FetchResponse {
        status,
        headers,
        body,
    })
}
