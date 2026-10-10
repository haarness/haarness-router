//! Account model discovery through the Bedrock control-plane APIs, using only the supplied key.
use super::{BTreeMap, ExposeSecret, ModelObject, ProviderConfig, SecretString, Value, json};

pub(super) fn control_url(config: &ProviderConfig) -> Option<url::Url> {
    let mut url = url::Url::parse(&format!(
        "{}/",
        config.effective_base_url().ok()?.trim_end_matches('/')
    ))
    .ok()?;
    let host = url.host_str()?.to_owned();
    if let Some(region) = host.strip_prefix("bedrock-runtime.")
        && (region.ends_with(".amazonaws.com") || region.ends_with(".amazonaws.com.cn"))
    {
        url.set_host(Some(&format!("bedrock.{region}"))).ok()?;
    }
    Some(url)
}

pub(super) async fn discover(
    client: &reqwest::Client,
    config: &ProviderConfig,
    key: &SecretString,
) -> Option<Vec<ModelObject>> {
    let base = control_url(config)?;
    let mut foundation_url = base.join("foundation-models").ok()?;
    foundation_url
        .query_pairs_mut()
        .append_pair("byOutputModality", "TEXT");
    let foundation = super::read_json(client.get(foundation_url).bearer_auth(key.expose_secret()));
    let profiles = profiles(client, &base, key);
    let (foundation, profiles) = tokio::join!(foundation, profiles);
    let foundation = foundation?;
    let mut definitions = BTreeMap::new();
    let mut models = BTreeMap::new();
    for entry in foundation["modelSummaries"].as_array()? {
        let Some(id) = entry["modelId"].as_str() else {
            continue;
        };
        let supports = |field: &str, value: &str| {
            entry[field]
                .as_array()
                .is_some_and(|v| v.iter().any(|v| v == value))
        };
        if !supports("inputModalities", "TEXT")
            || !supports("outputModalities", "TEXT")
            || entry["responseStreamingSupported"] != true
            || entry
                .pointer("/modelLifecycle/status")
                .is_some_and(|s| s != "ACTIVE" && s != "LEGACY")
        {
            continue;
        }
        let model = model(id, entry["modelName"].as_str().unwrap_or(id));
        definitions.insert(id.to_owned(), model.clone());
        if supports("inferenceTypesSupported", "ON_DEMAND") {
            models.insert(id.to_owned(), model);
        }
    }
    // Profiles are distinct callable IDs (including regional and application profiles).
    // A key can be permitted to list foundation models but not profiles.
    for entry in profiles.unwrap_or_default() {
        let Some(id) = entry["inferenceProfileId"].as_str() else {
            continue;
        };
        let Some(targets) = entry["models"].as_array() else {
            continue;
        };
        if entry["status"] != "ACTIVE"
            || targets.is_empty()
            || !targets.iter().all(|target| {
                target["modelArn"]
                    .as_str()
                    .and_then(|arn| arn.split_once(":foundation-model/").map(|(_, id)| id))
                    .is_some_and(|id| definitions.contains_key(id))
            })
        {
            continue;
        }
        models.insert(
            id.to_owned(),
            model(id, entry["inferenceProfileName"].as_str().unwrap_or(id)),
        );
    }
    Some(models.into_values().collect())
}

fn model(id: &str, name: &str) -> ModelObject {
    ModelObject {
        id: id.into(),
        object: "model".into(),
        created: 0,
        owned_by: String::new(),
        metadata: [("display_name".into(), json!(name))].into(),
    }
}

async fn profiles(
    client: &reqwest::Client,
    base: &url::Url,
    key: &SecretString,
) -> Option<Vec<Value>> {
    let mut cursor = None::<String>;
    let mut results = Vec::new();
    for _ in 0..10 {
        let mut url = base.join("inference-profiles").ok()?;
        url.query_pairs_mut().append_pair("maxResults", "100");
        if let Some(cursor) = &cursor {
            url.query_pairs_mut().append_pair("nextToken", cursor);
        }
        let body = super::read_json(client.get(url).bearer_auth(key.expose_secret())).await?;
        results.extend(
            body["inferenceProfileSummaries"]
                .as_array()?
                .iter()
                .cloned(),
        );
        let Some(next) = body["nextToken"].as_str().filter(|v| !v.is_empty()) else {
            return Some(results);
        };
        if cursor.as_deref() == Some(next) {
            return None;
        }
        cursor = Some(next.into());
    }
    None
}
