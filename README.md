# llm-proxy

A provider-scoped LLM proxy with OpenAI Chat Completions and Anthropic
Messages client endpoints, normalized protocol translation, streaming, model
catalogs, and optional live model discovery.

## Layout

~~~
apps/llm-proxy                 daemon and CLI (binary: llm-proxy)
crates/llm-proxy-core          config, routing, registry, metrics
crates/llm-proxy-protocol      OpenAI / Claude / Gemini wire types
crates/llm-proxy-adapters      provider codecs (no I/O)
crates/llm-proxy-transport     HTTP egress, credentials, discovery
crates/llm-proxy-engine        request lifecycle (no HTTP) + traits
crates/llm-proxy-storage       usage ledger
crates/llm-proxy-api           read-only /usage API
crates/llm-proxy-server        axum routes and error mapping
crates/llm-proxy-sdk           facade crate (published as llm-proxy-sdk)
~~~

## Build

~~~
cargo build --release
~~~

## Run

~~~
./target/release/llm-proxy validate --config ./config.toml
./target/release/llm-proxy serve --config ./config.toml
~~~

Every API request names its provider in the path:

~~~text
POST /providers/{provider}/v1/chat/completions
POST /providers/{provider}/v1/messages
POST /providers/{provider}/v1/messages/count_tokens
GET  /providers/{provider}/v1/models
~~~

See `config.toml.example` and the files under `providers/` for configuration
examples.

See the [documentation index](docs/README.md) for the project goal, plans,
protocol design and research.

## References

These shaped the layout and scope:

- LeenHawk/gproxy
- modpotatodotdev/LLMG
- x5iu/openproxy
- habibi-dev/rust-llm-proxy
