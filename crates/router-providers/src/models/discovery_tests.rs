use super::*;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn fixture(
    handler: impl Fn(&str) -> (u16, Value) + Send + 'static,
) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut input = Vec::new();
            while !input.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut buffer = [0; 4096];
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0 && input.len() < 16384);
                input.extend_from_slice(&buffer[..count]);
            }
            let request = String::from_utf8(input).unwrap();
            let (status, body) = handler(request.split_whitespace().nth(1).unwrap());
            seen.lock().unwrap().push(request);
            let body = body.to_string();
            let response = format!(
                "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (format!("http://{address}"), requests, task)
}

async fn from_adapter(config: ProviderConfig) -> Option<Vec<ModelObject>> {
    let app = router_core::AppConfig {
        providers: [("customer".into(), config.clone())].into(),
        models: BTreeMap::default(),
        aliases: BTreeMap::default(),
        server: router_core::ServerConfig::default(),
        auth: router_core::AuthConfig::default(),
        web_ui: router_core::WebUiConfig::default(),
    };
    crate::build_adapters(&app).unwrap()["customer"]
        .discover_models(&config, Some(&SecretString::from("customer-key")))
        .await
}

fn foundation() -> Value {
    json!({"modelSummaries":[
        {"modelId":"anthropic.claude-test-v1:0","modelName":"Claude test","inputModalities":["TEXT","IMAGE"],"outputModalities":["TEXT"],"responseStreamingSupported":true,"inferenceTypesSupported":["ON_DEMAND"],"modelLifecycle":{"status":"ACTIVE"}},
        {"modelId":"embed-test","inputModalities":["TEXT"],"outputModalities":["EMBEDDING"],"responseStreamingSupported":true,"inferenceTypesSupported":["ON_DEMAND"]},
        {"modelId":"no-stream","inputModalities":["TEXT"],"outputModalities":["TEXT"],"responseStreamingSupported":false,"inferenceTypesSupported":["ON_DEMAND"]},
        {"modelId":"retired","inputModalities":["TEXT"],"outputModalities":["TEXT"],"responseStreamingSupported":true,"inferenceTypesSupported":["ON_DEMAND"],"modelLifecycle":{"status":"DEPRECATED"}}
    ]})
}
fn profile(id: &str, target: &str) -> Value {
    json!({"inferenceProfileId":id,"inferenceProfileName":id,"status":"ACTIVE","models":[{"modelArn":format!("arn:aws:bedrock:us-east-1::foundation-model/{target}")}]})
}

#[test]
fn provider_catalogue_endpoints_use_the_control_plane() {
    for (region, suffix) in [
        ("us-east-1", "amazonaws.com"),
        ("cn-north-1", "amazonaws.com.cn"),
    ] {
        let config = serde_json::from_value(json!({"kind":"amazon-bedrock","base_url":format!("https://bedrock-runtime.{region}.{suffix}")})).unwrap();
        assert_eq!(
            bedrock::control_url(&config).unwrap().as_str(),
            format!("https://bedrock.{region}.{suffix}/")
        );
    }
    let config = serde_json::from_value(json!({"kind":"gemini","auth_header":"authorization","base_url":"https://aiplatform.googleapis.com/v1/projects/example/locations/global/publishers/google"})).unwrap();
    assert_eq!(
        vertex_catalog_url(&config).unwrap().as_str(),
        "https://aiplatform.googleapis.com/v1beta1/publishers/google/models"
    );
}

#[tokio::test]
async fn bedrock_discovers_foundation_and_paginated_profile_ids_with_customer_key() {
    let (url, requests, task) = fixture(|path| {
        if path.starts_with("/foundation-models?") { return (200, foundation()); }
        if path == "/inference-profiles?maxResults=100" {
            return (200, json!({"inferenceProfileSummaries":[profile("us.anthropic.claude-test-v1:0","anthropic.claude-test-v1:0"), profile("us.embed-test","embed-test")],"nextToken":"second/+"}));
        }
        if path == "/inference-profiles?maxResults=100&nextToken=second%2F%2B" {
            return (200, json!({"inferenceProfileSummaries":[profile("my-app-profile","anthropic.claude-test-v1:0")]}));
        }
        (404,json!({}))
    }).await;
    let config = serde_json::from_value(
        json!({"kind":"amazon-bedrock","base_url":url,"allow_insecure_http":true}),
    )
    .unwrap();
    let models = from_adapter(config).await.unwrap();
    task.abort();
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        [
            "anthropic.claude-test-v1:0",
            "my-app-profile",
            "us.anthropic.claude-test-v1:0"
        ]
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests
            .iter()
            .all(|r| r.contains("authorization: Bearer customer-key\r\n"))
    );
}

