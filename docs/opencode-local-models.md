# OpenCode with Local / Network Models

Guide for pointing [OpenCode](https://opencode.ai) at models running on your own machine or LAN. OpenCode is model-agnostic — any OpenAI-compatible endpoint works.

## Config location

`~/.config/opencode/opencode.json`

All providers go under the `provider` key. Use `/models` inside OpenCode to hot-swap mid-session.

## Option 1: Ollama

Easiest on-ramp. Bind to the network so other machines can reach it:

```bash
OLLAMA_HOST=0.0.0.0:11434 ollama serve
ollama pull qwen2.5-coder:32b
```

Config:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "ollama": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "Ollama (LAN)",
      "options": {
        "baseURL": "http://192.168.1.50:11434/v1"
      },
      "models": {
        "qwen2.5-coder:32b": { "name": "Qwen 2.5 Coder 32B" },
        "deepseek-coder-v2:16b": { "name": "DeepSeek Coder V2 16B" }
      }
    }
  }
}
```

## Option 2: vLLM (recommended for agents)

Better throughput than Ollama because of batched/concurrent request handling — matters when OpenCode agents fan out tool calls.

Launch the server:

```bash
vllm serve Qwen/Qwen2.5-Coder-32B-Instruct \
  --host 0.0.0.0 --port 8000 \
  --api-key local-dummy \
  --enable-auto-tool-choice \
  --tool-call-parser hermes \
  --max-model-len 32768
```

Critical flags:

- `--enable-auto-tool-choice` + `--tool-call-parser` — **required** for OpenCode's tool-using agents. Parser depends on the model (`hermes`, `llama3_json`, `mistral`, etc.).
- `--max-model-len` — agent sessions burn tokens fast, set this as high as the model supports.
- Model ID must match the HF repo path exactly. Verify with `curl http://host:8000/v1/models`.

Config:

```json
{
  "provider": {
    "vllm": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "vLLM",
      "options": {
        "baseURL": "http://192.168.1.50:8000/v1",
        "apiKey": "local-dummy"
      },
      "models": {
        "Qwen/Qwen2.5-Coder-32B-Instruct": { "name": "Qwen2.5 Coder 32B" }
      }
    }
  }
}
```

## Option 3: LM Studio / llama.cpp server

Both expose `/v1` OpenAI-compatible endpoints. Same shape as above — just change `baseURL`:

- LM Studio: `http://host:1234/v1`
- llama.cpp: `http://host:8080/v1` (start with `llama-server --host 0.0.0.0`)

## Multi-provider setup for model comparison

You can register several providers side-by-side and flip between them with `/models`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "vllm": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "vLLM",
      "options": { "baseURL": "http://192.168.1.50:8000/v1", "apiKey": "local" },
      "models": {
        "Qwen/Qwen2.5-Coder-32B-Instruct": { "name": "Qwen2.5 Coder" }
      }
    },
    "ollama": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "Ollama",
      "options": { "baseURL": "http://192.168.1.50:11434/v1" },
      "models": {
        "devstral:latest": { "name": "Devstral" },
        "gpt-oss:20b": { "name": "GPT-OSS 20B" }
      }
    },
    "openrouter": {
      "npm": "@openrouter/ai-sdk-provider",
      "name": "OpenRouter",
      "options": { "apiKey": "{env:OPENROUTER_API_KEY}" },
      "models": {
        "anthropic/claude-sonnet-4.5": { "name": "Claude Sonnet 4.5" }
      }
    }
  }
}
```

## Agent mode per model

OpenCode supports assigning different models to different agent roles (cheap/fast for planning, strong for building). Configure under `agent` in the same file — see OpenCode docs for the current schema.

## Recommended models for coding/tool-calling

- **Qwen2.5-Coder-32B-Instruct** — best all-around local coder, good tool-calling
- **DeepSeek-Coder-V2** — strong on reasoning
- **Devstral** — Mistral's agent-tuned coder
- **GPT-OSS 20B/120B** — solid tool-calling

Smaller models (<14B) generally struggle with OpenCode's multi-step agent loop.

## Troubleshooting

- **Agents do nothing / immediately finish** — tool-calling isn't wired up. For vLLM, check `--enable-auto-tool-choice` and matching `--tool-call-parser`. For Ollama, confirm the model tag supports tools.
- **Connection refused from other machines** — server is bound to `127.0.0.1`. Rebind to `0.0.0.0`.
- **Context errors** — raise `--max-model-len` (vLLM) or `num_ctx` (Ollama `Modelfile`).
- **Model not found** — `curl $baseURL/models` and copy the ID exactly into config.
