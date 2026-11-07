# @freeoxide/llm-proxy (Node)

Node bindings for the llm-proxy engine: translation between wire dialects,
request execution through the full pipeline, streaming events with
cancellation, and configuration from code. The native module wraps the
shared bridge crate (`crates/llm-proxy-bridge`) and adds no logic of its own.

The package name is scoped because the unscoped `llm-proxy` name on npm
belongs to an unrelated package.

## Install

The published package ships prebuilt binaries per platform, so no Rust
toolchain is needed at install time:

```
npm install @freeoxide/llm-proxy
```

Binaries for macOS and Windows are built by CI runners with those
toolchains; this repository's local builds produce the Linux binary.

## Use

```js
const { Proxy, ProxyError } = require('@freeoxide/llm-proxy');

const proxy = new Proxy({
  providers: [
    {
      name: 'acme',
      api_key: 'sk-...',
      auth_style: 'bearer',
      adapters: {
        messages: {
          protocol: 'anthropic_messages',
          endpoint: 'https://api.example.com/v1/messages',
        },
      },
      routes: { messages: 'messages' },
      model_aliases: { 'claude-sonnet-4-5': 'claude-sonnet-4-5-20250929' },
    },
  ],
  record_usage: true,
});

const reply = await proxy.execute('acme', 'anthropic_messages', requestBody);

const stream = await proxy.executeStream('acme', 'anthropic_messages', streamBody);
for await (const event of stream) {
  // event objects; break or cancel to stop upstream
}
await stream.cancel();

const translated = proxy.translateRequest(body, 'anthropic_messages', 'openai_chat_completions');
```

Dialect names: `anthropic_messages`, `openai_chat_completions`,
`openai_responses`, `gemini_generate_content`, `ollama_chat`,
`ollama_generate`. Provider protocols: `anthropic_messages`,
`openai_chat_completions`, `openai_responses`, `gemini_generate_content`.

Failures throw `ProxyError` with `kind`, `status`, `message`,
`retryAfterSeconds` and `shouldRetry`.

## Build and test

```
npm install
npm run build
npm test
```

The test suite replays the plan-02 contract fixtures from
`bindings/shared/cases.json` and compares against the snapshots the Rust
bridge test generates: the same fixtures must give the same results in Rust
and Node.
