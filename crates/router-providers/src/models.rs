use std::{collections::BTreeMap, time::Duration};

use futures::StreamExt;
use router_core::{ProviderConfig, ProviderProtocol};
use router_protocol::ModelObject;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

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
    let base = format!(
        "{}/models",
        config.effective_base_url().ok()?.trim_end_matches('/')
    );
    let mut cursor = None::<String>;
    let mut models = BTreeMap::new();
    for _ in 0..10 {
        let mut url = url::Url::parse(&base).ok()?;
        match protocol {
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
            ProviderProtocol::AnthropicMessages => request
                .header("x-api-key", key.expose_secret())
                .header("anthropic-version", "2023-06-01"),
            ProviderProtocol::GeminiGenerateContent => {
                request.header("x-goog-api-key", key.expose_secret())
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
        let body: Value = serde_json::from_slice(&bytes).ok()?;
        let rows = body[if protocol == ProviderProtocol::GeminiGenerateContent {
            "models"
        } else {
            "data"
        }]
        .as_array()?;
        for entry in rows {
            if let Some(model) = model_entry(config, entry) {
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
            metadata.insert("reasoning_efforts".into(), json!(efforts));
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
        // Google models list thinking support, but not accepted effort levels.
        // Only advertise the portable levels for Gemini 3's native thinkingLevel.
        if id.starts_with("gemini-3") && entry["thinking"] == true {
            metadata.insert("reasoning_efforts".into(), json!(["low", "high"]));
        }
    } else if config.kind.is_native_openai() && (id.starts_with("gpt-5") || id.starts_with("gpt-6"))
    {
        metadata.insert("supports_reasoning".into(), json!(true));
        metadata.insert("supports_vision".into(), json!(true));
        metadata.insert("supports_tools".into(), json!(true));
        if !id.contains("-pro") && !id.contains("-chat") {
            metadata
                .entry("reasoning_efforts".into())
                .or_insert_with(|| {
                    if id.starts_with("gpt-6") {
                        json!(["low", "medium", "high", "xhigh", "max"])
                    } else {
                        json!(["low", "medium", "high"])
                    }
                });
        }
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
        assert_eq!(model.metadata["reasoning_efforts"], json!(["low", "high"]));
        assert!(model_entry(&config, &json!({"name":"models/veo-future","supportedGenerationMethods":["generateContent"]})).is_none());
        assert!(model_entry(&config, &json!({"name":"models/gemini-embedding-2","supportedGenerationMethods":["embedContent"]})).is_none());
    }
}
