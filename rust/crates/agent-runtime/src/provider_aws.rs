//! Small native SigV4 signer. No credential discovery or network runs in this module.
//! Canonicalization follows the installed Smithy signer and AWS IAM documentation.
use reqwest::Url;
use ring::hmac;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// No Debug/serde: signing material is never settings/session output.
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: String,
}
impl AwsCredentials {
    pub fn validate(&self) -> Result<(), String> {
        if self.access_key_id.is_empty() || self.secret_access_key.is_empty() {
            return Err("AWS access key ID and secret access key are required".into());
        }
        for value in [
            &self.access_key_id,
            &self.secret_access_key,
            &self.session_token,
        ] {
            super::validate_header(value)?;
        }
        if !self
            .access_key_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err("Invalid AWS access key ID format".into());
        }
        Ok(())
    }
}
pub struct SigV4Parameters<'a> {
    pub method: &'a str,
    pub url: &'a Url,
    pub body: &'a [u8],
    pub headers: &'a BTreeMap<String, String>,
    pub credentials: &'a AwsCredentials,
    pub region: &'a str,
    pub service: &'a str,
    pub timestamp: &'a str,
    /// AWS SDK defaults to true. False permits comparison with classic IAM examples.
    pub checksum_header: bool,
}
/// Exposed for deterministic offline verification; deliberately has no Debug/serde.
pub struct SigV4Signature {
    pub headers: BTreeMap<String, String>,
    pub canonical_request: String,
    pub string_to_sign: String,
    pub signature: String,
}
pub fn aws_uri_encode(bytes: &[u8], encode_slashes: bool) -> String {
    let mut encoded = String::new();
    for byte in bytes {
        if byte.is_ascii_alphanumeric()
            || matches!(*byte, b'-' | b'_' | b'.' | b'~')
            || (!encode_slashes && *byte == b'/')
        {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}
fn decode_query(source: &str) -> Result<Vec<u8>, String> {
    let bytes = source.as_bytes();
    let mut decoded = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let digits = bytes
                .get(index + 1..index + 3)
                .ok_or("Invalid SigV4 query escape")?;
            let hex = |byte: u8| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            };
            decoded.push(
                hex(digits[0])
                    .zip(hex(digits[1]))
                    .map(|(a, b)| a * 16 + b)
                    .ok_or("Invalid SigV4 query escape")?,
            );
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Ok(decoded)
}
fn keyed(key: &[u8], data: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data)
        .as_ref()
        .to_vec()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn scope_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}
fn unsigned_header(name: &str) -> bool {
    matches!(
        name,
        "authorization"
            | "cache-control"
            | "connection"
            | "expect"
            | "from"
            | "keep-alive"
            | "max-forwards"
            | "pragma"
            | "referer"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "user-agent"
            | "x-amzn-trace-id"
    ) || name.starts_with("proxy-")
        || name.starts_with("sec-")
}
pub fn sign_aws_v4(parameters: SigV4Parameters<'_>) -> Result<SigV4Signature, String> {
    let SigV4Parameters {
        method,
        url,
        body,
        headers,
        credentials,
        region,
        service,
        timestamp,
        checksum_header,
    } = parameters;
    credentials.validate()?;
    if !scope_component(region)
        || !scope_component(service)
        || method.is_empty()
        || !method.bytes().all(|byte| byte.is_ascii_uppercase())
    {
        return Err("Invalid AWS signing scope or method".into());
    }
    let date = timestamp.as_bytes();
    if date.len() != 16
        || date[8] != b'T'
        || date[15] != b'Z'
        || date
            .iter()
            .enumerate()
            .any(|(index, byte)| !matches!(index, 8 | 15) && !byte.is_ascii_digit())
    {
        return Err("Invalid AWS signing timestamp".into());
    }
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("Invalid AWS signing URL".into());
    }
    let mut normalized = BTreeMap::new();
    for (name, value) in headers {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || value
                .bytes()
                .any(|byte| !byte.is_ascii() || (byte.is_ascii_control() && byte != b'\t'))
        {
            return Err("Invalid AWS signing header".into());
        }
        let name = name.to_ascii_lowercase();
        if normalized
            .insert(
                name,
                value.split_ascii_whitespace().collect::<Vec<_>>().join(" "),
            )
            .is_some()
        {
            return Err("Duplicate AWS signing header".into());
        }
    }
    let host = url.host().ok_or("AWS signing host missing")?.to_string();
    normalized.insert(
        "host".into(),
        url.port()
            .map_or(host.clone(), |port| format!("{host}:{port}")),
    );
    normalized.insert("x-amz-date".into(), timestamp.into());
    if !credentials.session_token.is_empty() {
        normalized.insert(
            "x-amz-security-token".into(),
            credentials.session_token.clone(),
        );
    }
    let payload_hash = hex(&Sha256::digest(body));
    // Never trust a caller checksum that does not correspond to the actual request bytes.
    normalized.remove("x-amz-content-sha256");
    if checksum_header {
        normalized.insert("x-amz-content-sha256".into(), payload_hash.clone());
    }
    let signed: Vec<_> = normalized
        .keys()
        .filter(|name| !unsigned_header(name))
        .cloned()
        .collect();
    let canonical_headers = signed
        .iter()
        .map(|name| format!("{name}:{}\n", normalized[name]))
        .collect::<String>();
    let mut segments = vec![];
    for segment in url.path().split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            _ => segments.push(segment),
        }
    }
    let mut path = format!("/{}", segments.join("/"));
    if !segments.is_empty() && url.path().ends_with('/') {
        path.push('/');
    }
    let path = aws_uri_encode(path.as_bytes(), false);
    let mut query = vec![];
    if let Some(source) = url.query() {
        for pair in source.split('&').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let key = decode_query(key)?;
            if key.eq_ignore_ascii_case(b"x-amz-signature") {
                continue;
            }
            query.push((
                aws_uri_encode(&key, true),
                aws_uri_encode(&decode_query(value)?, true),
            ));
        }
    }
    query.sort();
    let query = query
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let signed_headers = signed.join(";");
    let canonical_request =
        format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
    let scope = format!("{}/{region}/{service}/aws4_request", &timestamp[..8]);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );
    let mut initial = format!("AWS4{}", credentials.secret_access_key).into_bytes();
    let mut key = keyed(&initial, &timestamp.as_bytes()[..8]);
    initial.fill(0);
    for component in [region, service, "aws4_request"] {
        let next = keyed(&key, component.as_bytes());
        key.fill(0);
        key = next;
    }
    let signature = hex(&keyed(&key, string_to_sign.as_bytes()));
    key.fill(0);
    normalized.insert("authorization".into(),format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",credentials.access_key_id));
    Ok(SigV4Signature {
        headers: normalized,
        canonical_request,
        string_to_sign,
        signature,
    })
}
pub fn aws_timestamp(seconds: u64) -> Result<String, String> {
    let days = i64::try_from(seconds / 86_400).map_err(|_| "AWS signing time out of range")?;
    let z = days
        .checked_add(719_468)
        .ok_or("AWS signing time out of range")?;
    let era = z / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    if !(1970..=9999).contains(&year) {
        return Err("AWS signing time out of range".into());
    }
    let time = seconds % 86_400;
    Ok(format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        time / 3600,
        time / 60 % 60,
        time % 60
    ))
}
