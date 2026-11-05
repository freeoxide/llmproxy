# Research Notes

These notes summarize compatibility lessons from the projects under `ref/`.
They are design input, not a claim that every provider or endpoint is already
implemented.

Some claims here are known to be wrong. Read the
[research audit](audit-2026-11-05/REPORT.md) alongside these notes until
[plan 11](../plans/11-research-corrections.md) is done.

The current proxy exposes OpenAI Chat Completions and Anthropic Messages client
routes. Upstream adapters support OpenAI Chat Completions, Anthropic Messages,
OpenAI Responses, and Gemini GenerateContent. The notes also cover useful
extension points for native providers and additional endpoint families.

## Contents

- [Core protocol and lifecycle](core-protocol.md) — normalization, streaming,
  tools, usage, routing, retries, and shutdown.
- [Provider compatibility](providers.md) — provider-specific authentication,
  request shaping, response repair, and stable OpenCode configuration.
- [Extended endpoints](endpoints.md) — embeddings, rerank, realtime, audio,
  images, video, files, OCR, batch, vector stores, and search.
- [Competitive gap analysis](competitive-gap-analysis.md) — October 2026
  capability comparison against 40+ LLM gateways and routing products across
  two research waves, with prioritized gaps and near-term candidates.
- [Competitive landscape compendium](competitive-landscape-compendium.md) —
  consolidated record of the three-wave competitive research: market map,
  capability areas, complete gap register (cross-referencing the wave-3
  candidate gaps), implementation blueprints, and a coverage appendix for
  all 90 research items.
- [Niche repository survey](niche-repos.md) — third-wave (November 2026)
  deep dives into 23 niche repositories — routers, caches, queue
  dispatchers, DLP proxies, subscription adapters — each mapped to the gap
  register, with a coverage appendix of all 121 deduplicated finds.
- [Product registry](repos-registry.json) — machine-readable registry of
  the products researched in depth across the three October waves (72
  products, 859 capability rows), each capability with its evidence URL and
  gap-register mapping. Theme, sweep, and frontier findings are folded into
  the products they cover, and the 98 uninstantiated wave-3 finds stay in
  the niche survey's appendix — hence 72 product rows against 90 inventory
  items. The November wave 4 is recorded separately (below) and is
  deliberately not folded in: its machine-readable output is the
  compensation backlog, whose rows are crate-mapped candidates rather than
  capability rows.
- [Repository intelligence](repo-intelligence.md) — fourth-wave (November
  2026) harvest of 63 repositories: 23 full four-angle records (code,
  issues, pulls, gaps) and 40 issue-tracker sweeps, each with evidence,
  plus a pain-point index, a direction index, recurring unmet needs
  observed in three or more repositories, and the stated rule mapping
  record findings to backlog candidates.
- [Compensation backlog](compensation-backlog.json) — machine-readable,
  priority-sorted backlog of 38 features and fixes implied by the
  fourth-wave intelligence (12 high, 19 medium, 7 low), every candidate
  backed by evidence carried in the repository-intelligence record and
  assigned to the workspace crate where it would land.

The normative protocol direction remains in
[`protocol.md`](../protocol.md). This directory records edge cases that should
influence implementations and tests.

## How to use these notes

When adding a protocol or provider:

1. Add or extend an endpoint-family core instead of translating one provider
   directly into another.
2. Treat streaming as a state machine, not a sequence of independent chunks.
3. Make lossy translation, parameter dropping, and synthetic data observable.
4. Keep provider quirks at the adapter or transport edge.
5. Add fixtures for malformed, partial, streaming, tool-call, and usage cases.

Provider catalogs and prices are intentionally not copied here. They change
too frequently; use live discovery or maintained provider configuration
instead.
