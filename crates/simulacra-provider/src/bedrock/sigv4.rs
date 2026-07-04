//! AWS Signature Version 4 (SigV4) request signer.
//!
//! Implemented in-process to avoid pulling in the AWS SDK, preserving the
//! single-binary / minimal-dependency philosophy from `ARCHITECTURE.md`.
//! Only the subset needed to sign Bedrock Converse API POST requests is
//! supported: the method is always POST and all request parameters travel
//! in the (hashed) body, so the canonical query string is empty.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Percent-encode a string using the AWS SigV4 / RFC 3986 unreserved set.
///
/// When `encode_slash` is `false`, forward slashes are left untouched — this
/// is what we want when encoding a *path* (so segment separators survive).
pub(crate) fn uri_encode(input: &str, encode_slash: bool) -> String {
    use percent_encoding::{AsciiSet, NON_ALPHANUMERIC};
    // RFC 3986 unreserved set = A-Za-z0-9 - . _ ~
    const UNRESERVED: &AsciiSet = &(NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~'));
    const UNRESERVED_PATH: &AsciiSet = &(NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~')
        .remove(b'/'));
    let set: &AsciiSet = if encode_slash {
        UNRESERVED
    } else {
        UNRESERVED_PATH
    };
    percent_encoding::utf8_percent_encode(input, set).to_string()
}

/// Credentials used to sign a request.
#[derive(Debug, Clone)]
pub(crate) struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Optional temporary-session token (STS / role chain / web identity).
    pub session_token: Option<String>,
}

/// The wire location of a signed request.
#[derive(Debug, Clone)]
pub(crate) struct SigningTarget {
    pub host: String,
    /// Absolute path **as it will appear on the wire** (already percent-encoded
    /// for any reserved characters in the model id), e.g.
    /// `/model/anthropic.claude-3-5-sonnet-v1%3A0/converse`.
    pub path: String,
}

/// Inputs needed to sign one request at a point in time.
pub(crate) struct SigningRequest<'a> {
    pub credentials: &'a Credentials,
    pub region: &'a str,
    /// AWS service name (`"bedrock"`).
    pub service: &'a str,
    /// ISO 8601 basic timestamp, e.g. `20240101T120000Z`.
    pub amz_date: &'a str,
    /// Date component (YYYYMMDD), e.g. `20240101`.
    pub date_stamp: &'a str,
    pub target: &'a SigningTarget,
    pub body: &'a [u8],
}

/// The result of signing: the `Authorization` header plus the other SigV4
/// headers that must be attached to the outgoing request.
#[derive(Debug, Clone)]
pub(crate) struct SignedHeaders {
    pub authorization: String,
    /// `x-amz-date`, `x-amz-content-sha256`, and (when present)
    /// `x-amz-security-token`.
    pub extra: Vec<(String, String)>,
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Sign a request and return the headers to attach to it.
///
/// Implements the canonical SigV4 algorithm:
/// 1. Build the canonical request.
/// 2. Build the string-to-sign.
/// 3. Derive the signing key (`kDate → kRegion → kService → kSigning`).
/// 4. Compute the signature.
/// 5. Build the `Authorization` header.
pub(crate) fn sign(req: SigningRequest<'_>) -> SignedHeaders {
    let payload_hash = sha256_hex(req.body);

    // Canonical URI is the absolute path component only (host lives in the
    // canonical headers). All request parameters travel in the body, so the
    // canonical query string is empty.
    let canonical_uri = req.target.path.clone();
    let canonical_query = String::new();

    // Canonical headers: lowercased names, trimmed values, sorted by name,
    // with a trailing '\n' after the whole block. host, x-amz-date, and
    // x-amz-content-sha256 are always signed; x-amz-security-token is signed
    // when a session token is present.
    let mut header_lines: Vec<(String, String)> = vec![
        ("host".to_string(), req.target.host.clone()),
        ("x-amz-content-sha256".to_string(), payload_hash.clone()),
        ("x-amz-date".to_string(), req.amz_date.to_string()),
    ];
    if let Some(token) = &req.credentials.session_token {
        header_lines.push(("x-amz-security-token".to_string(), token.clone()));
    }
    header_lines.sort_by(|a, b| a.0.cmp(&b.0));

    let canonical_headers: String = header_lines
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect();
    let signed_headers: String = header_lines
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "POST\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );

    let hashed_canonical_request = sha256_hex(canonical_request.as_bytes());

    let credential_scope = format!(
        "{}/{}/{}/aws4_request",
        req.date_stamp, req.region, req.service
    );

    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        req.amz_date, credential_scope, hashed_canonical_request
    );

    let k_date = hmac(
        format!("AWS4{}", req.credentials.secret_access_key).as_bytes(),
        req.date_stamp.as_bytes(),
    );
    let k_region = hmac(&k_date, req.region.as_bytes());
    let k_service = hmac(&k_region, req.service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");

    let signature = hex::encode(hmac(&k_signing, string_to_sign.as_bytes()));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        req.credentials.access_key_id, credential_scope, signed_headers, signature
    );

    let mut extra: Vec<(String, String)> = vec![
        ("x-amz-date".to_string(), req.amz_date.to_string()),
        ("x-amz-content-sha256".to_string(), payload_hash),
    ];
    if let Some(token) = &req.credentials.session_token {
        extra.push(("x-amz-security-token".to_string(), token.clone()));
    }

