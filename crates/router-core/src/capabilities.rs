use std::{collections::BTreeSet, net::IpAddr};

use router_protocol::{ChatCompletionRequest, MessageContent};
use serde::{Deserialize, Serialize};

use crate::{ProviderKind, RouterError, config::CapabilityProfile};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct Capabilities {
    #[serde(default)]
    pub text: bool,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub images: bool,
    #[serde(default)]
    pub input_audio: bool,
    #[serde(default)]
    pub pdf: bool,
    #[serde(default)]
    pub pdf_text_extraction: bool,
    #[serde(default)]
    pub video: bool,
    #[serde(default)]
    pub function_tools: bool,
    #[serde(default)]
    pub custom_tools: bool,
    #[serde(default)]
    pub parallel_tools: bool,
    #[serde(default)]
    pub structured_outputs: bool,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub opaque_reasoning: bool,
    #[serde(default)]
    pub logprobs: bool,
    #[serde(default)]
    pub seed: bool,
    #[serde(default)]
    pub multiple_choices: bool,
    #[serde(default)]
    pub audio_output: bool,
}

impl Capabilities {
    #[must_use]
    pub fn for_kind(kind: ProviderKind) -> Self {
        match kind.capability_profile() {
            CapabilityProfile::OpenAi => Self {
                text: true,
                streaming: true,
                images: true,
                input_audio: true,
                function_tools: true,
                custom_tools: true,
                opaque_reasoning: true,
                parallel_tools: true,
                structured_outputs: true,
                reasoning: true,
                pdf_text_extraction: true,
                logprobs: true,
                seed: true,
                multiple_choices: true,
                audio_output: true,
                ..Self::default()
            },
            CapabilityProfile::Anthropic => Self {
                text: true,
                streaming: true,
                images: true,
                pdf: true,
                function_tools: true,
                parallel_tools: true,
                structured_outputs: true,
                reasoning: true,
                opaque_reasoning: true,
                pdf_text_extraction: true,
                ..Self::default()
            },
            CapabilityProfile::Gemini => Self {
                text: true,
                streaming: true,
                images: true,
                input_audio: true,
                pdf: true,
                video: true,
                function_tools: true,
                parallel_tools: true,
                structured_outputs: true,
                reasoning: true,
                opaque_reasoning: true,
                pdf_text_extraction: true,
                ..Self::default()
            },
            // DeepSeek V4 thinks at chosen levels (`reasoning_effort`) and its
            // Flash model reads images; its reasoning text is not opaque.
            CapabilityProfile::DeepSeek => Self {
                text: true,
                streaming: true,
                images: true,
                function_tools: true,
                reasoning: true,
                pdf_text_extraction: true,
                ..Self::default()
            },
            CapabilityProfile::ConservativeOpenAiCompatible => Self {
                text: true,
                streaming: true,
                function_tools: true,
                pdf_text_extraction: true,
                ..Self::default()
            },
        }
    }

    #[must_use]
    pub fn missing(self, requirements: &Requirements) -> Vec<&'static str> {
        let pairs = [
            (requirements.text, self.text, "text"),
            (requirements.streaming, self.streaming, "streaming"),
            (requirements.images, self.images, "images"),
            (requirements.input_audio, self.input_audio, "input_audio"),
            (
                requirements.pdf,
                self.pdf || self.pdf_text_extraction,
                "pdf_or_pdf_text_extraction",
            ),
            (requirements.video, self.video, "video"),
            (
                requirements.function_tools,
                self.function_tools,
                "function_tools",
            ),
            (requirements.custom_tools, self.custom_tools, "custom_tools"),
            (
                requirements.parallel_tools,
                self.parallel_tools,
                "parallel_tools",
            ),
            (
                requirements.structured_outputs,
                self.structured_outputs,
                "structured_outputs",
            ),
            (requirements.reasoning, self.reasoning, "reasoning"),
            (
                requirements.opaque_reasoning,
                self.opaque_reasoning,
                "opaque_reasoning",
            ),
            (requirements.logprobs, self.logprobs, "logprobs"),
            (requirements.seed, self.seed, "seed"),
            (
                requirements.multiple_choices,
                self.multiple_choices,
                "multiple_choices",
            ),
            (requirements.audio_output, self.audio_output, "audio_output"),
        ];
        pairs
            .into_iter()
            .filter_map(|(needed, available, name)| (needed && !available).then_some(name))
            .collect()
    }
}

#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct Requirements {
    pub text: bool,
    pub streaming: bool,
    pub images: bool,
    pub input_audio: bool,
    pub pdf: bool,
    pub video: bool,
    pub function_tools: bool,
    pub custom_tools: bool,
    pub parallel_tools: bool,
    pub structured_outputs: bool,
    pub reasoning: bool,
    pub opaque_reasoning: bool,
    pub logprobs: bool,
    pub seed: bool,
    pub multiple_choices: bool,
    pub audio_output: bool,
    pub affinity: Option<&'static str>,
}

