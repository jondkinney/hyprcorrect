//! Bounded parsing helpers for small JSON API responses.

use std::io::Read;

pub(crate) fn json_response(
    response: ureq::Response,
    maximum: usize,
) -> Result<serde_json::Value, String> {
    if response
        .header("Content-Length")
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(format!("response exceeds the {maximum}-byte limit"));
    }
    json_reader(response.into_reader(), maximum)
}

fn json_reader(mut reader: impl Read, maximum: usize) -> Result<serde_json::Value, String> {
    let mut bytes = Vec::new();
    (&mut reader)
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read response: {error}"))?;
    if bytes.len() > maximum {
        return Err(format!("response exceeds the {maximum}-byte limit"));
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("parse response JSON: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_reader_accepts_exact_limit_and_rejects_limit_plus_one() {
        let exact = br#"{"ok":true}"#;
        assert!(json_reader(exact.as_slice(), exact.len()).is_ok());
        assert!(json_reader(exact.as_slice(), exact.len() - 1).is_err());
    }
}
