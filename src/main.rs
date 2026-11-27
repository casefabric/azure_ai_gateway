mod anthropic;
mod files;
mod foundry;
mod ollama;
mod proxy;
mod requestlog;
mod route;
mod savings;

use actix_web::{App, HttpServer, web};
use clap::{Parser, ValueEnum};
use std::time::Duration;

use crate::anthropic::{AnthropicClient, RequestOptions};
use crate::ollama::OllamaClient;
use crate::proxy::{AppState, Backend};
use crate::requestlog::RequestLog;
use crate::savings::LoopGuard;

#[derive(Parser)]
#[command(about = "Azure AI Foundry compatible proxy in front of Ollama or the Anthropic API")]
struct Args {
    /// Port to listen on
    #[arg(short, long, env = "AZURE_AI_GATEWAY_PORT", default_value_t = 8135)]
    port: u16,

    /// Where chat completions and the deployment list come from
    #[arg(long, env = "AZURE_AI_GATEWAY_BACKEND", value_enum, default_value_t = BackendKind::Ollama)]
    backend: BackendKind,

    /// Base URL of the Ollama server to forward to
    #[arg(long, env = "OLLAMA_URL", default_value = "http://localhost:11434")]
    ollama_url: String,

    /// API key sent to Ollama as a Bearer token (only needed for ollama.com or an authenticating reverse proxy)
    #[arg(long, env = "OLLAMA_API_KEY")]
    ollama_api_key: Option<String>,

    /// Where to read the cloud model catalog (`/api/tags`) that the Ollama desktop app also lists;
    /// set to an empty string to list local models only
    #[arg(long, env = "OLLAMA_CLOUD_URL", default_value = "https://ollama.com")]
    ollama_cloud_url: String,

    /// Base URL of the Anthropic API (`--backend anthropic`)
    #[arg(
        long,
        env = "ANTHROPIC_BASE_URL",
        default_value = "https://api.anthropic.com"
    )]
    anthropic_url: String,

    /// Anthropic API key, required for `--backend anthropic`
    #[arg(long, env = "ANTHROPIC_API_KEY", hide_env_values = true)]
    anthropic_api_key: Option<String>,

    /// `max_tokens` sent to Anthropic when the caller doesn't set `max_tokens` / `max_completion_tokens`
    #[arg(long, env = "ANTHROPIC_MAX_TOKENS", default_value_t = 16000)]
    anthropic_max_tokens: u64,

    /// Don't add `fallbacks: "default"` (re-running a request declined by a safety classifier on another model)
    #[arg(long, env = "ANTHROPIC_NO_FALLBACKS")]
    anthropic_no_fallbacks: bool,

    /// `output_config.effort` sent to Anthropic when the caller doesn't set `reasoning_effort`;
    /// lower effort means fewer thinking and output tokens
    #[arg(long, env = "ANTHROPIC_EFFORT", value_parser = ["low", "medium", "high", "xhigh", "max"])]
    anthropic_effort: Option<String>,

    /// Ask Anthropic for cache diagnostics: each request names the previous answer of its
    /// conversation, and the API reports where the prompt diverged (logged and in the request log)
    #[arg(long, env = "ANTHROPIC_CACHE_DIAGNOSTICS")]
    anthropic_cache_diagnostics: bool,

    /// Regex of tool names removed from every chat request, e.g. "^(cmmn_|vc_)"; tool
    /// definitions are resent on every call, so unused tools cost input tokens each time
    #[arg(long, env = "AZURE_AI_GATEWAY_TOOLS_DENY")]
    tools_deny: Option<regex::Regex>,

    /// A chat request with the same body arriving more than this many times within a minute is
    /// refused with 429 instead of being sent to the backend again; 0 turns this off
    #[arg(long, env = "AZURE_AI_GATEWAY_LOOP_MAX", default_value_t = 3)]
    loop_max: usize,

    /// One JSON line per chat request in `logs/requests.jsonl` (see `log-config.yaml`): `summary`
    /// holds counts, timings, token usage and hashes; `full` also holds requests and answers
    #[arg(long, env = "AZURE_AI_GATEWAY_REQUEST_LOG", value_enum, default_value_t = requestlog::Mode::Summary)]
    request_log: requestlog::Mode,

    /// API key callers must send as `api-key` or `Authorization: Bearer`; accepts any caller when unset
    #[arg(long, env = "AZURE_AI_GATEWAY_API_KEY")]
    api_key: Option<String>,

    /// Timeout in seconds for a request to the backend, including streaming the full response
    #[arg(long, env = "OLLAMA_TIMEOUT", default_value_t = 600)]
    timeout: u64,

    /// Maximum request body size in megabytes
    #[arg(long, env = "AZURE_AI_GATEWAY_MAX_BODY_MB", default_value_t = 32)]
    max_body_mb: usize,

    /// Path to the log4rs YAML configuration file
    #[arg(
        long,
        env = "AZURE_AI_GATEWAY_LOG_CONFIG",
        default_value = "log-config.yaml"
    )]
    log_config: std::path::PathBuf,
}

#[derive(Clone, Copy, ValueEnum)]
enum BackendKind {
    Ollama,
    Anthropic,
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();

    log4rs::init_file(&args.log_config, Default::default())
        .unwrap_or_else(|e| eprintln!("Failed to load log config {:?}: {e}", args.log_config));

    let timeout = Duration::from_secs(args.timeout);
    let backend = match args.backend {
        BackendKind::Ollama => {
            let cloud_url = Some(args.ollama_cloud_url.as_str()).filter(|url| !url.is_empty());
            let ollama =
                OllamaClient::new(&args.ollama_url, args.ollama_api_key, cloud_url, timeout)
                    .expect("Failed to build HTTP client");
            Backend::Ollama(ollama)
        }
        BackendKind::Anthropic => {
            let Some(api_key) = args.anthropic_api_key else {
                eprintln!("--backend anthropic needs --anthropic-api-key or ANTHROPIC_API_KEY");
                std::process::exit(2);
            };
            let fallbacks = !args.anthropic_no_fallbacks;
            let mut anthropic = AnthropicClient::new(
                &args.anthropic_url,
                api_key,
                RequestOptions {
                    max_tokens: args.anthropic_max_tokens,
                    fallbacks,
                    effort: args.anthropic_effort,
                },
                timeout,
            )
            .expect("Failed to build HTTP client");
            anthropic.cache_diagnostics = args.anthropic_cache_diagnostics;
            Backend::Anthropic(anthropic)
        }
    };

    log::info!(
        "Azure AI gateway listening on http://0.0.0.0:{}, forwarding to {}",
        args.port,
        backend.base_url()
    );

    if args.request_log == requestlog::Mode::Full {
        log::warn!("--request-log full writes prompts and answers to the request log");
    }

    let state = web::Data::new(AppState {
        backend,
        api_key: args.api_key,
        tools_deny: args.tools_deny,
        loop_guard: LoopGuard::new(args.loop_max, Duration::from_secs(60)),
        request_log: RequestLog::new(args.request_log),
    });
    let max_body = args.max_body_mb * 1024 * 1024;

    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .app_data(web::PayloadConfig::new(max_body))
            .default_service(web::to(proxy::handle))
    })
    .bind(("0.0.0.0", args.port))?
    .run()
    .await
}
