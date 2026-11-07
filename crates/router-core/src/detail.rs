//! A provider's own explanation of a failure, made safe to keep in an
//! operator's log or record.
//!
//! Providers explain a refusal in words ("Your credit balance is too low",
//! "messages.1.content.0: text content blocks must be non-empty"), and those
//! words are what tells an operator why requests fail. They can also echo
//! what the caller sent or name accounts and keys, so the router never sends
//! them to clients and keeps them only after [`scrub`]: quoted text longer
//! than a short identifier, long numbers, addresses and secret-shaped words
//! are replaced with a marker, and the result is cut short.

use serde_json::Value;

/// What stands in for anything taken out.
pub const HIDDEN: &str = "‹…›";
/// The longest explanation kept, in characters.
pub const DETAIL_CHARS: usize = 300;
/// A quoted value kept as it is: a field name or an option, not a sentence.
const QUOTED_CHARS: usize = 40;
/// A run of digits this long is an account, organisation or phone number.
const LONG_NUMBER: usize = 7;

const QUOTES: [(char, char); 5] = [
    ('"', '"'),
    ('\'', '\''),
    ('`', '`'),
    ('\u{201c}', '\u{201d}'),
    ('\u{2018}', '\u{2019}'),
];

/// The explanation in a provider's error body, scrubbed. `OpenRouter` wraps
/// the serving provider's own error in `metadata.raw`; its words and the
/// provider's name follow the outer message.
#[must_use]
pub fn from_error_body(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        let text = text.trim();
        // A proxy's HTML page says nothing a status line does not.
        if text.is_empty() || text.starts_with('<') {
            return None;
        }
        return scrub(text);
    };
    from_error_value(&value)
}

/// [`from_error_body`] for a body already parsed, or an in-stream error event.
#[must_use]
pub fn from_error_value(value: &Value) -> Option<String> {
    let error = value.get("error").filter(|error| error.is_object());
    let message = message_of(value);
    let metadata = error.and_then(|error| error.get("metadata"));
    let provider = metadata
        .and_then(|metadata| metadata.get("provider_name"))
        .and_then(Value::as_str);
    let inner = metadata
        .and_then(|metadata| metadata.get("raw"))
        .and_then(|raw| match raw {
            Value::String(raw) => serde_json::from_str::<Value>(raw)
                .ok()
                .and_then(|raw| message_of(&raw))
                .or_else(|| Some(raw.clone())),
            raw => message_of(raw),
        })
        .filter(|inner| Some(inner) != message.as_ref());
    let text = match (message, inner) {
        (Some(message), Some(inner)) => {
            format!("{message} ({}: {inner})", provider.unwrap_or("upstream"))
        }
        (Some(message), None) => match provider {
            Some(provider) => format!("{message} ({provider})"),
            None => message,
        },
        (None, Some(inner)) => inner,
        (None, None) => return None,
    };
    scrub(&text)
}

fn message_of(value: &Value) -> Option<String> {
    [
        "/error/message",
        "/message",
        "/detail",
        "/error",
        "/errors/0/message",
        "/0/error/message",
    ]
    .iter()
    .find_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
    .map(str::trim)
    .filter(|message| !message.is_empty())
    .map(str::to_owned)
}

