//! Streaming chat calls to Ollama and OpenAI-compatible endpoints, used by the custom agent harness.
use crate::config::AgentSpec;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
// Local models can take minutes to load or to produce the first token.
const READ_TIMEOUT: Duration = Duration::from_secs(300);

/// Sends `history` to the model described by `spec` and calls `on_chunk` for every text delta.
pub fn stream_chat(
    spec: &AgentSpec,
    history: &[Value],
    on_chunk: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    let (url, api_key, system, model, parse): (
        String,
        Option<String>,
        &Option<String>,
        &str,
        LineParser,
    ) = match spec {
        AgentSpec::Ollama {
            model,
            base_url,
            system,
        } => {
            let url = format!("{}/api/chat", base_url.trim_end_matches('/'));
            (url, None, system, model, parse_ollama_line)
        }
        AgentSpec::Openai {
            model,
            base_url,
            api_key_env,
            system,
        } => {
            let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
            let key = api_key_env
                .as_ref()
                .map(|var| std::env::var(var).with_context(|| format!("{var} is not set")))
                .transpose()?;
            (url, key, system, model, parse_sse_line)
        }
        AgentSpec::Acp { .. } => bail!("not a custom LLM agent"),
    };

    let mut messages: Vec<Value> = system
        .iter()
        .map(|text| json!({"role": "system", "content": text}))
        .collect();
    messages.extend_from_slice(history);

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .build();
    let mut request = agent.post(&url);
    if let Some(key) = api_key {
        request = request.set("Authorization", &format!("Bearer {key}"));
    }
    let body = json!({"model": model, "messages": messages, "stream": true});
    let response = match request.send_json(body) {
        Ok(response) => response,
        Err(ureq::Error::Status(code, response)) => {
            bail!(
                "{url} returned {code}: {}",
                response.into_string().unwrap_or_default()
            )
        }
        Err(error) => return Err(error).with_context(|| format!("calling {url}")),
    };
    for line in BufReader::new(response.into_reader()).lines() {
        if let Some(text) = parse(&line?)? {
            on_chunk(&text)?;
        }
    }
    Ok(())
}

type LineParser = fn(&str) -> Result<Option<String>>;

/// Extracts the text delta from one Ollama NDJSON line; None for lines without text.
fn parse_ollama_line(line: &str) -> Result<Option<String>> {
    let value: Value = serde_json::from_str(line).context("invalid Ollama stream line")?;
    if let Some(error) = value["error"].as_str() {
        bail!("ollama error: {error}");
    }
    Ok(non_empty(value["message"]["content"].as_str()))
}

/// Extracts the text delta from one OpenAI server-sent-events line; None for non-data lines and `[DONE]`.
fn parse_sse_line(line: &str) -> Result<Option<String>> {
    let Some(data) = line.strip_prefix("data:").map(str::trim) else {
        return Ok(None);
    };
    if data == "[DONE]" {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(data).context("invalid SSE data line")?;
    if !value["error"].is_null() {
        bail!("api error: {}", value["error"]);
    }
    Ok(non_empty(value["choices"][0]["delta"]["content"].as_str()))
}

/// Maps empty or missing text to None so callers skip no-op chunks.
fn non_empty(text: Option<&str>) -> Option<String> {
    text.filter(|t| !t.is_empty()).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The final Ollama line has empty content and must not emit a chunk.
    #[test]
    fn ollama_lines() {
        let line = r#"{"message":{"role":"assistant","content":"hi"},"done":false}"#;
        assert_eq!(parse_ollama_line(line).unwrap().as_deref(), Some("hi"));
        assert_eq!(
            parse_ollama_line(r#"{"message":{"content":""},"done":true}"#).unwrap(),
            None
        );
        assert!(parse_ollama_line(r#"{"error":"model not found"}"#).is_err());
    }

    // SSE streams interleave comments and blank lines with data lines and end with [DONE].
    #[test]
    fn sse_lines() {
        let line = r#"data: {"choices":[{"delta":{"content":"yo"}}]}"#;
        assert_eq!(parse_sse_line(line).unwrap().as_deref(), Some("yo"));
        assert_eq!(parse_sse_line("data: [DONE]").unwrap(), None);
        assert_eq!(parse_sse_line(": keepalive").unwrap(), None);
        assert_eq!(parse_sse_line("").unwrap(), None);
    }
}
