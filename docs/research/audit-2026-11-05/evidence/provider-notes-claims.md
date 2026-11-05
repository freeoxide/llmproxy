# Fact audit: providers.md, core-protocol.md, endpoints.md (as of 2026-11-05)

Method: vendor docs fetched live (WebFetch, may be summarised by a small model), LiteLLM source at pin b84f7f8 (files pulled with `gh api ... ?ref=<sha>`, grepped locally at scratchpad/audit/providers/llm/src), live unauthenticated probes of opencode.ai and AWS.
Evidence tags: [V]=vendor doc/live probe, [L]=LiteLLM source only (shows what LiteLLM does, not that the vendor requires it).
Paths: P=docs/research/providers.md, C=core-protocol.md, E=endpoints.md.

| # | Loc | Claim | Verdict | Evidence |
|---|-----|-------|---------|----------|
| 1 | P:12 | Native Anthropic Messages rejects empty text blocks | CONFIRMED | [L] anthropic/chat/transformation.py:1639 "Anthropic API raises errors for empty text"; community 400 "text content blocks must be non-empty". No official Anthropic doc found stating it. |
| 2 | P:12 | Invalid replayed thinking blocks are rejected | CONFIRMED (+new) | [V] https://platform.claude.com/docs/en/api/errors "`thinking` blocks ... cannot be modified". New since: Claude 5.1+ prefix check, 400 "bound to a different conversation", `thinking.block_binding.prefix_mismatch_behavior` + beta `thinking-binding-controls-2026-08-01`. |
| 3 | P:16 | Raw `output_config`/beta/cache fields must not leak to non-Anthropic | CONFIRMED | [L] anthropic/chat/transformation.py:228,1546 drop unsupported output_config. |
| 4 | P:19 | Legacy thinking, adaptive thinking, effort need model-aware handling | CONFIRMED | [V] errors page: 4.7+ reject `thinking.type=enabled` ("Use thinking.type.adaptive and output_config.effort"); 4.5- reject adaptive; Sonnet 5.5 uses `between_tools`; effort lives at `output_config.effort`, levels low/medium/high/xhigh/max. |
| 5 | P:21 | OAuth tokens use bearer auth + provider beta headers, not x-api-key | PARTIAL | [L] anthropic/common_utils.py:45-70: sk-ant-oat token -> `Authorization: Bearer`, drops x-api-key, adds oauth beta + `anthropic-dangerous-direct-browser-access`. [V] Anthropic beta-headers page does not document any OAuth beta; Anthropic policy forbids third-party use of subscription OAuth (see policy section). Mechanics true, undocumented, policy-restricted. |
| 6 | P:23 | Anthropic-to-Responses bridge: thinking->reasoning, structured output->text.format, context edits->compaction | UNVERIFIABLE | Not checked. |
| 7 | P:26 | SAP/Vertex/Azure/Bedrock Claude add own auth/header/version/param limits | PARTIAL | [V] beta-headers page: Bedrock InvokeModel reads betas from body `anthropic_beta`; Google-managed Vertex reads one `anthropic-beta` header only. SAP/Azure parts not checked. |
| 8 | P:31 | Bedrock needs SigV4, region, ARN, role, credential refresh | CONFIRMED | [L] bedrock/common_utils.py:440,466 RoleArn/AssumeRole. |
| 9 | P:37 | Invoke and Converse rewrite request shapes differently | CONFIRMED | [L] bedrock/chat/{converse_transformation,invoke_transformations}. |
| 10 | P:38 | Structured output may be prompt-inlined (Invoke) | CONFIRMED | [L] bedrock/common_utils.py:70-74 "Invoke paths convert the schema to inline message text". |
| 11 | P:39 | Guardrails may require `guarded_text` content blocks | PARTIAL | [V] AWS Converse native name is `guardContent` {text:{text,qualifiers}} https://docs.aws.amazon.com/bedrock/latest/userguide/guardrails-use-converse-api.html . `guarded_text` is a LiteLLM/OpenAI-side content type (factory.py:4699), mapped to guardContent. Doc presents LiteLLM's name as the provider requirement. |
| 12 | P:40 | Model IDs need ARN / cross-region prefix normalisation | CONFIRMED | [L] bedrock/common_utils.py:579 prefixes ["global","us","eu","apac","jp","au","us-gov"], :621 ARN handling. |
| 13 | P:42 | Converse requestMetadata has limits | CONFIRMED | [L] converse_transformation.py:230 max 16 items. |
| 14 | P:43 | Invoke Agent is session/trace protocol | CONFIRMED | [L] bedrock/chat/invoke_agent; AWS API_agent-runtime_InvokeAgent. |
| 15 | P:45 | SageMaker may use HF templates + `inputs`/`parameters` | CONFIRMED | [L] sagemaker/completion/transformation.py:4,160 (TGI format, prompt_factory); embedding "inputs". |
| 16 | P:46 | SageMaker temperature/token floors | PARTIAL | [L] transformation.py:92-96 temperature must be strictly positive -> 0.01. Token floor not found. |
| 17 | P:54 | Preserve embedded api-version; normalise v1 path without duplicating /openai | CONFIRMED | [L] azure/common_utils.py:497,820-825. [V] MS Learn: v1 API, `api-version` "no longer a required parameter", base `https://<res>.openai.azure.com/openai/v1/`. |
| 18 | P:56 | Azure API key and AD bearer | CONFIRMED | [V] https://learn.microsoft.com/en-us/azure/ai-foundry/openai/api-version-lifecycle : `api-key` header or `Authorization: Bearer`, token scope `https://ai.azure.com/.default`. |
| 19 | P:57 | Drop unsupported Responses `temperature` on Azure | UNVERIFIABLE | grep of azure/responses/transformation.py found nothing. |
| 20 | P:59 | Azure Anthropic headers separate | UNVERIFIABLE | Not checked. |
| 21 | P:61 | Assistants/AI Agents are thread/run workflows | UNVERIFIABLE | Not checked; OpenAI Assistants API deprecation/sunset (announced for 2026) may make this STALE. Unconfirmed. |
| 22 | P:68 | response_schema vs response_json_schema by model; | CONFIRMED | [L] vertex_ai/common_utils.py:206, vertex_and_google_ai_studio_gemini.py:858-886. [V] Google ref: responseSchema "internal detail. Use responseJsonSchema". |
| 23 | P:68 | Preserve propertyOrdering for Vertex | CONFIRMED | [L] vertex_ai/common_utils.py:538-583. |
| 24 | P:73 | file_data / inline_data parts | CONFIRMED | [V] https://aistudio.google.com/api-reference/generate-content : inlineData "Inline media bytes", fileData "URI based data". |
| 25 | P:75 | Standalone usage frames buffered | UNVERIFIABLE | Not checked. |
| 26 | P:77 | Translate `citationSources` | CONFIRMED | [V] same Google ref: citationMetadata.citationSources. [L] gemini/google_genai/transformation.py:416. |
| 27 | P:80 | Vertex partner models: publisher URLs, drop model from body | UNVERIFIABLE | Not checked. |
| 28 | P:87 | max_completion_tokens vs max_tokens | CONFIRMED | [V] OpenAI chat ref: max_tokens "deprecated in favor of max_completion_tokens", unusable on o-series. |
| 29 | P:93 | Responses completed/failed/incomplete distinct | PARTIAL | [V] completed + failed confirmed (SDK union, Perplexity schema). `incomplete` not shown on pages I could read. |
| 30 | P:104 | OpenRouter route/model/transforms in extra_body | CONFIRMED | [L] openrouter/chat/transformation.py:63-72. |
| 31 | P:105 | OpenRouter usage cost "may require opt-in fields" | STALE | [V] https://openrouter.ai/docs/use-cases/usage-accounting : "`usage: { include: true }` ... deprecated and have no effect"; usage always returned. LiteLLM pin still sends it (transformation.py:171-174). |
| 32 | P:106 | Vercel AI Gateway options under extra_body | CONFIRMED | [L] vercel_ai_gateway/chat/transformation.py:29-61 (providerOptions -> extra_body). No vendor doc read. |
| 33 | P:107 | Perplexity HTTP 200 with failed Responses object | CONFIRMED | [L] perplexity/responses/transformation.py:7,116-128. [V] docs.perplexity.ai ResponsesResponse has `status: failed` + `error` (200 not stated explicitly). |
| 34 | P:108 | Perplexity citations are side-channel | UNVERIFIABLE | Not checked. |
| 35 | P:109 | Volcengine Responses events need missing fields filled | CONFIRMED | [L] volcengine/responses/transformation.py:239,254. |
| 36 | P:111 | Databricks: strip prefixes, Responses stays HTTP | UNVERIFIABLE | No matching logic found in databricks/chat. |
| 37 | P:112 | Copilot/ChatGPT: synthesize headers and account identity | PARTIAL | [L] github_copilot/authenticator.py:214 (user-agent "GithubCopilot/1.155.0"), chatgpt/authenticator.py:74-79 (account_id). True mechanically; doc silent on vendor terms (see policy). |
| 38 | P:118 | Ollama uses /api/chat | CONFIRMED | [V] https://docs.ollama.com/api/chat |
| 39 | P:119 | Ollama sometimes `<think>` stream boundaries | PARTIAL | [V] Ollama now has `think` request param and separate `message.thinking` field; `<think>` parsing is only a fallback ([L] ollama/chat/transformation.py:190,284). |
| 40 | P:120 | Hosted vLLM placeholder credential | CONFIRMED | [L] hosted_vllm/embedding/transformation.py:56 "fake-api-key". "Caller-provided URLs" not checked. |
| 41 | P:122 | LM Studio placeholder auth + json_schema rewrite | CONFIRMED | [L] lm_studio/chat/transformation.py:18,30-31. |
| 42 | P:124 | HF/TGI pick envelopes from task metadata | PARTIAL | [L] huggingface/common_utils.py:30 hf_tasks. Streaming reconstruction not checked. |
| 43 | P:126 | Heroku flattens list content | CONFIRMED | [L] heroku/chat/transformation.py:40-41. Path completion not checked. |
| 44 | P:136 | Mistral: sanitise schemas; collapse content-list to text+reasoning | PARTIAL | [L] mistral/chat/transformation.py:28,36,115 (content-list->str, thinking block, reasoning_effort). Schema sanitising not found. |
| 45 | P:138 | DeepInfra flatten tool msgs, constrain tool_choice, unwrap errors | UNVERIFIABLE | Only saw tool_choice param. |
| 46 | P:140 | NIM model-specific allowlists | CONFIRMED | [L] nvidia_nim/chat/transformation.py:21. |
| 47 | P:142 | Cohere tool_results history; text vs image embeddings | CONFIRMED | [L] cohere/chat/transformation.py:64,85; cohere/embed. |
| 48 | P:144 | WatsonX tool-choice forms, deployment URLs, templates | CONFIRMED | [L] watsonx/chat/transformation.py:32,41; embed:63; common_utils.py:8,224. |
| 49 | P:146 | GigaChat/Ollama structured output via tool calls or JSON parsing | PARTIAL | [L] gigachat/__init__.py:9 confirms GigaChat (function-call emulation). Ollama has native `format`; claim unverified for it. |
| 50 | P:148 | Baseten model identity selects dedicated host | PARTIAL | [L] baseten/chat.py:92 default `https://inference.baseten.co/v1`; dedicated-host path not confirmed. |
| 51 | P:160 | Go base `https://opencode.ai/zen/go/v1` | STALE | [V] live: `/zen/go/v1/models` HTTP 200 today, but OpenCode docs (https://opencode.ai/docs/go/) now publish `https://opencode.ai/inference/go/openai/v1/chat/completions`, `/inference/go/anthropic/v1/messages`, `/inference/go/v1/models` (all live). Legacy path works but is no longer documented. |
| 52 | P:161 | Zen base `https://opencode.ai/zen/v1` | STALE | [V] docs/zen now: `/inference/openai/v1/{chat/completions,responses}`, `/inference/anthropic/v1/messages`, `/inference/google/v1beta/models/<id>`, `/inference/v1/models`. Legacy `/zen/v1/*` still routes (400 "`model` must be a non-empty string"). |
| 53 | P:168 | GET {base}/models | CONFIRMED | [V] both bases HTTP 200, unauthenticated, OpenAI-shaped list (80 models on Zen). |
| 54 | P:169 | POST {base}/chat/completions | CONFIRMED | [V] 400 on empty body from both = route exists. |
| 55 | P:170 | POST {base}/messages | CONFIRMED | [V] Anthropic-shaped 400 from both. |
| 56 | P:171 | POST {zen-base}/responses (Zen only) | PARTIAL | [V] Go also answers `/zen/go/v1/responses` (400) and docs/go lists a Go Responses endpoint. Understated. providers/opencode-go.toml.example has no responses adapter. |
| 57 | P:172 | POST {zen-base}/models/{model}:generateContent | PARTIAL | [V] route alive (400 "Model is unavailable"); docs path is now `/inference/google/v1beta/models/<id>`; docs/go also lists a Gemini endpoint for Go. |
| 58 | P:179 | `/models` contains limited metadata | STALE | [V] live response keys: api, capabilities (tools, reasoning.effort, structured_output, ...), cost, limit, modalities, family, release_date, knowledge, open_weights, canonical_id on Go. Rich, not limited. The `.toml.example` files match the legacy URLs. |
| 59 | C:69 | Finish reason may precede usage; usage in own final chunk | CONFIRMED | [V] OpenAI ref: include_usage sends "one extra final chunk" before stream end. |
| 60 | C:75 | [DONE] vs message_stop vs Responses terminal vs Gemini close | PARTIAL | Not re-fetched for Anthropic/Gemini; Responses SDK has ResponseFailedEvent/CompletedEvent. |
| 61 | C:111 | Structured output may be a synthetic forced tool call | STALE | [V] errors page: Opus 5.5, Sonnet 5.5, Fable 5.1, Mythos 5.1 return 400 for `tool_choice` any/tool. Native `output_config.format` structured outputs exist. |
| 62 | C:125 | Signed thinking replay required by some providers | CONFIRMED | [V] errors page, see #2. |
| 63 | C:159 | `stream_options.include_usage` opt-in | CONFIRMED | [V] OpenAI chat ref. |
| 64 | C:159 | `usage.include` opt-in | STALE | [V] OpenRouter: deprecated, no effect (see #31). |
| 65 | C:160 | Token-count endpoints need own bridge/headers | CONFIRMED | [V] errors page references Anthropic token counting endpoint. |
| 66 | C:153-157 | Usage has separate cached/reasoning counts | CONFIRMED | [V] Anthropic `usage.output_tokens_details.thinking_tokens` (final message_delta when streaming). |
| 67 | C:170 | OAuth subscriptions are routing state (track credentials) | PARTIAL | Framing omits that Anthropic/Google state third-party use is prohibited (see policy). |
| 68 | E:19 | `input`->`inputs` | CONFIRMED | [L] sagemaker/embedding/transformation.py:83-84. |
| 69 | E:19 | `dimensions`->`output_dimension` | CONFIRMED | [L] cohere/embed/transformation.py:58. |
| 70 | E:29 | `top_n`->`top_k` | UNVERIFIABLE | Saw top_n handling (HF, Bedrock numberOfResults); no top_k mapping located. |
| 71 | E:30 | Bedrock rerank uses agent-runtime host | CONFIRMED | [V] live: POST bedrock-agent-runtime.us-east-1.amazonaws.com/rerank -> 403 (op exists); bedrock-runtime.../rerank -> 404 UnknownOperationException. [L] bedrock/rerank/handler.py:150. |
| 72 | E:70 | Deepgram query-param bridge | CONFIRMED | [L] deepgram/audio_transcription/transformation.py:2 (OpenAI -> /v1/listen). |
| 73 | E:70 | Riva is gRPC | CONFIRMED | [L] nvidia_riva/* reference grpc. |
| 74 | E:109 | Anthropic Files uses beta headers | STALE | [V] https://platform.claude.com/docs/en/build-with-claude/files : "The Files API is out of beta and needs no beta header" (`files-api-2025-04-14` optional legacy). |
| 75 | E:109 | Anthropic Skills uses beta header | UNVERIFIABLE | [L] only (anthropic/skills/transformation.py:53). Vendor status not checked. |
| 76 | E:109 | Gemini resumable upload is multi-step | CONFIRMED | [L] gemini/files/transformation.py:81,119 "two-step upload". |
| 77 | E:71 | Azure Speech, Polly, ElevenLabs bridges | UNVERIFIABLE | Not checked. |

Not counted: advice-only sentences (adapter checklists, MCP/orchestration paragraph at C:118, "do not add one-off transforms"). MCP sentence is a design statement, not a checkable fact.

## Missing current behaviour (primary-sourced)
- Anthropic caching: up to 4 explicit `cache_control` breakpoints; top-level automatic `cache_control`; `ttl` "5m" default or "1h"; min cacheable tokens vary by model (512-4096); changing thinking config or effort invalidates cache. https://platform.claude.com/docs/en/build-with-claude/prompt-caching
- Anthropic 400s the docs never mention: temperature/top_p/top_k rejected on 4.7+ and API version 2026-10-21; assistant prefill rejected on 4.6+; `thinking: disabled`/`enabled` rejected on newer models; forced `tool_choice` rejected on 5.x; mid-conversation `system` role messages. https://platform.claude.com/docs/en/api/errors , .../working-with-messages
- OpenAI: `prompt_cache_key` exists; `prompt_cache_retention` deprecated in favour of `prompt_cache_options.ttl`. https://developers.openai.com/api/docs/api-reference/chat/create
- Azure v1 API: `/openai/v1/`, no api-version needed. (doc partly covers.)
- Not confirmed from a primary source: Responses `reasoning.encrypted_content`, Gemini thought-signature requirements (the Google page I reached only said signatures live in "steps"; details on the linked thinking guide not read).

## Subscription-OAuth policy (facts, with sources)
- Anthropic, Claude Code legal page https://code.claude.com/docs/en/legal-and-compliance : "Anthropic does not permit third-party developers ... route requests through Free, Pro, Max, or Ultra plan credentials" ; developers "may not collect, store, or intermediate Claude.ai credentials or session tokens". Use API keys via Console or a cloud provider for products. Also: "Customers may not pay for, resell, or intermediate Claude usage on their end users' behalf." Consumer Terms https://www.anthropic.com/legal/consumer-terms (via fetch summary): no access "through automated or non-human means" except via API key; no sharing credentials.
- Google, Gemini CLI ToS/privacy https://geminicli.com/docs/resources/tos-privacy/ : using third-party software "with Gemini CLI OAuth ... is a violation of applicable terms"; "grounds for suspension or termination". Antigravity: no official text found; only user forum reports of 403 ToS suspensions (anecdotal).
- OpenAI Codex auth https://learn.chatgpt.com/docs/auth (redirect from developers.openai.com/codex/auth): "Treat ~/.codex/auth.json like a password"; "API keys are still the recommended default for automation"; "For general OpenAI API calls, continue to use Platform API keys." No explicit third-party-proxy ban in that page. OpenAI Terms of Use pages returned 403 to my fetcher; a search snippet says terms prohibit programmatic extraction of Output and sharing credentials but I could not verify live text.
- GitHub: ToS Section H https://docs.github.com/en/site-policy/github-terms/github-terms-of-service : "You may not share API tokens to exceed GitHub's rate limitations." AUP https://docs.github.com/en/site-policy/acceptable-use-policies/github-acceptable-use-policies : "excessive automated bulk activity". The Copilot additional-terms page names no third-party-tool rule. I found no explicit GitHub statement on Copilot proxies.

## Prompt-injection notes
None of the fetched pages contained instructions directed at an AI agent.
