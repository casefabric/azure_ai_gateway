use futures_util::future::join_all;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::Duration;

use crate::files::File;

/// A locally available Ollama model, with the capabilities reported by `/api/show`.
#[derive(Debug, Clone)]
pub struct OllamaModel {
    pub name: String,
    /// `None` when the Ollama version doesn't report capabilities.
    pub capabilities: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct TagsResponse {
    models: Vec<Tag>,
}

#[derive(Deserialize)]
struct Tag {
    name: String,
}

#[derive(Deserialize)]
struct ShowResponse {
    capabilities: Option<Vec<String>>,
}

#[derive(Clone)]
pub struct OllamaClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    /// Where the cloud model catalog is read from (`{url}/api/tags`), like the Ollama desktop app does.
    cloud_url: Option<String>,
}

impl OllamaClient {
    pub fn new(
        base_url: &str,
        api_key: Option<String>,
        cloud_url: Option<&str>,
        timeout: Duration,
    ) -> reqwest::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(timeout)
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            cloud_url: cloud_url.map(|url| url.trim_end_matches('/').to_string()),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Builds a request to `path` on the Ollama server, adding the API key when configured.
    pub fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let builder = self
            .http
            .request(method, format!("{}{path}", self.base_url));
        match &self.api_key {
            Some(key) => builder.bearer_auth(key),
            None => builder,
        }
    }

    /// Lists the local models (`/api/tags`) plus, when enabled, the cloud catalog, and looks up
    /// each model's capabilities (`/api/show`). Cloud models are named `<model>:cloud`, which the
    /// local server runs remotely when it is signed in to ollama.com.
    pub async fn models(&self) -> reqwest::Result<Vec<OllamaModel>> {
        let tags: TagsResponse = self
            .request(reqwest::Method::GET, "/api/tags")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let local: Vec<String> = tags.models.into_iter().map(|tag| tag.name).collect();

        let names = match self.cloud_models().await {
            Ok(cloud) => merge_cloud(local, cloud),
            Err(e) => {
                log::warn!("Could not read cloud models: {e}");
                local
            }
        };

        let models = join_all(names.into_iter().map(|name| async move {
            let capabilities = match self.capabilities(&name).await {
                Ok(caps) => caps,
                Err(e) => {
                    log::warn!("Could not read capabilities of {name}: {e}");
                    None
                }
            };
            OllamaModel { name, capabilities }
        }))
        .await;

        Ok(models)
    }

    /// The cloud catalog's model names, empty when cloud models are disabled.
    async fn cloud_models(&self) -> reqwest::Result<Vec<String>> {
        let Some(url) = &self.cloud_url else {
            return Ok(Vec::new());
        };
        let tags: TagsResponse = self
            .http
            .get(format!("{url}/api/tags"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(tags.models.into_iter().map(|tag| tag.name).collect())
    }

    async fn capabilities(&self, model: &str) -> reqwest::Result<Option<Vec<String>>> {
        let show: ShowResponse = self
            .request(reqwest::Method::POST, "/api/show")
            .json(&serde_json::json!({ "model": model }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(show.capabilities)
    }
}

/// Rewrites `file` content parts, which Ollama's OpenAI API rejects ("invalid message format"),
/// into parts it accepts: PDFs and text files become text parts holding their contents, images
/// become `image_url` parts. Returns whether anything changed.
pub fn inline_files(chat: &mut Value) -> Result<bool, String> {
    let mut changed = false;
    let messages = chat["messages"].as_array_mut().into_iter().flatten();
    let parts = messages.filter_map(|message| message["content"].as_array_mut());
    for part in parts.flatten().filter(|part| part["type"] == "file") {
        let file = File::from_part(&part["file"])?;
        *part = if file.is_pdf() || file.is_text() {
            let text = if file.is_pdf() {
                file.pdf_text()?
            } else {
                file.text()?
            };
            json!({"type": "text", "text": format!("<file name=\"{}\">\n{text}\n</file>", file.name())})
        } else if file.media_type.starts_with("image/") {
            let url = format!("data:{};base64,{}", file.media_type, file.data);
            json!({"type": "image_url", "image_url": {"url": url}})
        } else {
            return Err(format!(
                "{}: unsupported file type {}",
                file.name(),
                file.media_type
            ));
        };
        changed = true;
    }
    Ok(changed)
}

/// Appends the cloud models to the local ones the way the Ollama desktop app does: each gets a
/// `:cloud` suffix (unless its tag already marks it as cloud), and one that is also available
/// locally is left out.
fn merge_cloud(local: Vec<String>, cloud: Vec<String>) -> Vec<String> {
    let base = |name: &str| {
        name.trim_end_matches(":latest")
            .trim_end_matches(":cloud")
            .to_string()
    };
    let mut seen: HashSet<String> = local.iter().map(|name| base(name)).collect();
    let cloud = cloud.into_iter().filter_map(|name| {
        let name = name.trim_end_matches(":latest");
        let tag = name.rsplit_once(':').map(|(_, tag)| tag.to_lowercase());
        let name = match tag {
            Some(tag) if tag == "cloud" || tag.ends_with("-cloud") => name.to_string(),
            _ => format!("{name}:cloud"),
        };
        seen.insert(base(&name)).then_some(name)
    });
    local
        .iter()
        .cloned()
        .chain(cloud.collect::<Vec<_>>())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn inlines_text_files() {
        let mut chat = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "Summarize"},
            {"type": "file", "file": {"filename": "a.txt", "file_data": "data:text/plain;base64,aGVsbG8="}},
            {"type": "file", "file": {"file_data": "data:image/png;base64,AAAA"}}
        ]}]});
        assert!(inline_files(&mut chat).unwrap());
        let content = &chat["messages"][0]["content"];
        assert_eq!(
            content[1],
            json!({"type": "text", "text": "<file name=\"a.txt\">\nhello\n</file>"})
        );
        assert_eq!(content[2]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn leaves_plain_chats_alone() {
        let mut chat = json!({"messages": [{"role": "user", "content": "Hi"}]});
        assert!(!inline_files(&mut chat).unwrap());
    }

    #[test]
    fn cloud_models_get_cloud_suffix() {
        let merged = merge_cloud(
            names(&["mistral:latest"]),
            names(&["glm-5.3", "gpt-oss:120b", "qwen3:480b-cloud"]),
        );
        assert_eq!(
            merged,
            names(&[
                "mistral:latest",
                "glm-5.3:cloud",
                "gpt-oss:120b:cloud",
                "qwen3:480b-cloud"
            ])
        );
    }

    #[test]
    fn local_model_wins_over_cloud() {
        let merged = merge_cloud(
            names(&["gpt-oss:20b", "glm-5.3:latest"]),
            names(&["gpt-oss:20b", "glm-5.3"]),
        );
        assert_eq!(merged, names(&["gpt-oss:20b", "glm-5.3:latest"]));
    }
}
