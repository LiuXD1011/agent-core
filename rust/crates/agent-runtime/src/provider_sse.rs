use agent_core::types::CancellationToken;
use std::io::BufRead;

pub(super) const RESPONSE_LIMIT: usize = 8_388_608;
const EVENT_LIMIT: usize = 1_048_576;
pub(super) fn preserved_error_code(code: &str) -> bool {
    matches!(
        code,
        "context_length_exceeded"
            | "context_window_exceeded"
            | "too_many_tokens"
            | "max_context_length_exceeded"
            | "insufficient_quota"
            | "billing_hard_limit_reached"
            | "GoUsageLimitError"
            | "FreeUsageLimitError"
    )
}

/// SSE framing is shared by native protocols; UTF-8 decoding stays in BufRead.
pub(super) fn next_event(
    reader: &mut impl BufRead,
    signal: &CancellationToken,
) -> Result<Option<String>, String> {
    let mut data = String::new();
    loop {
        if signal.is_cancelled() {
            return Err("Operation aborted".into());
        }
        let mut line = String::new();
        let size = std::io::Read::take(&mut *reader, EVENT_LIMIT as u64)
            .read_line(&mut line)
            .map_err(|error| {
                if signal.is_cancelled() {
                    "Operation aborted".to_owned()
                } else {
                    let text = error.to_string();
                    let Some(http) = text.strip_prefix("模型服务返回 HTTP ") else {
                        return "Model response transport failed".to_owned();
                    };
                    let mut fields = http.split_whitespace();
                    let Some(status) = fields
                        .next()
                        .and_then(|status| status.parse::<u16>().ok())
                        .filter(|status| (100..=599).contains(status))
                    else {
                        return "Model response transport failed".to_owned();
                    };
                    let code = fields.next().filter(|code| {
                        code.strip_prefix('(')
                            .and_then(|code| code.strip_suffix(')'))
                            .is_some_and(preserved_error_code)
                    });
                    match code {
                        Some(code) => format!("模型服务返回 HTTP {status} {code}"),
                        None => format!("模型服务返回 HTTP {status}"),
                    }
                }
            })?;
        if size >= EVENT_LIMIT {
            return Err("SSE line exceeds limit".into());
        }
        if size == 0 {
            return Ok((!data.is_empty()).then_some(data));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if !data.is_empty() {
                return Ok(Some(data));
            }
            continue;
        }
        if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
            if data.len() > EVENT_LIMIT {
                return Err("SSE event exceeds limit".into());
            }
        }
    }
}

pub(super) fn json_event(data: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(data).map_err(|_| "Invalid JSON in model stream".into())
}