    SignedHeaders {
        authorization,
        extra,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reproduce the AWS SigV4 test-suite vector `get-vanilla-empty-query-key`
    /// (a community mirror of the official AWS `aws-sig-v4-test-suite`). We
    /// check every intermediate against the published numbers.
    ///
    /// `sign()` is POST-only (it always signs `x-amz-content-sha256`), so we
    /// recompute the canonical SigV4 intermediates directly here rather than
    /// routing through `sign()`.
    #[test]
    fn aws_get_vanilla_reference_vector_matches() {
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let date = "20150830";
        let amz_date = "20150830T123600Z";
        let region = "us-east-1";
        let service = "service";

        // Canonical request from the test suite (LF-joined).
        let canonical_request = concat!(
            "GET\n",
            "/\n",
            "Param1=value1\n",
            "host:example.amazonaws.com\n",
            "x-amz-date:20150830T123600Z\n",
            "\n",
            "host;x-amz-date\n",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        let hashed = sha256_hex(canonical_request.as_bytes());
        assert_eq!(
            hashed,
            "1e24db194ed7d0eec2de28d7369675a243488e08526e8c1c73571282f7c517ab"
        );

        let credential_scope = format!("{date}/{region}/{service}/aws4_request");
        let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{hashed}");

        let k_date = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
        let k_region = hmac(&k_date, region.as_bytes());
        let k_service = hmac(&k_region, service.as_bytes());
        let k_signing = hmac(&k_service, b"aws4_request");
        assert_eq!(
            hex::encode(&k_signing),
            "938127b5336810ddb6a5d6af445fcac9e371f9ed418ed386b022aed82901be75"
        );

        let signature = hex::encode(hmac(&k_signing, string_to_sign.as_bytes()));
        assert_eq!(
            signature,
            "a67d582fa61cc504c4bae71f336f98b97f1ea3c7a6bfe1b6e45aec72011b9aeb"
        );
    }

    #[test]
    fn sign_produces_authorization_header_with_expected_scheme() {
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: None,
        };
        let target = SigningTarget {
            host: "bedrock-runtime.us-east-1.amazonaws.com".to_string(),
            path: "/model/anthropic.claude-3-5-sonnet-20240620-v1%3A0/converse".to_string(),
        };
        let signed = sign(SigningRequest {
            credentials: &creds,
            region: "us-east-1",
            service: "bedrock",
            amz_date: "20240101T120000Z",
            date_stamp: "20240101",
            target: &target,
            body: b"{}",
        });

        assert!(signed.authorization.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20240101/us-east-1/bedrock/aws4_request"
        ));
        assert!(signed.authorization.contains("SignedHeaders="));
        assert!(signed.authorization.contains("Signature="));

        let extras: std::collections::HashMap<&str, &str> = signed
            .extra
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(extras.get("x-amz-date").copied(), Some("20240101T120000Z"));
        assert!(extras.contains_key("x-amz-content-sha256"));
        assert!(!extras.contains_key("x-amz-security-token"));
    }

    #[test]
    fn sign_adds_session_token_header_when_present() {
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: Some("session-token-value".to_string()),
        };
        let target = SigningTarget {
            host: "bedrock-runtime.us-east-1.amazonaws.com".to_string(),
            path: "/model/x/converse".to_string(),
        };
        let signed = sign(SigningRequest {
            credentials: &creds,
            region: "us-east-1",
            service: "bedrock",
            amz_date: "20240101T120000Z",
            date_stamp: "20240101",
            target: &target,
            body: b"{}",
        });

        assert!(
            signed.authorization.contains("x-amz-security-token"),
            "session token header must be signed: {}",
            signed.authorization
        );
        let has_token = signed
            .extra
            .iter()
            .any(|(k, v)| k == "x-amz-security-token" && v == "session-token-value");
        assert!(
            has_token,
            "session token must be attached to the outgoing request"
        );
    }

    #[test]
    fn sign_changes_when_body_changes() {
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: None,
        };
        let target = SigningTarget {
            host: "bedrock-runtime.us-east-1.amazonaws.com".to_string(),
            path: "/model/x/converse".to_string(),
        };

        let a = sign(SigningRequest {
            credentials: &creds,
            region: "us-east-1",
            service: "bedrock",
            amz_date: "20240101T120000Z",
            date_stamp: "20240101",
            target: &target,
            body: b"{\"a\":1}",
        });
        let b = sign(SigningRequest {
            credentials: &creds,
            region: "us-east-1",
            service: "bedrock",
            amz_date: "20240101T120000Z",
            date_stamp: "20240101",
            target: &target,
            body: b"{\"a\":2}",
        });

        assert_ne!(a.authorization, b.authorization);
    }

    #[test]
    fn uri_encode_encodes_reserved_characters_in_model_id() {
        // `:` is reserved and must be encoded in path components per RFC 3986.
        assert_eq!(
            uri_encode("anthropic.claude-3-5-sonnet-v1:0", false),
            "anthropic.claude-3-5-sonnet-v1%3A0"
        );
        // Slashes survive when encode_slash is false (path encoding).
        assert_eq!(uri_encode("/model/x/converse", false), "/model/x/converse");
    }
}
