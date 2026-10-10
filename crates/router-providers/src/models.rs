use std::{collections::BTreeMap, time::Duration};

use futures::StreamExt;
use router_core::{ProviderConfig, ProviderProtocol};
use router_protocol::ModelObject;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

mod bedrock;
#[cfg(test)]
mod discovery_tests;

/// Account catalogs are data, not a whitelist for routing. Keep pagination,
/// response sizes and time bounded, and never forward credentials on redirects.
pub(crate) async fn discover(
    config: &ProviderConfig,
    key: Option<&SecretString>,
) -> Option<Vec<ModelObject>> {
    let key = key?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let protocol = config.kind.protocol();
    if protocol == ProviderProtocol::BedrockConverse {
        return bedrock::discover(&client, config, key).await;
    }
    let vertex = vertex_catalog_url(config);
    let base = if let Some(url) = &vertex {
        url.to_string()
    } else {
        format!(
            "{}/models",
            config.effective_base_url().ok()?.trim_end_matches('/')
        )
    };
    let mut cursor = None::<String>;
    let mut models = BTreeMap::new();
    for _ in 0..10 {
        let mut url = url::Url::parse(&base).ok()?;
        match protocol {
            ProviderProtocol::BedrockConverse => return None,
            ProviderProtocol::AnthropicMessages => {
                url.query_pairs_mut().append_pair("limit", "1000");
                if let Some(cursor) = &cursor {
                    url.query_pairs_mut().append_pair("after_id", cursor);
                }
            }
            ProviderProtocol::GeminiGenerateContent => {
                url.query_pairs_mut().append_pair("pageSize", "1000");
                if let Some(cursor) = &cursor {
                    url.query_pairs_mut().append_pair("pageToken", cursor);
                }
            }
            ProviderProtocol::OpenAiChatCompletions => {
                if let Some(cursor) = &cursor {
                    url.query_pairs_mut().append_pair("after", cursor);
                }
            }
        }
        let request = client.get(url);
        let request = match protocol {
            ProviderProtocol::BedrockConverse => return None,
            ProviderProtocol::AnthropicMessages => request
                .header("x-api-key", key.expose_secret())
                .header("anthropic-version", "2023-06-01"),
            ProviderProtocol::GeminiGenerateContent => {
                if config.auth_header.as_deref() == Some("authorization") {
                    request.bearer_auth(key.expose_secret())
                } else {
                    request.header("x-goog-api-key", key.expose_secret())
                }
            }
            ProviderProtocol::OpenAiChatCompletions => {
                let header = config.auth_header.as_deref().unwrap_or("authorization");
                let scheme = if config.auth_header.is_some() {
                    config.auth_scheme.as_deref()
                } else {
                    Some("Bearer")
                };
                let value = scheme.map_or_else(
                    || key.expose_secret().to_owned(),
                    |scheme| format!("{scheme} {}", key.expose_secret()),
                );
                request.header(header, value)
            }
        };
        let body = read_json(request).await?;
        let rows = body[if vertex.is_some() {
            "publisherModels"
        } else if protocol == ProviderProtocol::GeminiGenerateContent {
            "models"
        } else {
            "data"
        }]
        .as_array()?;
        for entry in rows {
            let model = if vertex.is_some() {
                vertex_model(config, entry)
            } else {
                model_entry(config, entry)
            };
            if let Some(model) = model {
                models.insert(model.id.clone(), model);
            }
        }
        let next = if protocol == ProviderProtocol::GeminiGenerateContent {
            body["nextPageToken"]
                .as_str()
                .filter(|value| !value.is_empty())
        } else if body["has_more"].as_bool().unwrap_or(false) {
            Some(body["last_id"].as_str().filter(|value| !value.is_empty())?)
        } else {
            None
        };
        let Some(next) = next else {
            return Some(models.into_values().collect());
        };
        if cursor.as_deref() == Some(next) {
            return None;
        }
        cursor = Some(next.to_owned());
    }
    None
}