/// `text` without quoted sentences, long numbers, addresses or secret-shaped
/// words, on one line and at most [`DETAIL_CHARS`] long; `None` when nothing
/// is left.
#[must_use]
pub fn scrub(text: &str) -> Option<String> {
    let chars: Vec<char> = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut unquoted = String::with_capacity(text.len().min(4 * DETAIL_CHARS));
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        let opens = QUOTES
            .iter()
            .find(|(open, _)| *open == c)
            .map(|(_, close)| *close);
        // An apostrophe inside a word ("doesn't") opens nothing.
        let after_word = index > 0 && chars[index - 1].is_alphanumeric();
        let Some(close) = opens.filter(|_| !after_word) else {
            unquoted.push(c);
            index += 1;
            continue;
        };
        let end = (index + 1..chars.len()).find(|&at| {
            chars[at] == close
                && (close != '\'' || chars.get(at + 1).is_none_or(|next| !next.is_alphanumeric()))
        });
        let Some(end) = end else {
            // Unclosed: what follows may be the caller's own text.
            unquoted.push_str(HIDDEN);
            break;
        };
        let inner: String = chars[index + 1..end].iter().collect();
        if inner.chars().count() <= QUOTED_CHARS
            && !inner.chars().any(char::is_whitespace)
            && scrub_word(&inner) == inner
        {
            unquoted.push(c);
            unquoted.push_str(&inner);
            unquoted.push(close);
        } else {
            unquoted.push_str(HIDDEN);
        }
        index = end + 1;
    }
    let mut out = String::new();
    for word in unquoted.split_whitespace().map(scrub_word) {
        // One marker for a run of hidden words.
        if word == HIDDEN && out.ends_with(HIDDEN) {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&word);
    }
    if out.chars().count() > DETAIL_CHARS {
        out = out.chars().take(DETAIL_CHARS - 1).collect();
        out.push('…');
    }
    (!out.is_empty() && out != HIDDEN).then_some(out)
}

/// One whitespace-free word, with an address, a secret-shaped part or a long
/// number replaced by [`HIDDEN`]. A link keeps its host only.
fn scrub_word(word: &str) -> String {
    let core = word.trim_matches(|c: char| !c.is_alphanumeric());
    if core.is_empty() {
        return word.to_owned();
    }
    let lower = word.to_ascii_lowercase();
    if let Some(at) = lower.find("://") {
        let rest = &word[at + 3..];
        let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let host = &rest[..host_end];
        if host.contains('@') {
            return HIDDEN.to_owned();
        }
        let tail = if host_end < rest.len() { "/…" } else { "" };
        return format!("{}{host}{tail}", &word[..at + 3]);
    }
    if core.contains('@')
        && core
            .rsplit('@')
            .next()
            .is_some_and(|domain| domain.contains('.'))
    {
        return HIDDEN.to_owned();
    }
    if core
        .split(['.', '/', ':', '=', ',', '(', ')', '[', ']'])
        .any(secret_shaped)
    {
        return word.replace(core, HIDDEN);
    }
    hide_long_numbers(word)
}

/// A key, token, signature or opaque ID: long, and either mixing cases with
/// digits (base64, `toolu_01PMq…`) or carrying many digits (hex, UUIDs).
fn secret_shaped(part: &str) -> bool {
    let length = part.chars().count();
    let digits = part.chars().filter(char::is_ascii_digit).count();
    let upper = part.chars().any(|c| c.is_ascii_uppercase());
    let lower = part.chars().any(|c| c.is_ascii_lowercase());
    (length >= 16 && upper && lower && digits > 0) || (length >= 32 && digits >= 8)
}

fn hide_long_numbers(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut digits = String::new();
    let flush = |out: &mut String, digits: &mut String| {
        if digits.len() >= LONG_NUMBER {
            out.push_str(HIDDEN);
        } else {
            out.push_str(digits);
        }
        digits.clear();
    };
    for c in word.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else {
            flush(&mut out, &mut digits);
            out.push(c);
        }
    }
    flush(&mut out, &mut digits);
    out
}

#[cfg(test)]
mod tests {
    use super::{DETAIL_CHARS, HIDDEN, from_error_body, scrub};

