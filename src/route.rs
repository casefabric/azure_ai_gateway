use actix_web::http::Method;
use percent_encoding::percent_decode_str;

/// The Foundry / Azure OpenAI operations this proxy understands.
///
/// Paths are matched on their suffix, so any prefix works: a project endpoint such as
/// `/api/projects/<project>/deployments` resolves the same as a bare `/deployments`.
#[derive(Debug, PartialEq)]
pub enum Route {
    /// `GET …/deployments`
    ListDeployments,
    /// `GET …/deployments/{name}`
    GetDeployment(String),
    /// `POST …/openai/deployments/{name}/chat/completions` (deployment in path) or
    /// `POST …/chat/completions` (model in body, e.g. `/models/chat/completions`)
    ChatCompletions(Option<String>),
    /// `POST …/deployments/{name}/embeddings` or `POST …/embeddings`
    Embeddings(Option<String>),
    /// `GET …/models`
    Models,
    Unknown,
}

pub fn parse(method: &Method, path: &str) -> Route {
    let segments: Vec<String> = path
        .trim_matches('/')
        .split('/')
        .map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned())
        .collect();
    let n = segments.len();
    let seg = |from_end: usize| -> Option<&str> {
        n.checked_sub(from_end + 1).map(|i| segments[i].as_str())
    };

    if *method == Method::POST {
        if seg(0) == Some("completions") && seg(1) == Some("chat") {
            let deployment = (seg(3) == Some("deployments")).then(|| seg(2).unwrap().to_string());
            return Route::ChatCompletions(deployment);
        }
        if seg(0) == Some("embeddings") {
            let deployment = (seg(2) == Some("deployments")).then(|| seg(1).unwrap().to_string());
            return Route::Embeddings(deployment);
        }
    }

    if *method == Method::GET {
        if seg(0) == Some("deployments") {
            return Route::ListDeployments;
        }
        if seg(1) == Some("deployments") {
            return Route::GetDeployment(seg(0).unwrap().to_string());
        }
        if seg(0) == Some("models") {
            return Route::Models;
        }
    }

    Route::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_deployments_with_project_prefix() {
        assert_eq!(
            parse(&Method::GET, "/api/projects/demo/deployments"),
            Route::ListDeployments
        );
        assert_eq!(parse(&Method::GET, "/deployments"), Route::ListDeployments);
    }

    #[test]
    fn get_single_deployment_decodes_name() {
        assert_eq!(
            parse(&Method::GET, "/api/projects/demo/deployments/llama3.1%3A8b"),
            Route::GetDeployment("llama3.1:8b".into())
        );
    }

    #[test]
    fn chat_completions_with_deployment_in_path() {
        assert_eq!(
            parse(
                &Method::POST,
                "/api/projects/demo/openai/deployments/qwen2.5/chat/completions"
            ),
            Route::ChatCompletions(Some("qwen2.5".into()))
        );
    }

    #[test]
    fn chat_completions_with_model_in_body() {
        assert_eq!(
            parse(&Method::POST, "/models/chat/completions"),
            Route::ChatCompletions(None)
        );
        assert_eq!(
            parse(&Method::POST, "/openai/v1/chat/completions"),
            Route::ChatCompletions(None)
        );
    }

    #[test]
    fn embeddings() {
        assert_eq!(
            parse(
                &Method::POST,
                "/openai/deployments/nomic-embed-text/embeddings"
            ),
            Route::Embeddings(Some("nomic-embed-text".into()))
        );
        assert_eq!(
            parse(&Method::POST, "/openai/v1/embeddings"),
            Route::Embeddings(None)
        );
    }

    #[test]
    fn unknown_paths() {
        assert_eq!(parse(&Method::POST, "/deployments"), Route::Unknown);
        assert_eq!(parse(&Method::GET, "/assistants"), Route::Unknown);
    }
}