async fn read_json(request: reqwest::RequestBuilder) -> Option<Value> {
    let response = request.send().await.ok()?.error_for_status().ok()?;
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if bytes.len() + chunk.len() > 4 * 1024 * 1024 {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).ok()
}

fn vertex_catalog_url(config: &ProviderConfig) -> Option<url::Url> {
    if config.kind.protocol() != ProviderProtocol::GeminiGenerateContent
        || config.auth_header.as_deref() != Some("authorization")
    {
        return None;
    }
    let mut url = url::Url::parse(&config.effective_base_url().ok()?).ok()?;
    if !url
        .path()
        .trim_end_matches('/')
        .ends_with("/publishers/google")
    {
        return None;
    }
    // Model Garden listing is a publisher resource, not a project-scoped inference resource.
    url.set_path("/v1beta1/publishers/google/models");
    url.set_query(None);
    Some(url)
}

fn vertex_model(config: &ProviderConfig, entry: &Value) -> Option<ModelObject> {
    let id = entry["name"]
        .as_str()?
        .strip_prefix("publishers/google/models/")?;
    // Other Model Garden entries require deployment or a different inference protocol.
    if !id.starts_with("gemini-") || id.contains('/') || id.contains('@') {
        return None;
    }
    model_entry(
        config,
        &json!({"name":format!("models/{id}"), "displayName":entry["displayName"].as_str().unwrap_or(id), "supportedGenerationMethods":["generateContent"]}),
    )
}

fn model_entry(config: &ProviderConfig, entry: &Value) -> Option<ModelObject> {
    let protocol = config.kind.protocol();
    let id = if protocol == ProviderProtocol::GeminiGenerateContent {
        if !entry["supportedGenerationMethods"]
            .as_array()?
            .iter()
            .any(|method| method == "generateContent")
        {
            return None;
        }
        entry["name"].as_str()?.strip_prefix("models/")?
    } else {
        entry["id"].as_str()?
    };
    if id.is_empty() {
        return None;
    }
    // These APIs also list embeddings, media generation, realtime, and
    // non-chat deployments. They cannot serve this chat-completions API.
    if config.kind.is_native_openai() && !openai_chat_model(id) {
        return None;
    }
    if protocol == ProviderProtocol::GeminiGenerateContent
        && (!(id.starts_with("gemini-") || id.starts_with("gemma-"))
            || [
                "image",
                // Image models named after the brand, e.g. gemini-nano-banana-2.1.
                "nano-banana",
                "embedding",
                "tts",
                "transcribe",
                "native-audio",
                "live",
                "robotics",
                "computer-use",
            ]
            .iter()
            .any(|part| id.contains(part)))
    {
        return None;
    }
    let mut metadata: BTreeMap<String, Value> = entry
        .as_object()?
        .iter()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "id" | "object" | "created" | "owned_by" | "name" | "type"
            )
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if protocol == ProviderProtocol::AnthropicMessages {
        copy(entry, "max_input_tokens", &mut metadata, "context_window");
        copy(entry, "max_tokens", &mut metadata, "max_output_tokens");
        if let Some(value) = entry.pointer("/capabilities/image_input/supported") {
            metadata.insert("supports_vision".into(), value.clone());
        }
        if let Some(value) = entry.pointer("/capabilities/thinking/supported") {
            metadata.insert("supports_reasoning".into(), value.clone());
        }
        let efforts: Vec<_> = ["low", "medium", "high", "xhigh", "max"]
            .into_iter()
            .filter(|effort| {
                entry.pointer(&format!("/capabilities/effort/{effort}/supported"))
                    == Some(&Value::Bool(true))
            })
            .collect();
        if !efforts.is_empty() {
            metadata
                .entry("reasoning_efforts".into())
                .or_insert(json!(efforts));
        } else if entry.pointer("/capabilities/thinking/supported") == Some(&json!(true))
            && crate::anthropic::uses_manual_thinking(id)
        {
            // Older Claude models expose thinking budgets, not native effort.
            metadata
                .entry("reasoning_efforts".into())
                .or_insert(json!(["low", "medium", "high"]));
        }
        metadata.insert("supports_tools".into(), json!(true));
    } else if protocol == ProviderProtocol::GeminiGenerateContent {
        copy(entry, "displayName", &mut metadata, "display_name");
        copy(entry, "inputTokenLimit", &mut metadata, "context_window");
        copy(
            entry,
            "outputTokenLimit",
            &mut metadata,
            "max_output_tokens",
        );
        copy(entry, "thinking", &mut metadata, "supports_reasoning");
        if id.starts_with("gemini-") {
            metadata.insert("supports_tools".into(), json!(true));
            metadata.insert("supports_vision".into(), json!(true));
        }
        if entry["thinking"]
            .as_bool()
            .unwrap_or(id.starts_with("gemini-3") || id.starts_with("gemini-2.5"))
        {
            metadata
                .entry("supports_reasoning".into())
                .or_insert(json!(true));
            metadata
                .entry("reasoning_efforts".into())
                .or_insert_with(|| json!(gemini_reasoning_efforts(id)));
        }
    } else if config.kind.is_native_openai() {
        let efforts = openai_reasoning_efforts(id);
        let supports_reasoning = metadata
            .get("reasoning_efforts")
            .and_then(Value::as_array)
            .map_or(!efforts.is_empty(), |values| !values.is_empty());
        metadata
            .entry("supports_reasoning".into())
            .or_insert(json!(supports_reasoning));
        if id.starts_with("gpt-5") || id.starts_with("gpt-6") {
            metadata
                .entry("supports_vision".into())
                .or_insert(json!(true));
            metadata
                .entry("supports_tools".into())
                .or_insert(json!(true));
        }
        metadata
            .entry("reasoning_efforts".into())
            .or_insert_with(|| json!(efforts));
    }
    if metadata.get("supports_reasoning") == Some(&json!(false)) {
        metadata.insert("reasoning_efforts".into(), json!([]));
    }
    Some(ModelObject {
        id: id.into(),
        object: "model".into(),
        created: entry["created"].as_u64().unwrap_or(0),
        owned_by: String::new(),
        metadata,
    })
}

