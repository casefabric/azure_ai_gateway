use serde::Serialize;
use std::collections::BTreeMap;

use crate::anthropic::AnthropicModel;
use crate::ollama::OllamaModel;

/// A Foundry project deployment (`ModelDeployment`), as returned by `GET /deployments`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Deployment {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub name: String,
    pub model_name: String,
    pub model_version: String,
    pub model_publisher: String,
    pub capabilities: BTreeMap<&'static str, &'static str>,
    pub sku: Sku,
    pub connection_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Sku {
    pub name: &'static str,
    pub capacity: u32,
    pub family: &'static str,
    pub size: &'static str,
    pub tier: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentList {
    pub value: Vec<Deployment>,
    pub next_link: Option<String>,
}

impl Deployment {
    pub fn from_ollama(model: &OllamaModel) -> Self {
        let (model_name, model_version) = match model.name.split_once(':') {
            Some((name, tag)) => (name.to_string(), tag.to_string()),
            None => (model.name.clone(), "latest".to_string()),
        };

        let mut capabilities = BTreeMap::new();
        match &model.capabilities {
            // Older Ollama versions don't report capabilities; assume a chat model.
            None => {
                capabilities.insert("chat_completion", "true");
            }
            Some(caps) => {
                if caps.iter().any(|c| c == "completion") {
                    capabilities.insert("chat_completion", "true");
                }
                if caps.iter().any(|c| c == "embedding") {
                    capabilities.insert("embeddings", "true");
                }
                if caps.iter().any(|c| c == "tools") {
                    capabilities.insert("tool_calling", "true");
                }
                if caps.iter().any(|c| c == "vision") {
                    capabilities.insert("vision", "true");
                }
            }
        }

        Self::new(
            model.name.clone(),
            model_name,
            model_version,
            "Ollama",
            capabilities,
        )
    }

    /// Claude model IDs carry no tag, so the version is always `latest`. Every Claude model
    /// chats and calls tools; vision follows the Models API's `image_input` capability.
    pub fn from_anthropic(model: &AnthropicModel) -> Self {
        let mut capabilities =
            BTreeMap::from([("chat_completion", "true"), ("tool_calling", "true")]);
        if model.supports("image_input") {
            capabilities.insert("vision", "true");
        }
        Self::new(
            model.id.clone(),
            model.id.clone(),
            "latest".to_string(),
            "Anthropic",
            capabilities,
        )
    }

    fn new(
        name: String,
        model_name: String,
        model_version: String,
        publisher: &str,
        capabilities: BTreeMap<&'static str, &'static str>,
    ) -> Self {
        Deployment {
            kind: "ModelDeployment",
            name,
            model_name,
            model_version,
            model_publisher: publisher.to_string(),
            capabilities,
            sku: Sku {
                name: "GlobalStandard",
                capacity: 1,
                family: "",
                size: "",
                tier: "",
            },
            connection_name: None,
        }
    }

    /// Applies the optional `modelPublisher`, `modelName` and `deploymentType` query filters.
    pub fn matches(&self, query: &std::collections::HashMap<String, String>) -> bool {
        let eq =
            |key: &str, value: &str| query.get(key).is_none_or(|q| q.eq_ignore_ascii_case(value));
        eq("modelPublisher", &self.model_publisher)
            && eq("modelName", &self.model_name)
            && eq("deploymentType", self.kind)
    }
}

/// Azure-style error body: `{"error": {"code": …, "message": …}}`.
pub fn error_body(code: &str, message: &str) -> serde_json::Value {
    serde_json::json!({ "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(name: &str, caps: Option<&[&str]>) -> OllamaModel {
        OllamaModel {
            name: name.to_string(),
            capabilities: caps.map(|c| c.iter().map(|s| s.to_string()).collect()),
        }
    }

    #[test]
    fn splits_tag_into_version() {
        let d = Deployment::from_ollama(&model("llama3.1:8b", None));
        assert_eq!(d.name, "llama3.1:8b");
        assert_eq!(d.model_name, "llama3.1");
        assert_eq!(d.model_version, "8b");
        assert_eq!(d.capabilities.get("chat_completion"), Some(&"true"));
    }

    #[test]
    fn embedding_model_has_no_chat_capability() {
        let d = Deployment::from_ollama(&model("nomic-embed-text", Some(&["embedding"])));
        assert_eq!(d.model_version, "latest");
        assert_eq!(d.capabilities.get("chat_completion"), None);
        assert_eq!(d.capabilities.get("embeddings"), Some(&"true"));
    }

    #[test]
    fn serializes_in_foundry_shape() {
        let d = Deployment::from_ollama(&model("qwen2.5:7b", Some(&["completion", "tools"])));
        let json = serde_json::to_value(DeploymentList {
            value: vec![d],
            next_link: None,
        })
        .unwrap();
        assert_eq!(json["value"][0]["type"], "ModelDeployment");
        assert_eq!(json["value"][0]["modelName"], "qwen2.5");
        assert_eq!(json["value"][0]["capabilities"]["chat_completion"], "true");
        assert_eq!(json["value"][0]["sku"]["name"], "GlobalStandard");
        assert!(json["nextLink"].is_null());
    }

    #[test]
    fn anthropic_model_capabilities() {
        let model = AnthropicModel {
            id: "claude-opus-5-5".into(),
            capabilities: Some(serde_json::json!({"image_input": {"supported": true}})),
        };
        let d = Deployment::from_anthropic(&model);
        assert_eq!(d.name, "claude-opus-5-5");
        assert_eq!(d.model_publisher, "Anthropic");
        assert_eq!(d.capabilities.get("chat_completion"), Some(&"true"));
        assert_eq!(d.capabilities.get("vision"), Some(&"true"));
    }

    #[test]
    fn query_filters() {
        let d = Deployment::from_ollama(&model("qwen2.5:7b", None));
        let q = |k: &str, v: &str| [(k.to_string(), v.to_string())].into_iter().collect();
        assert!(d.matches(&Default::default()));
        assert!(d.matches(&q("modelName", "qwen2.5")));
        assert!(!d.matches(&q("modelName", "llama3.1")));
        assert!(d.matches(&q("deploymentType", "ModelDeployment")));
    }
}
