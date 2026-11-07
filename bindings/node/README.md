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

The local build produces the Linux binary
(`llm-proxy.linux-x64-gnu.node`). A Windows x64 binary
(`llm-proxy.win32-x64-gnu.node`) is produced by mingw-w64 cross-compilation:
the napi import library is generated from the official `node.exe` exports
(objdump export table -> dlltool), then

    RUSTC_WRAPPER= LIBNODE_PATH=<dir with libnode.dll> \
      cargo build --release --target x86_64-pc-windows-gnu

(`RUSTC_WRAPPER=` disables the host's rustc cache, which does not handle
cross-compiler names.) The Windows binary links against `node.exe`'s
`napi_*` exports; `index.js` falls back to it when the generated loader's
msvc-first path finds no msvc binary (official node.exe is MSVC-built, and
the gnu binary loads there too — the napi boundary is a plain C ABI).
Run-verified under wine 10.0 with the official node-v20.19.2 win-x64
`node.exe`:

    wine <node.exe> test/bindings.test.js   # 13/13 with llm-proxy.win32-x64-gnu.node

An msvc build and macOS binaries are left to CI runners with those
toolchains.

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