fn copy(entry: &Value, from: &str, metadata: &mut BTreeMap<String, Value>, to: &str) {
    if let Some(value) = entry.get(from) {
        metadata.insert(to.into(), value.clone());
    }
}

fn openai_chat_model(id: &str) -> bool {
    (id.starts_with("gpt-")
        || id.starts_with("chatgpt-")
        || id.starts_with("o1")
        || id.starts_with("o3")
        || id.starts_with("o4"))
        && ![
            "-realtime",
            "-audio",
            "-transcribe",
            "-search",
            "-deep-research",
            "-image",
            "-instruct",
            "-tts",
        ]
        .iter()
        .any(|part| id.contains(part))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_efforts_cover_model_families_and_respect_live_overrides() {
        let config: ProviderConfig = serde_json::from_value(json!({"kind":"openai"})).unwrap();
        for (id, expected) in [
            ("gpt-6-astra", vec!["low", "medium", "high", "xhigh", "max"]),
            // Probed 2026-10-10: GPT-6.1 Sol answers 400 to `none`.
            ("gpt-6.1-sol", vec!["low", "medium", "high", "xhigh", "max"]),
            (
                "gpt-6-sol",
                vec!["none", "low", "medium", "high", "xhigh", "max"],
            ),
            (
                "gpt-5.6-luna",
                vec!["none", "low", "medium", "high", "xhigh", "max"],
            ),
            ("gpt-5.5", vec!["none", "low", "medium", "high", "xhigh"]),
            ("gpt-5.1", vec!["none", "low", "medium", "high"]),
            ("gpt-5-mini", vec!["minimal", "low", "medium", "high"]),
            ("gpt-5-2025-08-07", vec!["minimal", "low", "medium", "high"]),
            ("gpt-5.1-2025-11-13", vec!["none", "low", "medium", "high"]),
            ("gpt-5-1", vec!["none", "low", "medium", "high"]),
            ("gpt-5-pro", vec!["high"]),
            ("gpt-5.2-pro", vec!["medium", "high", "xhigh"]),
            ("o3", vec!["low", "medium", "high"]),
            ("gpt-4.1", vec![]),
            ("gpt-5-chat-latest", vec![]),
        ] {
            let entry = model_entry(&config, &json!({"id":id})).unwrap();
            assert_eq!(entry.metadata["reasoning_efforts"], json!(expected), "{id}");
        }
        let live = model_entry(
            &config,
            &json!({"id":"gpt-6-astra", "reasoning_efforts":["low","max"]}),
        )
        .unwrap();
        assert_eq!(live.metadata["reasoning_efforts"], json!(["low", "max"]));
        let future = model_entry(
            &config,
            &json!({"id":"gpt-future", "reasoning_efforts":["low","max"]}),
        )
        .unwrap();
        assert_eq!(future.metadata["supports_reasoning"], true);
        assert_eq!(future.metadata["reasoning_efforts"], json!(["low", "max"]));
        let disabled = model_entry(
            &config,
            &json!({"id":"gpt-6-astra", "supports_reasoning":false}),
        )
        .unwrap();
        assert_eq!(disabled.metadata["reasoning_efforts"], json!([]));
        let legacy = model_entry(&config, &json!({"id":"gpt-3.5-turbo"})).unwrap();
        assert!(!legacy.metadata.contains_key("supports_vision"));
        let text_only = model_entry(
            &config,
            &json!({"id":"gpt-6-astra", "supports_vision":false}),
        )
        .unwrap();
        assert_eq!(text_only.metadata["supports_vision"], false);
    }

    #[test]
    fn claude_native_efforts_and_older_thinking_budgets_are_exposed() {
        let config: ProviderConfig = serde_json::from_value(json!({"kind":"anthropic"})).unwrap();
        let live = model_entry(&config, &json!({"id":"claude-future", "capabilities":{"thinking":{"supported":true},"effort":{"low":{"supported":true},"medium":{"supported":false},"max":{"supported":true}}}})).unwrap();
        assert_eq!(live.metadata["reasoning_efforts"], json!(["low", "max"]));
        let manual = model_entry(&config, &json!({"id":"claude-haiku-4-5-20251001", "capabilities":{"thinking":{"supported":true}}})).unwrap();
        assert_eq!(
            manual.metadata["reasoning_efforts"],
            json!(["low", "medium", "high"])
        );
    }
    use router_core::ProviderKind;

    #[test]
    fn new_chat_families_are_not_limited_to_a_fixed_model_allowlist() {
        for id in [
            "gpt-6-astra",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-7-future",
            "gpt-5.5-pro",
            "gpt-5.6-codex",
        ] {
            assert!(openai_chat_model(id));
        }
        for id in [
            "gpt-image-1",
            "gpt-realtime",
            "text-embedding-3-small",
            "o3-deep-research",
        ] {
            assert!(!openai_chat_model(id));
        }
    }

    #[test]
    fn preserves_live_model_metadata_and_filters_non_chat_gemini_models() {
        let config = ProviderConfig {
            kind: ProviderKind::Gemini,
            ..serde_json::from_value(json!({"kind":"gemini"})).unwrap()
        };
        let model = model_entry(&config, &json!({"name":"models/gemini-3.8-flash","displayName":"Gemini 3.8 Flash","inputTokenLimit":1_048_576,"outputTokenLimit":65_536,"thinking":true,"supportedGenerationMethods":["generateContent"]})).unwrap();
        assert_eq!(model.id, "gemini-3.8-flash");
        assert_eq!(model.metadata["context_window"], 1_048_576);
        assert_eq!(
            model.metadata["reasoning_efforts"],
            json!(["low", "medium", "high"])
        );
        for id in [
            "veo-future",
            "gemini-nano-banana-2.1",
            "gemini-3.1-flash-image",
        ] {
            assert!(
                model_entry(&config, &json!({"name":format!("models/{id}"),"supportedGenerationMethods":["generateContent"]})).is_none(),
                "{id}"
            );
        }
        assert!(model_entry(&config, &json!({"name":"models/gemini-embedding-2","supportedGenerationMethods":["embedContent"]})).is_none());
    }
}

