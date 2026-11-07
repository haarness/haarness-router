use std::{collections::BTreeMap, time::Duration};

use futures::StreamExt;
use router_core::{ProviderConfig, ProviderKind, ProviderProtocol};
use router_protocol::ModelObject;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

mod bedrock;
#[cfg(test)]
mod discovery_tests;

/// Kinds of model Gemini lists that cannot answer a chat, matched as whole
/// segments of the id. Image models also go by the brand `nano-banana`.
const GEMINI_NON_CHAT: [&str; 12] = [
    "image",
    "nano-banana",
    "imagen",
    "veo",
    "lyria",
    "embedding",
    "tts",
    "transcribe",
    "native-audio",
    "live",
    "robotics",
    "computer-use",
];

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
    if vertex_publisher(config) && crate::gemini::google_api_key(key.expose_secret()) {
        return vertex_key_models(&client, config, key).await;
    }
    let vertex = vertex_catalog_url(config, key);
    // xAI's `/models` mixes its image and video generators in with its chat
    // models and says nothing of their input; `/language-models` lists only
    // the chat models, with their input and reasoning levels.
    let xai = config.kind == ProviderKind::Xai;
    let base = if let Some(url) = &vertex {
        url.to_string()
    } else {
        format!(
            "{}/{}",
            config.effective_base_url().ok()?.trim_end_matches('/'),
            if xai { "language-models" } else { "models" }
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
                if crate::gemini::sends_bearer(config, key.expose_secret()) {
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
        } else if protocol == ProviderProtocol::GeminiGenerateContent || xai {
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

/// A Gemini provider whose base URL is a Vertex AI publisher path, with or
/// without a project: `…/v1/publishers/google` or
/// `…/v1/projects/<project>/locations/<location>/publishers/google`.
fn vertex_publisher(config: &ProviderConfig) -> bool {
    config.kind.protocol() == ProviderProtocol::GeminiGenerateContent
        && config.effective_base_url().is_ok_and(|base| {
            url::Url::parse(&base).is_ok_and(|url| {
                url.path()
                    .trim_end_matches('/')
                    .ends_with("/publishers/google")
            })
        })
}

fn vertex_catalog_url(config: &ProviderConfig, key: &SecretString) -> Option<url::Url> {
    if !vertex_publisher(config) || !crate::gemini::sends_bearer(config, key.expose_secret()) {
        return None;
    }
    let mut url = url::Url::parse(&config.effective_base_url().ok()?).ok()?;
    // Model Garden listing is a publisher resource, not a project-scoped inference resource.
    url.set_path("/v1beta1/publishers/google/models");
    url.set_query(None);
    Some(url)
}

/// Vertex's Model Garden listing takes only OAuth access tokens, so with an
/// API key the router cannot read the catalog. These are the Gemini chat
/// models Vertex serves; each is kept only when the key's project reaches it,
/// which the free `countTokens` call answers. Models missing here can still be
/// requested by ID.
const VERTEX_KEY_MODELS: [&str; 12] = [
    "gemini-4-argon",
    "gemini-3.8-flash",
    "gemini-3.7-flash",
    "gemini-3.6-flash",
    "gemini-3.5-flash",
    "gemini-3.5-flash-lite",
    "gemini-3.1-pro-preview",
    "gemini-3.1-flash-lite",
    "gemini-3-flash-preview",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
];

async fn vertex_key_models(
    client: &reqwest::Client,
    config: &ProviderConfig,
    key: &SecretString,
) -> Option<Vec<ModelObject>> {
    let base = config.effective_base_url().ok()?;
    let base = base.trim_end_matches('/');
    let probe = json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]});
    let found = futures::future::join_all(VERTEX_KEY_MODELS.iter().map(|id| {
        let request = client
            .post(format!("{base}/models/{id}:countTokens"))
            .header("x-goog-api-key", key.expose_secret())
            .json(&probe);
        async move {
            let response = request.send().await.ok()?;
            response.status().is_success().then_some(*id)
        }
    }))
    .await;
    let models: Vec<_> = found
        .into_iter()
        .flatten()
        .filter_map(|id| {
            model_entry(
                config,
                &json!({"name":format!("models/{id}"), "displayName":id, "supportedGenerationMethods":["generateContent"]}),
            )
        })
        .collect();
    (!models.is_empty()).then_some(models)
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
    // OpenRouter lists batch-only variants (no chat route in `endpoints`)
    // and image and audio generators beside its chat models; its routers
    // (`openrouter/auto`) answer chats in text.
    let chat_route = entry["endpoints"].as_array().is_none_or(|routes| {
        routes.iter().any(|route| {
            route
                .as_str()
                .is_some_and(|route| route.ends_with("/chat/completions"))
        })
    });
    let media_output = !id.starts_with("openrouter/")
        && entry
            .pointer("/architecture/output_modalities")
            .and_then(Value::as_array)
            .is_some_and(|modalities| {
                modalities
                    .iter()
                    .any(|modality| matches!(modality.as_str(), Some("image" | "audio")))
            });
    if !chat_route || media_output {
        return None;
    }
    // Gemini lists media generators, embeddings, speech and live-audio
    // models beside its chat models, some of them with `generateContent`.
    if protocol == ProviderProtocol::GeminiGenerateContent
        && GEMINI_NON_CHAT.iter().any(|word| names(id, word))
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
        if entry["thinking"].as_bool().unwrap_or(
            gemini_generation(id).is_some_and(|generation| generation >= 3)
                || id.starts_with("gemini-2.5"),
        ) {
            metadata
                .entry("supports_reasoning".into())
                .or_insert(json!(true));
            // A model the catalog says thinks keeps its levels even when its
            // name is new to the table (`gemini-flash-latest`, a new family).
            let known = gemini_reasoning_efforts(id);
            metadata
                .entry("reasoning_efforts".into())
                .or_insert_with(|| {
                    json!(if known.is_empty() {
                        GEMINI_NEWEST_EFFORTS.to_vec()
                    } else {
                        known
                    })
                });
        }
    } else if config.kind == ProviderKind::Xai {
        // Its multi-agent models answer only on the Responses API.
        if id.contains("multi-agent") {
            return None;
        }
        if let Some(inputs) = entry["input_modalities"].as_array() {
            metadata.insert(
                "supports_vision".into(),
                json!(inputs.iter().any(|input| input == "image")),
            );
        }
        let efforts = entry
            .pointer("/capabilities/reasoning_effort")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // A model that reasons at a fixed depth says so in its id only.
        let fixed = id.ends_with("-reasoning") && !id.ends_with("-non-reasoning");
        metadata
            .entry("supports_reasoning".into())
            .or_insert(json!(fixed || !efforts.is_empty()));
        metadata
            .entry("reasoning_efforts".into())
            .or_insert(json!(efforts));
        copy(entry, "context_length", &mut metadata, "context_window");
        metadata.insert("supports_tools".into(), json!(true));
    } else if config.kind == ProviderKind::Mistral {
        // Mistral lists embeddings, OCR, moderation, speech and transcription
        // beside its chat models, and says which is which.
        if entry.pointer("/capabilities/completion_chat") == Some(&json!(false)) {
            return None;
        }
        for (from, to) in [
            ("/capabilities/function_calling", "supports_tools"),
            ("/capabilities/vision", "supports_vision"),
            ("/capabilities/reasoning", "supports_reasoning"),
        ] {
            if let Some(value) = entry.pointer(from).filter(|value| value.is_boolean()) {
                metadata.insert(to.into(), value.clone());
            }
        }
        copy(entry, "max_context_length", &mut metadata, "context_window");
        known_efforts(config, id, &mut metadata);
    } else if config.kind == ProviderKind::Zai {
        // Z.ai's catalog names its models and nothing more. Every GLM chat
        // model calls tools and thinks; only the Flash models read images.
        metadata.insert("supports_tools".into(), json!(true));
        metadata.insert("supports_reasoning".into(), json!(true));
        metadata.insert("supports_vision".into(), json!(id.contains("-flash")));
        known_efforts(config, id, &mut metadata);
    } else if config.kind == ProviderKind::Groq {
        // Groq lists speech (`orpheus`), transcription (`whisper`) and
        // prompt-classifier models beside its chat models.
        let speaks_text = entry["output_modalities"]
            .as_array()
            .is_none_or(|outputs| outputs.iter().any(|output| output == "text"));
        if !speaks_text || id.contains("prompt-guard") {
            return None;
        }
        let features = entry["supported_features"].as_array();
        let has = |feature: &str| features.is_some_and(|all| all.iter().any(|f| f == feature));
        metadata.insert("supports_tools".into(), json!(has("tools")));
        metadata.insert("supports_reasoning".into(), json!(has("reasoning")));
        if let Some(inputs) = entry["input_modalities"].as_array() {
            metadata.insert(
                "supports_vision".into(),
                json!(inputs.iter().any(|input| input == "image")),
            );
        }
        copy(
            entry,
            "max_completion_tokens",
            &mut metadata,
            "max_output_tokens",
        );
        known_efforts(config, id, &mut metadata);
    } else if config.kind.is_native_openai() {
        let efforts = openai_reasoning_efforts(id);
        let supports_reasoning = metadata
            .get("reasoning_efforts")
            .and_then(Value::as_array)
            .map_or(!efforts.is_empty(), |values| !values.is_empty());
        metadata
            .entry("supports_reasoning".into())
            .or_insert(json!(supports_reasoning));
        if openai_reads_images(id) {
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
    } else if let Some(inputs) = entry
        .pointer("/architecture/input_modalities")
        .and_then(Value::as_array)
    {
        // OpenRouter says what each model reads.
        metadata
            .entry("supports_vision".into())
            .or_insert(json!(inputs.iter().any(|input| input == "image")));
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

/// The levels a thinking model takes, where the router knows them.
fn known_efforts(config: &ProviderConfig, id: &str, metadata: &mut BTreeMap<String, Value>) {
    let efforts = crate::openai_compatible::compatible_reasoning_efforts(config.kind, id);
    if metadata.get("supports_reasoning") == Some(&json!(true)) && !efforts.is_empty() {
        metadata
            .entry("reasoning_efforts".into())
            .or_insert(json!(efforts));
    }
}

fn copy(entry: &Value, from: &str, metadata: &mut BTreeMap<String, Value>, to: &str) {
    if let Some(value) = entry.get(from) {
        metadata.insert(to.into(), value.clone());
    }
}

/// Whether `id` names `word` in whole segments: `gpt-realtime` names
/// `realtime` and `o3-deep-research` names `deep-research`, while
/// `gemini-deliverable` names no `live`.
fn names(id: &str, word: &str) -> bool {
    let id = id.to_ascii_lowercase();
    let parts: Vec<&str> = id.split(['-', '.', '_', ':', '/']).collect();
    let wanted: Vec<&str> = word.split('-').collect();
    parts
        .windows(wanted.len())
        .any(|window| window == wanted.as_slice())
}

/// Kinds of model `OpenAI` lists beside its chat models: speech, transcription,
/// embeddings, moderation, image and video generation, the realtime and
/// search-only variants, deep research and the completions-only models.
const OPENAI_NON_CHAT: [&str; 16] = [
    "audio",
    "realtime",
    "transcribe",
    "tts",
    "whisper",
    "embedding",
    "moderation",
    "image",
    "dall-e",
    "sora",
    "search",
    "deep-research",
    "instruct",
    "davinci",
    "babbage",
    "computer-use",
];

/// Everything `OpenAI` lists is a chat model unless its id names a kind that
/// is not, so a new family (`o5`, `gpt-7`) shows up without a release.
fn openai_chat_model(id: &str) -> bool {
    let family = openai_family(id);
    !OPENAI_NON_CHAT.iter().any(|word| names(&family, word))
}

/// The base model of an id, lowercase: a fine-tune is
/// `ft:<family>:<org>:<suffix>` and only the family says what it is.
fn openai_family(id: &str) -> String {
    let lower = id.to_ascii_lowercase();
    let family = lower.strip_prefix("ft:").unwrap_or(&lower);
    family.split(':').next().unwrap_or(family).to_owned()
}

/// A GPT id's version: `(5, None)` for `gpt-5-mini` and the snapshot
/// `gpt-5-2025-08-07`, `(5, Some(1))` for `gpt-5.1` and `gpt-5-1`,
/// `(7, None)` for `gpt-7`.
fn gpt_version(family: &str) -> Option<(u32, Option<u32>)> {
    let rest = family.strip_prefix("gpt-")?;
    let major_len = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let major = rest[..major_len].parse().ok()?;
    let rest = &rest[major_len..];
    let minor = rest.strip_prefix(['.', '-']).and_then(|after| {
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        // A four-digit hyphenated suffix starts a snapshot date, not a minor version.
        if rest.starts_with('-') && digits.len() == 4 {
            return None;
        }
        digits.parse().ok()
    });
    Some((major, minor))
}

/// The number of an `o`-series reasoning model: 3 for `o3-mini`, 5 for `o5`.
fn o_series(family: &str) -> Option<u32> {
    let rest = family.strip_prefix('o')?;
    let digits = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if digits == 0 || !(rest[digits..].is_empty() || rest[digits..].starts_with('-')) {
        return None;
    }
    rest[..digits].parse().ok()
}

/// GPT-5 and later, and o4 and later, read images and call tools; a new
/// generation is taken to do the same until its catalog entry says otherwise.
/// So do GPT-4o, GPT-4.1 and GPT-4 Turbo, and o1 and o3 but not their mini or
/// preview models. GPT-4 itself and GPT-3.5 read only text.
fn openai_reads_images(id: &str) -> bool {
    let family = openai_family(id);
    if let Some((major, minor)) = gpt_version(&family) {
        return major >= 5
            || (major == 4
                && (minor.is_some_and(|minor| minor >= 1)
                    || family.starts_with("gpt-4o")
                    || names(&family, "turbo")));
    }
    o_series(&family).is_some_and(|number| {
        number >= 4 || (!names(&family, "mini") && !names(&family, "preview"))
    })
}

/// Reasoning tiers the public Responses API accepts per `OpenAI` model family,
/// lightest first. GPT-5.1 replaced `minimal` with `none`, 5.2 added `xhigh`,
/// 5.6 added `max`, GPT-6 Astra and GPT-6.1 Sol dropped `none`; GPT-6 Sol/Luna retain it. Codex, Pro and chat variants keep
/// their own narrower sets. A generation newer than these gets the newest
/// set, and every `o` model the classic three. A catalog that lists the
/// levels itself wins over this table.
fn openai_reasoning_efforts(id: &str) -> Vec<&'static str> {
    let family = openai_family(id);
    let segment = |name: &str| family.split(['-', '.']).any(|part| part == name);
    if names(&family, "deep-research") {
        return vec!["medium"];
    }
    if o_series(&family).is_some() {
        return vec!["low", "medium", "high"];
    }
    let Some((major, minor)) = gpt_version(&family) else {
        return Vec::new();
    };
    if major < 5 || segment("chat") {
        return Vec::new();
    }
    if major >= 6 {
        if family.starts_with("gpt-6-sol") || family.starts_with("gpt-6-luna") {
            return vec!["none", "low", "medium", "high", "xhigh", "max"];
        }
        return vec!["low", "medium", "high", "xhigh", "max"];
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

/// The generation a Gemini model id names: 3 for `gemini-3.8-flash`, 4 for
/// `gemini-4-argon`, nothing for an alias such as `gemini-flash-latest`.
fn gemini_generation(id: &str) -> Option<u32> {
    let name = id.rsplit('/').next()?.strip_prefix("gemini-")?;
    let digits = name
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(name.len());
    name[..digits].parse().ok()
}

/// The levels of the newest Gemini generations, for a thinking model the
/// table below does not name.
pub(crate) const GEMINI_NEWEST_EFFORTS: [&str; 3] = ["low", "medium", "high"];

/// Model-family fallback only; provider-advertised capabilities take precedence.
pub(crate) fn gemini_reasoning_efforts(id: &str) -> Vec<&'static str> {
    // Probed 2026-10-15: Gemini 4 Argon answers `minimal` with a 400, as
    // 3.7/3.8 Flash and 3.1 Pro do; later generations are assumed to follow.
    if id.starts_with("gemini-3.8-flash")
        || id.starts_with("gemini-3.7-flash")
        || id.starts_with("gemini-3.1-pro")
        || gemini_generation(id).is_some_and(|generation| generation >= 4)
    {
        GEMINI_NEWEST_EFFORTS.to_vec()
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

/// A reasoning level's place on the scale, lightest first. A level the scale
/// does not know (a provider's own `ultra`) ranks strongest.
fn effort_rank(level: &str) -> u8 {
    match level {
        "none" => 0,
        "minimal" => 1,
        "low" => 2,
        "medium" => 3,
        "high" => 4,
        "xhigh" => 5,
        "max" => 6,
        _ => 7,
    }
}

/// The level of `supported` closest to `wanted`: `wanted` itself when listed,
/// otherwise the nearest on the scale, the stronger of two equally near.
pub(crate) fn nearest_effort<'a>(wanted: &'a str, supported: &[&'a str]) -> &'a str {
    if supported.is_empty() || supported.contains(&wanted) {
        return wanted;
    }
    let rank = effort_rank(wanted);
    supported
        .iter()
        .copied()
        .min_by_key(|level| {
            let other = effort_rank(level);
            (rank.abs_diff(other), std::cmp::Reverse(other))
        })
        .unwrap_or(wanted)
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
            "o4-mini-deep-research",
            "gpt-4o-mini-transcribe",
            "gpt-4o-mini-tts",
            "tts-1-hd",
            "whisper-1",
            "dall-e-3",
            "sora-2",
            "omni-moderation-latest",
            "gpt-5-search-api",
            "gpt-3.5-turbo-instruct",
            "davinci-002",
            "gpt-audio-mini",
            "chatgpt-image-latest",
            "computer-use-preview",
        ] {
            assert!(!openai_chat_model(id), "{id}");
        }
    }

    #[test]
    fn an_unknown_new_openai_model_is_listed_with_the_newest_behaviour() {
        let config: ProviderConfig = serde_json::from_value(json!({"kind":"openai"})).unwrap();
        // Families no table names yet, and a name that only contains a
        // non-chat word inside a segment.
        for id in [
            "o5-mini",
            "o5",
            "gpt-7",
            "gpt-7.2-nova",
            "orion-1",
            "gpt-5.6-imagery",
        ] {
            assert!(openai_chat_model(id), "{id}");
        }
        let o5 = model_entry(&config, &json!({"id":"o5-mini"})).unwrap();
        assert_eq!(o5.metadata["supports_reasoning"], true);
        assert_eq!(
            o5.metadata["reasoning_efforts"],
            json!(["low", "medium", "high"])
        );
        assert_eq!(o5.metadata["supports_vision"], true);
        let gpt7 = model_entry(&config, &json!({"id":"gpt-7"})).unwrap();
        assert_eq!(
            gpt7.metadata["reasoning_efforts"],
            json!(["low", "medium", "high", "xhigh", "max"])
        );
        assert_eq!(gpt7.metadata["supports_vision"], true);
        assert_eq!(gpt7.metadata["supports_tools"], true);
        let chat = model_entry(&config, &json!({"id":"gpt-7-chat-latest"})).unwrap();
        assert_eq!(chat.metadata["supports_reasoning"], false);
        // A family nothing describes stays listed, reasoning only when its
        // entry says so.
        let other = model_entry(&config, &json!({"id":"orion-1"})).unwrap();
        assert_eq!(other.metadata["supports_reasoning"], false);
        let said = model_entry(
            &config,
            &json!({"id":"orion-1","reasoning_efforts":["low","ultra"]}),
        )
        .unwrap();
        assert_eq!(said.metadata["supports_reasoning"], true);
        assert_eq!(said.metadata["reasoning_efforts"], json!(["low", "ultra"]));
        // o3-mini reads no images; nothing is claimed for it.
        let o3 = model_entry(&config, &json!({"id":"o3-mini"})).unwrap();
        assert!(!o3.metadata.contains_key("supports_vision"));
    }

    #[test]
    fn openai_models_that_read_images_say_so() {
        let config: ProviderConfig = serde_json::from_value(json!({"kind":"openai"})).unwrap();
        let vision = |id: &str| {
            model_entry(&config, &json!({ "id": id }))
                .unwrap()
                .metadata
                .get("supports_vision")
                .cloned()
        };
        for id in [
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4o-2024-11-20",
            "gpt-4o-mini-2024-07-18",
            "gpt-4.1",
            "gpt-4.1-mini",
            "gpt-4.1-nano",
            "gpt-4-turbo",
            "o1",
            "o3",
            "o3-pro",
            "o4-mini",
            "gpt-5-mini",
        ] {
            assert_eq!(vision(id), Some(json!(true)), "{id}");
        }
        for id in ["gpt-4", "gpt-4-0613", "gpt-3.5-turbo", "o1-mini", "o3-mini"] {
            assert_eq!(vision(id), None, "{id}");
        }
    }

    #[test]
    fn an_unknown_new_gemini_model_is_listed_when_it_generates_content() {
        let config: ProviderConfig = serde_json::from_value(json!({"kind":"gemini"})).unwrap();
        let entry = |id: &str, thinking: Option<bool>| {
            let mut entry = json!({"name":format!("models/{id}"),"supportedGenerationMethods":["generateContent","countTokens"]});
            if let Some(thinking) = thinking {
                entry["thinking"] = json!(thinking);
            }
            model_entry(&config, &entry)
        };
        let next = entry("gemini-5-flash", Some(true)).unwrap();
        assert_eq!(next.metadata["supports_reasoning"], true);
        assert_eq!(
            next.metadata["reasoning_efforts"],
            json!(["low", "medium", "high"])
        );
        assert_eq!(next.metadata["supports_vision"], true);
        // Neither prefix nor table knows these; the catalog says they think.
        let alias = entry("gemini-flash-latest", Some(true)).unwrap();
        assert_eq!(
            alias.metadata["reasoning_efforts"],
            json!(["low", "medium", "high"])
        );
        assert!(entry("learnlm-3.0-flash", None).is_some());
        // Whole segments only: `deliverable` is not `live`.
        assert!(entry("gemini-5-deliverable", Some(false)).is_some());
        for id in [
            "gemini-5-flash-live",
            "gemini-5-flash-native-audio-preview",
            "gemini-5-flash-image",
            "imagen-5",
            "lyria-3",
            "gemini-5-flash-preview-tts",
        ] {
            assert!(entry(id, None).is_none(), "{id}");
        }
        let without =
            json!({"name":"models/gemini-5-embed","supportedGenerationMethods":["embedContent"]});
        assert!(model_entry(&config, &without).is_none());
    }

    #[test]
    fn nearest_effort_keeps_listed_levels_and_moves_others_to_the_closest() {
        assert_eq!(nearest_effort("high", &["low", "high"]), "high");
        assert_eq!(nearest_effort("medium", &["low", "high"]), "high");
        assert_eq!(nearest_effort("none", &["low", "medium"]), "low");
        assert_eq!(nearest_effort("ultra", &["low", "max"]), "max");
        assert_eq!(nearest_effort("max", &["low", "ultra"]), "ultra");
        assert_eq!(nearest_effort("ultra", &[]), "ultra");
    }

    #[test]
    fn openrouter_entries_say_whether_the_model_reads_images() {
        let config: ProviderConfig = serde_json::from_value(
            json!({"kind":"openai-compatible","base_url":"https://openrouter.ai/api/v1"}),
        )
        .unwrap();
        let text = model_entry(&config, &json!({"id":"deepseek/deepseek-v4","architecture":{"input_modalities":["text"],"output_modalities":["text"]}})).unwrap();
        assert_eq!(text.metadata["supports_vision"], false);
        let sees = model_entry(&config, &json!({"id":"x-ai/grok-5","architecture":{"input_modalities":["text","image"],"output_modalities":["text"]}})).unwrap();
        assert_eq!(sees.metadata["supports_vision"], true);
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
        let argon = model_entry(&config, &json!({"name":"models/gemini-4-argon","displayName":"Gemini 4 Argon","inputTokenLimit":1_048_576,"outputTokenLimit":32_768,"thinking":true,"supportedGenerationMethods":["generateContent","countTokens"]})).unwrap();
        assert_eq!(argon.metadata["supports_reasoning"], true);
        assert_eq!(
            argon.metadata["reasoning_efforts"],
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

    #[test]
    fn mistral_and_groq_list_only_chat_models_with_what_each_can_do() {
        let config =
            |kind| -> ProviderConfig { serde_json::from_value(json!({"kind": kind})).unwrap() };
        let mistral = config("mistral");
        // Entries as Mistral's `/models` lists them.
        let entry = |id: &str, chat: bool, reasoning: bool| {
            json!({"id": id, "name": "mistral-large-4", "max_context_length": 1_048_576,
                "capabilities": {"completion_chat": chat, "function_calling": chat, "vision": chat, "reasoning": reasoning}})
        };
        let large = model_entry(&mistral, &entry("mistral-large-4-0", true, true)).unwrap();
        assert_eq!(
            large.metadata["reasoning_efforts"],
            json!(["low", "medium", "high"])
        );
        assert_eq!(large.metadata["supports_vision"], json!(true));
        assert_eq!(large.metadata["supports_tools"], json!(true));
        assert_eq!(large.metadata["context_window"], json!(1_048_576));
        assert!(!large.metadata.contains_key("name"));
        let small = model_entry(&mistral, &entry("mistral-small-latest", true, true)).unwrap();
        assert_eq!(small.metadata["reasoning_efforts"], json!(["none", "high"]));
        let plain = model_entry(&mistral, &entry("ministral-8b-latest", true, false)).unwrap();
        assert_eq!(plain.metadata["reasoning_efforts"], json!([]));
        for id in [
            "mistral-embed",
            "mistral-ocr-latest",
            "voxtral-mini-tts-latest",
            "mistral-moderation-2603",
        ] {
            assert!(
                model_entry(&mistral, &entry(id, false, false)).is_none(),
                "{id}"
            );
        }
        let zai = config("zai");
        // Z.ai lists ids only, as `{"id":"glm-5.3","object":"model","owned_by":"z-ai"}`.
        let glm = |id: &str| {
            model_entry(
                &zai,
                &json!({"id": id, "object": "model", "owned_by": "z-ai"}),
            )
            .unwrap()
        };
        assert_eq!(
            glm("glm-5.3").metadata["reasoning_efforts"],
            json!(["low", "high", "max"])
        );
        assert_eq!(glm("glm-5.3").metadata["supports_vision"], json!(false));
        assert_eq!(
            glm("glm-5.3-flashx").metadata["supports_vision"],
            json!(true)
        );
        assert_eq!(
            glm("glm-4.7").metadata["reasoning_efforts"],
            json!(["none", "high"])
        );
        assert_eq!(glm("glm-4.7").metadata["supports_tools"], json!(true));
        let groq = config("groq");
        let entry = |id: &str, input: &[&str], output: &str, features: Value| {
            json!({"id": id, "input_modalities": input, "output_modalities": [output],
                "supported_features": features, "max_completion_tokens": 65_536})
        };
        let oss = model_entry(
            &groq,
            &entry(
                "openai/gpt-oss-120b",
                &["text"],
                "text",
                json!(["tools", "reasoning"]),
            ),
        )
        .unwrap();
        assert_eq!(oss.metadata["supports_reasoning"], json!(true));
        assert_eq!(
            oss.metadata["reasoning_efforts"],
            json!(["low", "medium", "high"])
        );
        assert_eq!(oss.metadata["supports_vision"], json!(false));
        assert_eq!(oss.metadata["max_output_tokens"], json!(65_536));
        let qwen = model_entry(
            &groq,
            &entry(
                "qwen/qwen3.8-27b",
                &["text", "image"],
                "text",
                json!(["tools", "reasoning"]),
            ),
        )
        .unwrap();
        assert_eq!(qwen.metadata["supports_vision"], json!(true));
        let allam = model_entry(
            &groq,
            &entry("allam-2-7b", &["text"], "text", json!(["json_mode"])),
        )
        .unwrap();
        assert_eq!(allam.metadata["supports_tools"], json!(false));
        assert_eq!(allam.metadata["reasoning_efforts"], json!([]));
        for (id, input, output) in [
            ("whisper-large-v3-turbo", "audio", "transcription"),
            ("canopylabs/orpheus-v1-english", "text", "speech"),
            ("meta-llama/llama-prompt-guard-2-86m", "text", "text"),
        ] {
            assert!(
                model_entry(&groq, &entry(id, &[input], output, Value::Null)).is_none(),
                "{id}"
            );
        }
    }

    #[test]
    fn xai_models_say_their_input_and_reasoning_and_multi_agent_stays_out() {
        let config = ProviderConfig {
            kind: ProviderKind::Xai,
            base_url: None,
            api_key_env: None,
            auth_header: None,
            auth_scheme: None,
            capabilities: None,
            allow_unknown_fields: false,
            allow_insecure_http: false,
            connect_timeout_ms: 5_000,
            stream_idle_timeout_ms: 60_000,
            max_error_body_bytes: 4_096,
        };
        // Entries as xAI's `/language-models` lists them.
        let leveled = model_entry(&config, &json!({"id": "grok-4.3", "input_modalities": ["text", "image"],
            "context_length": 1_000_000,
            "capabilities": {"reasoning_effort": ["none", "low", "medium", "high", "xhigh"], "default_reasoning_effort": "low"}}))
        .unwrap();
        assert_eq!(leveled.metadata["supports_vision"], json!(true));
        assert_eq!(leveled.metadata["supports_reasoning"], json!(true));
        assert_eq!(
            leveled.metadata["reasoning_efforts"],
            json!(["none", "low", "medium", "high", "xhigh"])
        );
        assert_eq!(leveled.metadata["context_window"], json!(1_000_000));
        assert_eq!(leveled.metadata["supports_tools"], json!(true));
        let fixed = model_entry(
            &config,
            &json!({"id": "grok-4.20-0309-reasoning", "input_modalities": ["text", "image"]}),
        )
        .unwrap();
        assert_eq!(fixed.metadata["supports_reasoning"], json!(true));
        assert_eq!(fixed.metadata["reasoning_efforts"], json!([]));
        let plain = model_entry(
            &config,
            &json!({"id": "grok-4.20-0309-non-reasoning", "input_modalities": ["text"]}),
        )
        .unwrap();
        assert_eq!(plain.metadata["supports_reasoning"], json!(false));
        assert_eq!(plain.metadata["supports_vision"], json!(false));
        assert!(model_entry(&config, &json!({"id": "grok-4.20-multi-agent-0309"})).is_none());
    }

    #[test]
    fn openrouter_lists_only_models_that_answer_a_chat() {
        let config = ProviderConfig {
            kind: router_core::ProviderKind::OpenaiCompatible,
            base_url: Some("https://openrouter.ai/api/v1".into()),
            api_key_env: None,
            auth_header: None,
            auth_scheme: None,
            capabilities: None,
            allow_unknown_fields: false,
            allow_insecure_http: false,
            connect_timeout_ms: 5_000,
            stream_idle_timeout_ms: 60_000,
            max_error_body_bytes: 4_096,
        };
        let kept = |entry: Value| model_entry(&config, &entry).map(|model| model.id);
        assert!(kept(json!({"id": "x-ai/grok-4.7", "endpoints": ["/api/v1/chat/completions"], "architecture": {"output_modalities": ["text"]}})).is_some());
        assert!(kept(json!({"id": "z-ai/glm-5.3-flash:batch", "endpoints": []})).is_none());
        assert!(kept(json!({"id": "google/gemini-3-pro-image", "architecture": {"output_modalities": ["image", "text"]}})).is_none());
        assert!(kept(json!({"id": "openrouter/auto", "architecture": {"output_modalities": ["text", "image"]}})).is_some());
        // Other compatible servers list neither field.
        assert!(kept(json!({"id": "local-model"})).is_some());
    }
}
