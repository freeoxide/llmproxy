# Research Notes

These notes summarize compatibility lessons from the projects under `ref/`.
They are design input, not a claim that every provider or endpoint is already
implemented.

The false and misleading claims found by the
[research audit](audit-2026-11-05/REPORT.md) were corrected in place on
2026-11-08 (plan 11); corrections are marked inline with their date. Where a
note describes llm-proxy's own code, it describes the tree at the audited
commit (`79cf406`, 2026-11-05) — plans 01–10 have since rebuilt most of it,
and current behaviour lives in [`docs/goal.md`](../goal.md) and
[`docs/completed/`](../completed/).

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
  the products researched in depth across the three waves, October and
  November 2026 (72
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
  record findings to backlog candidates. Read it as a catalogue of
  possibilities, not a ranking of demand: its "How strong the demand
  evidence is" section carries the audit's caveats.
- [Compensation backlog](compensation-backlog.json) — machine-readable,
  priority-sorted backlog of the features and fixes implied by the
  fourth-wave intelligence, every candidate
  backed by evidence carried in the repository-intelligence record and
  assigned to the workspace crate where it would land. Corrected
  2026-11-08 per the audit: rescoped or restated rationales, dropped
  candidates (#12, #29, #33) with reasons, the ten items the audit found
  missing, and each candidate's consuming plan number.
- [Harvest manifest](harvest-manifest.json) — the wave-4 harvest plan: 120
  listed repositories against the 63 actually harvested (the 23 full-angle
  clones plus the 40 issue-tracker sweeps). Listed-but-not-harvested
  repositories were never read.

## Evidence re-checkability

Two limits on rechecking this research from a clean checkout (noted
2026-11-08 per the audit):

- 93 evidence paths in the repository-intelligence record are
  clone-relative (`crates/gateway/src/...`, `providers/base/common.go`, …)
  pointing into the wave's clones. The clones (`ref/*/`, `research-repos/`)
  are gitignored and absent, and `/tmp/intel-mistral` is gone, so those
  paths cannot be rechecked here; the GitHub URLs alongside them can.
- The wave-4 clones carry no commit pins (mistral.rs at `52b3c49` is the
  one exception), so even a re-clone may not match the cited lines. 62 of
  the backlog's original 162 evidence entries are such paths.

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