    #[test]
    fn keeps_a_providers_explanation_in_its_own_words() {
        let anthropic = br#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."},"request_id":"req_011Cgo6LUUsAMJ82Ejv2kXiP"}"#;
        assert_eq!(
            from_error_body(anthropic).unwrap(),
            "Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."
        );
        let paths = "messages.1.content.0: unexpected `tool_use_id` found in `tool_result` blocks: toolu_01PMqDN9epyYWtZ4aFK48Jbf. Each `tool_result` block must have a corresponding `tool_use` block in the previous message.";
        assert_eq!(
            scrub(paths).unwrap(),
            format!(
                "messages.1.content.0: unexpected `tool_use_id` found in `tool_result` blocks: {HIDDEN}. Each `tool_result` block must have a corresponding `tool_use` block in the previous message."
            )
        );
        assert_eq!(
            scrub("max_tokens: 64001 > 64000, which is the maximum allowed for this model")
                .unwrap(),
            "max_tokens: 64001 > 64000, which is the maximum allowed for this model"
        );
        assert_eq!(
            scrub("This model doesn't support 'tool_choice'.").unwrap(),
            "This model doesn't support 'tool_choice'."
        );
    }

    #[test]
    fn takes_out_what_the_caller_sent_and_whose_account_it_is() {
        let echoed =
            "Invalid content: 'please refactor the login handler in src/auth.rs' is not allowed";
        assert_eq!(
            scrub(echoed).unwrap(),
            format!("Invalid content: {HIDDEN} is not allowed")
        );
        let unclosed = "Bad request: \"the user's secret plan is";
        assert_eq!(scrub(unclosed).unwrap(), format!("Bad request: {HIDDEN}"));
        let account = "Request too large for organization org-AbCdEf1234567890xyz on tokens per minute (TPM): Limit 8000, Requested 12082. Contact ops@example.com or see https://platform.example.com/account/rate-limits?org=42";
        let scrubbed = scrub(account).unwrap();
        assert!(!scrubbed.contains("AbCdEf"), "{scrubbed}");
        assert!(!scrubbed.contains("ops@example.com"), "{scrubbed}");
        assert!(
            scrubbed.contains("Limit 8000, Requested 12082"),
            "{scrubbed}"
        );
        assert!(
            scrubbed.contains("https://platform.example.com/…"),
            "{scrubbed}"
        );
        assert!(!scrubbed.contains("org=42"), "{scrubbed}");
        let key = "Incorrect API key provided: sk-proj-AbCdEfGhIjKlMnOpQrStUvWx1234";
        assert_eq!(
            scrub(key).unwrap(),
            format!("Incorrect API key provided: {HIDDEN}")
        );
        assert_eq!(
            scrub("project 057122764100 has no access to 01a1877e-83e6-7715-ad8e-243cbe3510c2")
                .unwrap(),
            format!("project {HIDDEN} has no access to {HIDDEN}")
        );
        assert!(scrub("'everything here was quoted by the caller'").is_none());
    }

    #[test]
    fn reads_the_serving_providers_words_inside_an_openrouter_error() {
        let body = br#"{"error":{"message":"Provider returned error","code":400,"metadata":{"raw":"{\"message\":\"invalid tool call id format\"}","provider_name":"Cohere"}}}"#;
        assert_eq!(
            from_error_body(body).unwrap(),
            "Provider returned error (Cohere: invalid tool call id format)"
        );
        let plain = br#"{"error":{"message":"deepseek/deepseek-chat is temporarily rate-limited upstream. Please retry shortly.","code":429,"metadata":{"provider_name":"DeepSeek"}}}"#;
        assert_eq!(
            from_error_body(plain).unwrap(),
            "deepseek/deepseek-chat is temporarily rate-limited upstream. Please retry shortly. (DeepSeek)"
        );
        assert!(from_error_body(b"<html><body>502 Bad Gateway</body></html>").is_none());
        assert!(from_error_body(b"{}").is_none());
    }

    #[test]
    fn is_one_short_line() {
        let long = "word ".repeat(200);
        let scrubbed = scrub(&format!("first\nsecond\t{long}")).unwrap();
        assert!(scrubbed.starts_with("first second word"));
        assert_eq!(scrubbed.chars().count(), DETAIL_CHARS);
        assert!(scrubbed.ends_with('…'));
    }
}