impl Requirements {
    pub fn from_request(request: &ChatCompletionRequest) -> Result<Self, RouterError> {
        if request.model.trim().is_empty() {
            return Err(RouterError::invalid("model cannot be empty", "model"));
        }
        if request.messages.is_empty() {
            return Err(RouterError::invalid(
                "messages must contain at least one item",
                "messages",
            ));
        }
        if !request.extra.is_empty() {
            // Candidate-specific passthrough validation happens in the router.
        }
        let mut requirements = Self {
            text: true,
            streaming: true,
            structured_outputs: request.response_format.is_some(),
            reasoning: request.reasoning.is_some() || request.reasoning_effort.is_some(),
            logprobs: request.logprobs.unwrap_or(false) || request.top_logprobs.is_some(),
            seed: request.seed.is_some(),
            multiple_choices: request.n.unwrap_or(1) > 1,
            audio_output: request
                .modalities
                .as_ref()
                .is_some_and(|items| items.iter().any(|item| item == "audio")),
            ..Self::default()
        };
        let mut affinities = BTreeSet::new();
        for message in &request.messages {
            if !matches!(
                message.role.as_str(),
                "developer" | "system" | "user" | "assistant" | "tool" | "function"
            ) {
                return Err(RouterError::invalid(
                    format!("unsupported message role {}", message.role),
                    "messages.role",
                ));
            }
            if let Some(MessageContent::Parts(parts)) = &message.content {
                for part in parts {
                    match part.kind.as_str() {
                        "text" | "refusal" => {}
                        "image_url" => {
                            let source = part.image_url.as_ref().ok_or_else(|| {
                                RouterError::invalid(
                                    "image_url part is missing image_url",
                                    "messages.content.image_url",
                                )
                            })?;
                            validate_attachment_source(
                                &source.url,
                                "messages.content.image_url.url",
                            )?;
                            requirements.images = true;
                        }
                        "input_audio" => {
                            let audio = part.input_audio.as_ref().ok_or_else(|| {
                                RouterError::invalid(
                                    "input_audio part is missing input_audio",
                                    "messages.content.input_audio",
                                )
                            })?;
                            if audio.data.is_empty() {
                                return Err(RouterError::invalid(
                                    "input_audio data cannot be empty",
                                    "messages.content.input_audio.data",
                                ));
                            }
                            requirements.input_audio = true;
                        }
                        "file" => {
                            let file = part.file.as_ref().ok_or_else(|| {
                                RouterError::invalid(
                                    "file part is missing file",
                                    "messages.content.file",
                                )
                            })?;
                            if file.file_id.is_some() {
                                return Err(RouterError::invalid(
                                    "file_id is unsupported because this router has no Files API",
                                    "messages.content.file.file_id",
                                ));
                            }
                            let source = file.file_data.as_deref().ok_or_else(|| {
                                RouterError::invalid(
                                    "file part is missing file_data",
                                    "messages.content.file.file_data",
                                )
                            })?;
                            validate_attachment_source(source, "messages.content.file.file_data")?;
                            requirements.pdf = true;
                        }
                        "video_url" => {
                            let source = part.video_url.as_ref().ok_or_else(|| {
                                RouterError::invalid(
                                    "video_url part is missing video_url",
                                    "messages.content.video_url",
                                )
                            })?;
                            validate_attachment_source(
                                &source.url,
                                "messages.content.video_url.url",
                            )?;
                            requirements.video = true;
                        }
                        other => {
                            return Err(RouterError::invalid(
                                format!("unsupported content part type {other}"),
                                "messages.content.type",
                            ));
                        }
                    }
                }
            }
            if !message.reasoning_details.is_empty() {
                requirements.reasoning = true;
                requirements.opaque_reasoning = true;
                for detail in &message.reasoning_details {
                    if let Some(format) = detail.get("format").and_then(serde_json::Value::as_str) {
                        let format = format.to_ascii_lowercase();
                        if format.contains("anthropic") || format.contains("claude") {
                            affinities.insert("anthropic");
                        }
                        if format == "openai-responses-v1" {
                            affinities.insert("openai");
                        }
                        if format.contains("google") || format.contains("gemini") {
                            affinities.insert("google");
                        }
                    }
                }
            }
        }
        if affinities.len() > 1 {
            return Err(RouterError::invalid(
                "request contains incompatible opaque reasoning formats",
                "messages.reasoning_details",
            ));
        }
        requirements.affinity = affinities.into_iter().next();
        for tool in &request.tools {
            match tool.kind.as_str() {
                "function" => requirements.function_tools = true,
                "custom" => requirements.custom_tools = true,
                other => {
                    return Err(RouterError::invalid(
                        format!("unsupported tool type {other}"),
                        "tools.type",
                    ));
                }
            }
        }
        requirements.parallel_tools = request.parallel_tool_calls == Some(true);
        Ok(requirements)
    }
}

fn validate_attachment_source(source: &str, parameter: &str) -> Result<(), RouterError> {
    if source.starts_with("data:") {
        if !source.contains(";base64,") {
            return Err(RouterError::invalid(
                "attachment data URLs must use base64 encoding",
                parameter,
            ));
        }
        return Ok(());
    }
    // File payloads may use bare base64 for OpenRouter compatibility.
    if parameter.ends_with("file_data") && !source.contains("://") {
        return Ok(());
    }
    let url = url::Url::parse(source)
        .map_err(|_| RouterError::invalid("invalid attachment URL", parameter))?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err(RouterError::invalid(
            "attachment URLs must use HTTPS and cannot contain credentials",
            parameter,
        ));
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return Err(RouterError::invalid(
            "attachment URL host is not public",
            parameter,
        ));
    }
    if let Ok(ip) = host.parse::<IpAddr>()
        && !is_public_literal(ip)
    {
        return Err(RouterError::invalid(
            "attachment URL address is not public",
            parameter,
        ));
    }
    Ok(())
}

fn is_public_literal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast())
        }
        IpAddr::V6(ip) => {
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_multicast())
        }
    }
}