fn gpt5_minor(id: &str) -> Option<u32> {
    let rest = id.strip_prefix("gpt-5")?;
    let digits: String = rest
        .strip_prefix(['.', '-'])?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    // A four-digit hyphenated suffix starts a snapshot date, not a minor version.
    if rest.starts_with('-') && digits.len() == 4 {
        return None;
    }
    digits.parse().ok()
}

/// Reasoning tiers the public Responses API accepts per OpenAI model family,
/// lightest first. GPT-5.1 replaced `minimal` with `none`, 5.2 added `xhigh`,
/// 5.6 added `max`, GPT-6 Astra and GPT-6.1 Sol dropped `none`; GPT-6 Sol/Luna retain it. Codex, Pro and chat variants keep
/// their own narrower sets. A request outside the set is a 400.
fn openai_reasoning_efforts(id: &str) -> Vec<&'static str> {
    let lower = id.to_ascii_lowercase();
    // A fine-tune is `ft:<family>:<org>:<suffix>`; only the family matters.
    let family = lower.strip_prefix("ft:").unwrap_or(&lower);
    let family = family.split(':').next().unwrap_or(family);
    let segment = |name: &str| family.split(['-', '.']).any(|part| part == name);
    if family.contains("deep-research") {
        return vec!["medium"];
    }
    if family.starts_with("gpt-6") {
        if family.starts_with("gpt-6-sol") || family.starts_with("gpt-6-luna") {
            return vec!["none", "low", "medium", "high", "xhigh", "max"];
        }
        return vec!["low", "medium", "high", "xhigh", "max"];
    }
    if !family.starts_with("gpt-5") {
        return if ["o1", "o3", "o4"]
            .iter()
            .any(|prefix| family.starts_with(prefix))
        {
            vec!["low", "medium", "high"]
        } else {
            Vec::new()
        };
    }
    let minor = gpt5_minor(family);
    if segment("chat") {
        return Vec::new();
    }
    if segment("pro") {
        return if minor.is_some() {
            vec!["medium", "high", "xhigh"]
        } else {
            vec!["high"]
        };
    }
    if segment("codex") {
        return match minor {
            Some(version) if version >= 3 => vec!["none", "low", "medium", "high", "xhigh"],
            Some(version) if version >= 2 => vec!["low", "medium", "high", "xhigh"],
            _ if family.contains("codex-max") => vec!["low", "medium", "high", "xhigh"],
            _ => vec!["low", "medium", "high"],
        };
    }
    match minor {
        None => vec!["minimal", "low", "medium", "high"],
        Some(1) => vec!["none", "low", "medium", "high"],
        Some(version) if version >= 6 => vec!["none", "low", "medium", "high", "xhigh", "max"],
        Some(_) => vec!["none", "low", "medium", "high", "xhigh"],
    }
}

/// Model-family fallback only; provider-advertised capabilities take precedence.
pub(crate) fn gemini_reasoning_efforts(id: &str) -> Vec<&'static str> {
    if id.starts_with("gemini-3.8-flash")
        || id.starts_with("gemini-3.7-flash")
        || id.starts_with("gemini-3.1-pro")
    {
        vec!["low", "medium", "high"]
    } else if id.starts_with("gemini-3-pro") {
        vec!["low", "high"]
    } else if id.starts_with("gemini-3") {
        vec!["minimal", "low", "medium", "high"]
    } else if id.starts_with("gemini-2.5-pro") {
        vec!["low", "medium", "high"]
    } else if id.starts_with("gemini-2.5") {
        vec!["none", "low", "medium", "high"]
    } else {
        Vec::new()
    }
}
