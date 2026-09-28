mod anthropic;
mod attachment;
mod bedrock;
mod gemini;
mod models;
mod openai_compatible;
mod openai_responses;
mod sse;
mod transport;

use std::{collections::HashMap, sync::Arc};

use router_core::{AdapterMap, AppConfig, ProviderAdapter, ProviderProtocol, RouterError};

pub fn build_adapters(config: &AppConfig) -> Result<AdapterMap, RouterError> {
    let mut adapters: HashMap<String, Arc<dyn ProviderAdapter>> = HashMap::new();
    for (name, provider) in &config.providers {
        let adapter: Arc<dyn ProviderAdapter> = match provider.kind.protocol() {
            ProviderProtocol::OpenAiChatCompletions => {
                Arc::new(openai_compatible::OpenAiCompatibleAdapter::new(provider)?)
            }
            ProviderProtocol::AnthropicMessages => {
                Arc::new(anthropic::AnthropicAdapter::new(provider)?)
            }
            ProviderProtocol::BedrockConverse => Arc::new(bedrock::BedrockAdapter::new(provider)?),
            ProviderProtocol::GeminiGenerateContent => {
                Arc::new(gemini::GeminiAdapter::new(provider)?)
            }
        };
        adapters.insert(name.clone(), adapter);
    }
    Ok(adapters)
}
