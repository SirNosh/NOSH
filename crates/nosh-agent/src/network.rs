//! `readAllowedUrl`: the Task Packet's bounded, read-only network reader.

use serde_json::{Value, json};
use std::time::Duration;

const MAX_BYTES: usize = 1_000_000;
const TIMEOUT: Duration = Duration::from_secs(15);

/// GET one HTTPS URL whose hostname is on `allowlist`: no credentials, no custom port, no
/// redirects, at most 1 MB, 15 s. Returns `{ url, status, contentType, body }`.
///
/// Errors carry Node's texts for the same failures ("Invalid URL", "fetch failed", and the timeout
/// abort). Unlike Node's fetch, no compressed encoding is requested, so the body arrives as sent.
pub async fn read_allowed_url(url_text: &str, allowlist: &[String]) -> Result<Value, String> {
    let url = reqwest::Url::parse(url_text).map_err(|_| "Invalid URL".to_owned())?;
    if url.scheme() != "https" {
        return Err("URL must use HTTPS".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL credentials are forbidden".into());
    }
    if url.port().is_some() {
        return Err("Custom ports are forbidden".into());
    }
    let hostname = url.host_str().unwrap_or_default().to_lowercase();
    if !allowlist.contains(&hostname) {
        return Err("URL hostname is not allowlisted".into());
    }
    // Node's fetch ignores proxy environment variables; so does this reader.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(TIMEOUT)
        .build()
        .map_err(|_| "fetch failed".to_owned())?;
    let mut response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|error| fetch_error(&error))?;
    let status = response.status().as_u16();
    if (300..400).contains(&status) {
        return Err(
            "Redirects are not followed; submit the allowlisted destination URL directly".into(),
        );
    }
    if !response.status().is_success() {
        return Err(format!(
            "Read-only network request failed with HTTP {status}"
        ));
    }
    let length =
        header(&response, reqwest::header::CONTENT_LENGTH).map_or(0.0, |text| number(&text));
    #[allow(clippy::cast_precision_loss)]
    if length > MAX_BYTES as f64 {
        return Err("Response exceeds the 1 MB read-only limit".into());
    }
    let content_type = header(&response, reqwest::header::CONTENT_TYPE);
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| fetch_error(&error))?
    {
        if body.len() + chunk.len() > MAX_BYTES {
            return Err("Response exceeds the 1 MB read-only limit".into());
        }
        body.extend_from_slice(&chunk);
    }
    // TextDecoder: replacement characters for invalid UTF-8, and a leading BOM dropped.
    let text = String::from_utf8_lossy(&body);
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
    Ok(json!({ "url": url.as_str(), "status": status, "contentType": content_type, "body": text }))
}

fn fetch_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "The operation was aborted due to timeout".into()
    } else {
        "fetch failed".into()
    }
}

/// `Headers.get`: every value of the header joined by ", ", each byte read as Latin-1.
fn header(response: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    let values: Vec<String> = response
        .headers()
        .get_all(name)
        .iter()
        .map(|value| {
            value
                .as_bytes()
                .iter()
                .map(|&byte| char::from(byte))
                .collect()
        })
        .collect();
    (!values.is_empty()).then(|| values.join(", "))
}

/// `Number(text)` for a decimal header value: surrounding whitespace ignored, empty is 0,
/// anything else NaN.
fn number(text: &str) -> f64 {
    let text = text.trim_matches(|character: char| character.is_ascii_whitespace());
    if text.is_empty() {
        return 0.0;
    }
    let decimal = text
        .bytes()
        .all(|byte| byte.is_ascii_digit() || b".eE+-".contains(&byte));
    if decimal {
        text.parse().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}
