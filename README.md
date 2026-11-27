# azure_ai_gateway

A small proxy that makes a local [Ollama](https://ollama.com) server, or the Anthropic API, look like an
Azure AI Foundry project endpoint.

The Mendix Agent Editor only supports Mendix Cloud GenAI and Azure AI Foundry. Point it at this gateway
instead of a real Foundry project and it can list your Ollama models (or Claude models) as deployments
and run chat completions against them.

```
Mendix Agent Editor ──► azure_ai_gateway (:8135) ──► Ollama  (default)
   (Foundry client)                              └─► Anthropic Messages API  (--backend anthropic)
```

## What you need

- [Rust](https://rustup.rs) (edition 2024, so a recent stable toolchain), or Docker
- For the Ollama backend: a running Ollama with at least one model pulled
- For the Anthropic backend: an Anthropic API key

## Quick start with Ollama

```bash
ollama pull llama3.1:8b      # any model will do
cargo run --release
```

The gateway listens on port `8135` and forwards to Ollama on `http://localhost:11434`. Check that it works:

```bash
# List the deployments the Agent Editor will see
curl 'http://localhost:8135/api/projects/demo/deployments?api-version=v1'

# Chat with one of them
curl 'http://localhost:8135/api/projects/demo/openai/deployments/llama3.1:8b/chat/completions?api-version=v1' \
  -H 'Content-Type: application/json' \
  -d '{"messages": [{"role": "user", "content": "Say hello"}]}'
```

The project name in the path (`demo` here) can be anything.

Run it from the repository root: the gateway reads `log-config.yaml` from the working directory. Without
that file it prints a warning and runs without logging.

## Quick start with Claude

```bash
export ANTHROPIC_API_KEY=sk-ant-...
cargo run --release -- --backend anthropic
```

Every Claude model your key has access to shows up as a deployment, named by its model ID
(for example `claude-opus-5-5`). Requests and responses are translated between the OpenAI chat
completions format and the Anthropic Messages API, including streaming, tools, images and PDFs.
Embeddings are not available on this backend.

## Connecting the Mendix Agent Editor

In the Agent Editor, add an Azure AI Foundry connection with:

| Field | Value |
|---|---|
| Endpoint | `http://<host>:8135/api/projects/<any-name>` |
| API key | anything, unless you started the gateway with `--api-key` |

Then use "List deployments" and pick a model. Only models that can chat are offered.

**Studio Pro in a Windows VM on a Mac:** `localhost` inside the VM is the VM itself. Use the Mac's IP
address instead, e.g. `http://192.168.1.20:8135/api/projects/demo`. The gateway binds to `0.0.0.0`,
so it is reachable from the VM. Ollama can stay on `localhost` as long as the gateway runs on the same Mac.

Mendix does not document which calls the Agent Editor makes, so the routes are based on the Foundry
project API. If something does not work, look in the log: every request is logged, and paths the gateway
does not know are answered with a 404 and show up there.

## Running in Docker

```bash
docker build -t azure_ai_gateway .

# Ollama on the Docker host
docker run --rm -p 8135:8135 azure_ai_gateway

# Anthropic
docker run --rm -p 8135:8135 \
  -e AZURE_AI_GATEWAY_BACKEND=anthropic \
  -e ANTHROPIC_API_KEY \
  azure_ai_gateway
```

Notes for the Ollama backend in Docker:

- The image points at `http://host.docker.internal:11434`. Docker Desktop resolves that name by itself;
  on Linux add `--add-host=host.docker.internal:host-gateway`.
- Ollama must listen on more than `127.0.0.1` to be reachable from the container (`OLLAMA_HOST=0.0.0.0`).

To keep the logs, mount a volume on `/var/lib/azure_ai_gateway/logs`. It must be writable by uid `65532`.

## Configuration

Every setting is a command line flag and an environment variable. `cargo run -- --help` lists them all.

| Flag | Environment variable | Default | What it does |
|---|---|---|---|
| `--port` | `AZURE_AI_GATEWAY_PORT` | `8135` | Port to listen on |
| `--backend` | `AZURE_AI_GATEWAY_BACKEND` | `ollama` | `ollama` or `anthropic` |
| `--api-key` | `AZURE_AI_GATEWAY_API_KEY` | none | Key callers must send (`api-key` header or `Authorization: Bearer`) |
| `--timeout` | `OLLAMA_TIMEOUT` | `600` | Seconds a backend request may take, including streaming (either backend) |
| `--max-body-mb` | `AZURE_AI_GATEWAY_MAX_BODY_MB` | `32` | Largest request body accepted |
| `--log-config` | `AZURE_AI_GATEWAY_LOG_CONFIG` | `log-config.yaml` | log4rs configuration file |
| `--request-log` | `AZURE_AI_GATEWAY_REQUEST_LOG` | `summary` | `off`, `summary`, or `full` (adds prompts and answers) |
| `--tools-deny` | `AZURE_AI_GATEWAY_TOOLS_DENY` | none | Regex of tool names to drop from every chat request |
| `--loop-max` | `AZURE_AI_GATEWAY_LOOP_MAX` | `3` | Identical chat requests allowed per minute; `0` turns it off |

Ollama backend:

| Flag | Environment variable | Default | What it does |
|---|---|---|---|
| `--ollama-url` | `OLLAMA_URL` | `http://localhost:11434` | Ollama server to forward to |
| `--ollama-api-key` | `OLLAMA_API_KEY` | none | Sent to Ollama as a Bearer token |
| `--ollama-cloud-url` | `OLLAMA_CLOUD_URL` | `https://ollama.com` | Cloud model catalog; set to an empty string to list local models only |

Anthropic backend:

| Flag | Environment variable | Default | What it does |
|---|---|---|---|
| `--anthropic-api-key` | `ANTHROPIC_API_KEY` | none | Required |
| `--anthropic-url` | `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | Anthropic API base URL |
| `--anthropic-max-tokens` | `ANTHROPIC_MAX_TOKENS` | `16000` | Used when the caller sends no `max_tokens` |
| `--anthropic-effort` | `ANTHROPIC_EFFORT` | none | `low` to `max`, used when the caller sends no `reasoning_effort` |
| `--anthropic-no-fallbacks` | `ANTHROPIC_NO_FALLBACKS` | off | Don't re-run declined requests on another model |
| `--anthropic-cache-diagnostics` | `ANTHROPIC_CACHE_DIAGNOSTICS` | off | Ask Anthropic why a prompt missed the cache |

## Good to know

- **Secure it before exposing it.** The gateway binds to all network interfaces and accepts any caller
  unless you set `--api-key`. On the Anthropic backend, anyone who can reach it spends your API credits.
- **Deployment names are full Ollama model names**, tag included (`llama3.1:8b`).
- **Ollama cloud models** are listed too, with `:cloud` appended. Chatting with them only works when your
  Ollama is signed in to ollama.com.
- **PDFs and text files** attached to a chat are supported on both backends. For Ollama the text is
  extracted and inlined; Claude receives the document itself.
- **Token savings.** Tools are sorted by name on every request so that prompt caching keeps working, and
  `--tools-deny` removes tools you don't need. A chat request repeated more than `--loop-max` times within
  a minute gets a 429 instead of being sent to the backend again.

## Logs

Logs go to stdout and to `logs/azure_ai_gateway.log`. Request headers and bodies are logged at debug
level, which `log-config.yaml` enables by default, with API keys redacted.

`logs/requests.jsonl` holds one JSON line per chat request: model, status, timings, token usage, cache
hits and an estimated cost for Anthropic. For example, requests and spend per day:

```bash
jq -s -c 'group_by(.ts_ms / 86400000 | floor) | map({day: (.[0].ts_ms / 1000 | strftime("%Y-%m-%d")), requests: length, cost_usd: (map(.cost_usd // 0) | add)})' logs/requests.jsonl
```

With `--request-log full` the file also contains the prompts and answers, so treat it as sensitive.

## Development

```bash
cargo build
cargo test      # unit tests only; no Ollama or API key needed
cargo clippy
cargo fmt
```

[CLAUDE.md](CLAUDE.md) describes the architecture, the routes and the translation details.

## License

[Apache License 2.0](LICENSE)