#[tokio::test]
async fn bedrock_keeps_foundation_discovery_when_profile_listing_is_forbidden() {
    let (url, _, task) = fixture(|path| {
        if path.starts_with("/foundation-models?") {
            (200, foundation())
        } else {
            (403, json!({"message":"not permitted"}))
        }
    })
    .await;
    let config = serde_json::from_value(
        json!({"kind":"amazon-bedrock","base_url":url,"allow_insecure_http":true}),
    )
    .unwrap();
    let models = from_adapter(config).await.unwrap();
    task.abort();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "anthropic.claude-test-v1:0");
}

#[tokio::test]
async fn vertex_discovers_publisher_models_with_pagination_and_filters_non_chat_models() {
    let (url, requests, task) = fixture(|path| match path {
        "/v1beta1/publishers/google/models?pageSize=1000" => (200,json!({"publisherModels":[{"name":"publishers/google/models/gemini-2.5-flash"},{"name":"publishers/google/models/gemini-embedding-2"},{"name":"publishers/google/models/imagen-4"},{"name":"publishers/google/models/gemma-self-hosted"}],"nextPageToken":"page-two"})),
        "/v1beta1/publishers/google/models?pageSize=1000&pageToken=page-two" => (200,json!({"publisherModels":[{"name":"publishers/google/models/gemini-3.8-flash"},{"name":"publishers/google/models/gemini-2.5-flash"}]})),
        _ => (404,json!({})),
    }).await;
    let config = serde_json::from_value(json!({"kind":"gemini","auth_header":"authorization","base_url":format!("{url}/v1/projects/test/locations/global/publishers/google"),"allow_insecure_http":true})).unwrap();
    let models = from_adapter(config).await.unwrap();
    task.abort();
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["gemini-2.5-flash", "gemini-3.8-flash"]
    );
    assert_eq!(
        models[1].metadata["reasoning_efforts"],
        json!(["low", "medium", "high"])
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|r| r.contains("authorization: Bearer customer-key\r\n")
                && !r.contains("x-goog-api-key"))
    );
}

#[tokio::test]
async fn azure_discovers_with_resource_key_and_never_bearer_header() {
    let (url, requests, task) = fixture(|path| {
        if path == "/openai/v1/models" {
            (200, json!({"data":[{"id":"my-deployment"}]}))
        } else {
            (404, json!({}))
        }
    })
    .await;
    let config = serde_json::from_value(json!({"kind":"openai-compatible","auth_header":"api-key","base_url":format!("{url}/openai/v1"),"allow_insecure_http":true})).unwrap();
    let models = from_adapter(config).await.unwrap();
    task.abort();
    assert_eq!(models[0].id, "my-deployment");
    let requests = requests.lock().unwrap();
    assert!(
        requests[0].contains("api-key: customer-key\r\n")
            && !requests[0].contains("authorization:")
    );
}

#[tokio::test]
async fn repeated_page_token_terminates_discovery() {
    let (url, requests, task) =
        fixture(|_| (200, json!({"models":[],"nextPageToken":"loop"}))).await;
    let config =
        serde_json::from_value(json!({"kind":"gemini","base_url":url,"allow_insecure_http":true}))
            .unwrap();
    assert!(from_adapter(config).await.is_none());
    task.abort();
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn xai_discovers_its_chat_models_from_language_models() {
    let (url, requests, task) = fixture(|path| {
        if path == "/v1/language-models" {
            (
                200,
                json!({"models":[
                    {"id":"grok-4.7","input_modalities":["text","image"],"capabilities":{"reasoning_effort":["low","high"]}},
                    {"id":"grok-4.20-multi-agent-0309","input_modalities":["text","image"]}
                ]}),
            )
        } else {
            (404, json!({}))
        }
    })
    .await;
    let config = serde_json::from_value(
        json!({"kind":"xai","base_url":format!("{url}/v1"),"allow_insecure_http":true}),
    )
    .unwrap();
    let models = from_adapter(config).await.unwrap();
    task.abort();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "grok-4.7");
    assert_eq!(models[0].metadata["supports_vision"], json!(true));
    assert!(requests.lock().unwrap()[0].starts_with("GET /v1/language-models "));
}

#[tokio::test]
async fn gemini_lists_a_new_generation_and_drops_what_cannot_chat() {
    let (url, _, task) = fixture(|path| {
        if path.starts_with("/models?") {
            (
                200,
                json!({"models":[
                    {"name":"models/gemini-5-flash","thinking":true,"supportedGenerationMethods":["generateContent","countTokens"]},
                    {"name":"models/gemini-5-flash-live","supportedGenerationMethods":["bidiGenerateContent"]},
                    {"name":"models/gemini-embedding-3","supportedGenerationMethods":["embedContent"]}
                ]}),
            )
        } else {
            (404, json!({}))
        }
    })
    .await;
    let config =
        serde_json::from_value(json!({"kind":"gemini","base_url":url,"allow_insecure_http":true}))
            .unwrap();
    let models = from_adapter(config).await.unwrap();
    task.abort();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "gemini-5-flash");
    assert_eq!(
        models[0].metadata["reasoning_efforts"],
        json!(["low", "medium", "high"])
    );
}
