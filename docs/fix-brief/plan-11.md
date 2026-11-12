# Plan 11 — Research corrections: fix spec

Scope: a docs plan. All "code" below is documentation or comment text; the one non-doc file is
`config.toml.example` (comment only). Plan file: `docs/completed/11-research-corrections.md`.
Paths are relative to the repository root. Doc keys:
BL = `docs/research/compensation-backlog.json`, GA = `docs/research/competitive-gap-analysis.md`,
CO = `docs/research/competitive-landscape-compendium.md`, RI = `docs/research/repo-intelligence.md`,
RR = `docs/research/README.md`, CT = `config.toml.example`.

Docs rules that apply to every item (AGENTS.md): no design narratives, no "why we chose X over Y"
essays and no self-report tables in user-facing documentation. Each correction is one terse sentence
or one field edit. Keep the existing inline marker style "(corrected 2026-11-08 per research audit)"
only where the plan already uses it; do not add new banners or changelog paragraphs.

CI has no docs/link checker (`.github/workflows` has only `ci.yml` and `release.yml`), so every
"Done when" below is a grep or a JSON parse. After editing BL, confirm it still parses
(`jq . docs/research/compensation-backlog.json >/dev/null` or any JSON validator); do not use python.

## Done — leave alone

- llm-proxy claims (audit "false or misleading", llm-proxy subsection): GA:418-427 (first-byte window), CO:713/733, CO:910 (credential pools), CO:884-889 (budget enforcement), GA:474-478 (cache done-condition), GA:348-359 and CO:633-642 and RI:715-719 (rustls-only history; `crates/llm-proxy-transport/Cargo.toml` is rustls-only), GA:116-120/GA:337-340/CO:194/CO:610-612 (musl), CO:1073 and NR:29-34 (cancellation gap withdrawn).
- Other-product claims: GA:300 (mTLS), GA:329 (successor mapping), GA:345 (config history/rollback), GA:212/GA:403/CO:404/CO:690 (`cancel_on_disconnect`), GA:177 (Cloudflare budgets), CO:450-452 (MCP 2026-07-28 final), GA:446-448 (LiteLLM #35524 historical), CO:85/CO:356 (S1–S14), absolute-claim sweep.
- Paid/beta separation and restated prevalence counts: CO:382, CO:672-700, GA:382-396, CO:334, CO:619, CO:678.
- Wave 3/4 citations and demand caveats: RI:379-384, RI:1067-1072, RI:2429-2431, RI:2693-2695, RI:629-631, RI:2664-2666, RI:2635-2663, NR:357-362, NR:37-43, NR:452-458, NR:842-856.
- Provider/protocol/endpoint notes: PV:14-27, PV:57-60, PV:77-83, PV:121, PV:146, PV:200-216, CP:120-124, CP:177-179, EP:124-129 (`providers.md`, `core-protocol.md`, `endpoints.md`), all sections dated.
- Backlog: 48 `"candidate"` entries each with a `"plan"` key (45 non-null; `dropped` + `plan: null` at BL:303-304 #12, BL:678-679 #29, BL:757-758 #33); the ten appended items at BL:842-959; priority/rationale corrections for #2, #3, #5, #7, #8, #10, #11/#25, #13, #14, #18, #22, #24, #27, #28, #37, #38.
- Housekeeping: CO title and "three-wave" wording, RR "three-wave" wording and warning replaced by pointer (RR:8-14), `repos-registry.json` scope 72, CO:112-114 Upstash negative-finding note, RR harvest-manifest and evidence re-checkability disclosures, `README.md:18` `llm-proxy-api` description (matches `crates/llm-proxy-api/src/routes.rs` GET-only routes).

## Work items

### 11-1 [DOC] [P1] Fix backlog #6 `crate` field: `llm-proxy-store` does not exist
- **Where:** `docs/research/compensation-backlog.json:161` (entry "Per-key budgets and multi-window rate limits (RPM/TPM/RPD) wired to the SQLite usage ledger", candidate at line 138, `"plan": "05"`).
- **Now:** `"crate": "llm-proxy-engine + llm-proxy-store (server + storage per audit G; the old 'core' mapping was wrong)"`. The workspace crates are `llm-proxy-{adapters,admin,api,bridge,core,engine,guardrails,mcp,protocol,sdk,semantic-cache,server,storage,store-postgres,store-redis,transport}` (`ls crates/`); there is no `llm-proxy-store`. The parenthetical itself says "server + storage".
- **Wanted:** plan 11 To do 6: "apply the corrections from 'Backlog review'", whose #6 correction is "one admission model; server + storage" with the old `core` mapping dropped. Every `crate` value must name real crates.
- **How:** replace the value with `"llm-proxy-engine + llm-proxy-server + llm-proxy-storage (admission model; audit G — the old 'core' mapping was wrong)"`. `llm-proxy-engine` stays because plan 05 put the `Admission` extension trait in `crates/llm-proxy-engine/src/traits.rs`; `server` and `storage` come from the audit. Do not touch `plan`.
- **Test:** none (no docs CI). Manual check below.
- **Done when:** `grep -n 'llm-proxy-store"' docs/research/compensation-backlog.json` returns nothing, and every `llm-proxy-*` token on a `"crate":` line is in the crate list above (`grep '"crate"' docs/research/compensation-backlog.json | grep -o 'llm-proxy-[a-z-]*' | sort -u` is a subset of `ls crates/`). The file still parses as JSON.
- **Risk / notes:** pure text edit. Combine with 11-3 in one pass over the `crate` fields.

### 11-2 [DOC] [P1] Record the dropped fingerprinting part of backlog #15
- **Where:** `docs/research/compensation-backlog.json:351-352` (entry "Prompt-cache hygiene: normalize cache-busting volatile bytes (client correlation tokens, per-turn reminder blocks) …", `"plan": "06"`, crate line 372). Cross-refs: `docs/README.md:31-35` ("Not planned": "Subscription OAuth and client fingerprinting (backlog 12, part of backlog 15, conflict 18)"), `docs/goal.md` Credentials ("Client fingerprinting and impersonation are not built"), `docs/completed/06-cache-and-dedup.md:30-33` ("… excludes fingerprinting") and its Done text ("No fingerprinting").
- **Now:** the BL #15 rationale says "sub2api engineered fingerprint convergence for the same effect" as supporting evidence and the candidate keeps "client correlation tokens" normalisation, with no statement that any part is dropped. `docs/README.md` says part of #15 is not planned. The two records disagree. (BL #12 at line 303 already carries a `dropped` reason; #15 has none because it is only partly dropped.)
- **Wanted:** plan 11 Done when: "Every backlog candidate names a plan, or says why it's dropped." For a partial drop the entry must say which part and why.
- **How:** (1) Append one sentence to the `rationale` of #15: "Partly dropped: matching a vendor client's fingerprint (sub2api's fingerprint convergence) is not built (docs/goal.md Credentials; docs/README.md 'Not planned'); plan 06 delivers only operator-configured removal of volatile bytes and marker injection." (2) Leave `plan: "06"`, `priority` and `crate` unchanged. Do not add a new JSON key (other tooling in this repo reads only `candidate/rationale/area/evidence/repos/priority/crate/plan/dropped`; a new key is unneeded). (3) Leave `docs/README.md` wording as is; it is now consistent with BL.
- **Test:** none.
- **Done when:** `grep -n -i 'fingerprint' docs/research/compensation-backlog.json` shows, inside the #15 entry, the "Partly dropped" sentence naming `docs/goal.md` and `docs/README.md`; JSON still parses.
- **Risk / notes:** keep the sentence factual; no narrative about why the vendor terms matter (that belongs to goal.md).

### 11-3 [DOC] [P1] Make `corrections_note` and the `crate` fields agree about crate names
- **Where:** `docs/research/compensation-backlog.json:963` (`corrections_note`, last key); stale crate values at BL:33, BL:86, BL:110, BL:302 (and nowhere else on a `"crate"` line: BL:134, 161, 181, 206, 254, 326, 372 already use post-split names).
- **Now:** the note says: "Plan 03 restructured the workspace crates (llm-proxy-provider split into adapters + transport; llm-proxy-engine added), so 'crate' keeps the audit-corrected, pre-restructure name and 'plan' is the authoritative landing spot." But the entries at BL:326/372 (`llm-proxy-adapters`), BL:206/254/490/910 (`llm-proxy-transport`) and BL:161/958 (`llm-proxy-engine`) already use post-restructure names, while BL:86, 110, 302 still say `llm-proxy-provider` (no longer a crate) and BL:33 says "provider crate only for error classification". Statement and data contradict, and the stale names point at nothing.
- **Wanted:** one rule, stated once, true for all entries: `crate` names current workspace crates. Plan 11 To do 6 says "crate mappings corrected".
- **How:**
  1. Rewrite the `crate` values: BL:33 replace the parenthetical "provider crate only for error classification" with "llm-proxy-adapters only for error classification"; BL:86 -> `"llm-proxy-protocol + llm-proxy-adapters (adapters; audit G)"`; BL:110 -> `"llm-proxy-server + llm-proxy-transport (context_length capture in discovery; audit G)"` (discovery moved to `crates/llm-proxy-transport/src/discovery.rs`); BL:302 (#12, dropped) -> `"llm-proxy-adapters"`.
  2. Replace the last sentence of `corrections_note`: "Plan 03 split `llm-proxy-provider` into `llm-proxy-adapters` and `llm-proxy-transport` and added `llm-proxy-engine`; `crate` names the current crates, and `plan` is the authoritative landing spot."
  3. Leave rationale/evidence text that cites audited-commit paths (e.g. BL:7 `crates/llm-proxy-provider/…`); the research banner already scopes those to commit `79cf406`.
- **Test:** none.
- **Done when:** `grep '"crate"' docs/research/compensation-backlog.json | grep -c 'llm-proxy-provider'` is 0; `corrections_note` contains no "pre-restructure"; JSON parses.
- **Risk / notes:** do 11-1 in the same pass (same lines region). Do not renumber or reorder entries; the backlog numbers are positions that `docs/README.md` and the plans cite.

### 11-4 [DOC] [P1] Correct the loopback rate-limit exemption comment in `config.toml.example`
- **Where:** `config.toml.example:19-22` (the plan's cited line 14 is stale). Truth: `crates/llm-proxy-server/src/routes/core_pipeline.rs:63-72` `prepare_request`: `direct_loopback = !trust_forwarded_headers && !caller.is_authenticated() && connect_info.is_some_and(|a| a.ip().is_loopback())`; the limiter is skipped only when `direct_loopback`.
- **Now:** comment says "Direct loopback connections are exempt unless trust_forwarded_headers = true." An authenticated loopback caller (a client key presented) is rate-limited by key id, so the sentence overstates the exemption.
- **Wanted:** plan 11 To do 7: "the rate-limit comment in `config.toml.example`" must be accurate. Code is the truth.
- **How:** replace line 21 with `# Unauthenticated direct loopback connections are exempt unless trust_forwarded_headers = true; callers with a client key are always limited.` Keep lines 19-20 (keyed by key id when authenticated, client IP otherwise; `0` disables) unchanged. Keep it to this one comment; no narrative.
- **Test:** none in this item. The behavioural test for the exemption is R-9 in the remodel spec (`also: remodel`).
- **Done when:** `grep -n 'loopback' config.toml.example` shows the word "Unauthenticated" and "client key". The file still parses as TOML (comments only changed).
- **Risk / notes:** the same sentence is repeated in `docs/completed/remodel-proxy.md:129-130`; fixed by remodel item R-12 (`also: plan remodel`).

### 11-5 [DOC] [P2] (O1) Describe audit BUG-2 accurately where GA/CO say only "unrepresentable"
- **Where:** GA:306-307 (the paragraph beginning "new: llm-proxy's protocol carries a cache marker …") and CO:557-559 (same claim in the compendium). Audit source: `docs/research/audit-2026-11-05/evidence/llm-proxy-claims.md` BUG-2: "`ttl` on cache_control silently deletes the block from the prompt, HTTP 200". Fix status: `docs/completed/01-bug-fixes.md:17` (BUG-2, `ttl` carried on markers).
- **Now:** both docs say `CacheControl` "has only a `type` field … so Anthropic's 1-hour lifetime is unrepresentable". At the audited commit the real behaviour was worse: a `ttl: "1h"` marker made the whole system or text block vanish from the upstream request with HTTP 200.
- **Wanted:** the research docs describe the audited baseline truthfully (plan 11 To do 1, "claims about llm-proxy").
- **How:** in each of the two places add one sentence after "unrepresentable": "At the audited commit a `ttl` on a marker also dropped the whole block from the upstream request while returning HTTP 200 (audit BUG-2; fixed by plan 01)." Do not alter the surrounding baseline text.
- **Test:** none.
- **Done when:** `grep -n 'BUG-2' docs/research/competitive-gap-analysis.md docs/research/competitive-landscape-compendium.md` shows one hit in each file.
- **Risk / notes:** also keep the existing candidate-list wording at CO:486 unchanged.

### 11-6 [DOC] [P2] (O2) Mark the `server.hot_reload` rejection as superseded inline
- **Where:** GA:121-124 ("Config changes require a restart — `server.hot_reload=true` is rejected at load"), GA:345 (already says the rejection "was lifted by plan 07"; leave), GA:399 (gap 16 "llm-proxy rejects `server.hot_reload`"), CO:624-625 (verdict: "llm-proxy rejects `server.hot_reload` at …provider_config.rs:1531-1538"), CO:416 / GA:416 and CO:870 ("hot reload … deferred" list of gaps without a candidate). Truth: plan 07 implemented SIGHUP/`llm-proxy reload` of provider/pool files (`config.toml.example:14-18`); `server.hot_reload = true` is accepted (`load_app_config_accepts_enabled_hot_reload`, `crates/llm-proxy-core/src/provider_config.rs`).
- **Now:** only GA:345 is annotated; four other sentences still say the setting is rejected and hot reload is deferred. The file-level banner (GA:3-8, CO banner) covers it generically but unlike the other stale items there is no inline marker.
- **Wanted:** a reader of any single sentence is not misled; same inline-marker pattern as other corrections.
- **How:** append to each of GA:121-124, GA:399, CO:624-625: "(superseded: plan 07 hot-reloads provider and pool files on SIGHUP; `server.hot_reload = true` is accepted)". For the "deferred" lists at GA:416/CO:416/CO:870 append "(hot reload since done: plan 07)" after the word "hot reload". Keep the original cites to `provider_config.rs:1531-1538` (they are baseline-scoped).
- **Test:** none.
- **Done when:** `grep -n 'hot_reload' docs/research/competitive-gap-analysis.md docs/research/competitive-landscape-compendium.md` shows every rejection sentence followed by "superseded" or "plan 07".
- **Risk / notes:** do not edit the gap's product count or ranking.

### 11-7 [DOC] [P2] (O3) Add a baseline scoping line to `repo-intelligence.md`
- **Where:** `docs/research/repo-intelligence.md:1-20` (intro); line-number citations to llm-proxy code at RI:32-33 (`core.rs:639-641`/`:683-685` area) and RI:2680-2681. These are valid only at commit `79cf406` (e.g. `Usage` is now at `crates/llm-proxy-protocol/src/core.rs:718-728`).
- **Now:** GA and CO carry a dated banner scoping llm-proxy statements to `79cf406`; RI does not, although it contains the same kind of claims.
- **Wanted:** same scoping as GA/CO (plan 11 Done: "GA and CO carry a dated banner"). RI is in scope of To do 4 and houses llm-proxy line cites.
- **How:** insert one paragraph after RI's first paragraph, modelled on GA:3-8 and CO's banner: "Statements and `path:line` citations about llm-proxy's own code describe commit `79cf406` (2026-11-05). Plans 01–10 have since rebuilt most of it; current behaviour is in docs/goal.md and docs/completed/." Nothing else.
- **Test:** none.
- **Done when:** `grep -n '79cf406' docs/research/repo-intelligence.md` has a hit in the first 30 lines.
- **Risk / notes:** RR:8-14 already states the same scope for the whole directory; keep this to one sentence-pair to avoid a narrative.

### 11-8 [DOC] [P2] (O4) Cross-reference the Upstash row in the CO coverage appendix
- **Where:** `docs/research/competitive-landscape-compendium.md:991` (row in "### Wave 2 — scouts", coverage appendix), vs the note at CO:112-114 that moved Upstash out of the product rows.
- **Now:** the appendix row "Upstash AI Gateway | Negative finding — verified not to exist …" sits beside real products. It is a coverage-outcome table, not a product table, so it is not wrong; but a skim reads it as a product.
- **Wanted:** housekeeping intent (plan 11 To do 7): Upstash is not presented as a product.
- **How:** change the first cell to "Upstash AI Gateway (not a product)" and append "; see the note under the product tables" to the outcome cell. If the coding agent prefers, leave the row alone (acceptable: the outcome text already says "Negative finding"). Do not delete the row; it is the only record of the 11 live checks in the appendix.
- **Test:** none.
- **Done when:** CO:991 contains "not a product" or the row is unchanged by explicit decision.
- **Risk / notes:** lowest priority item in this plan.

### 11-9 [DECIDE] [P2] Self-reported "verified against live documentation/GitHub" claims
- **Where:** `docs/completed/11-research-corrections.md` "Done" bullets: "each verified against the live GitHub item" (7 citations) and "each verified against the provider's current documentation" (PV:14-27, PV:77-83, PV:121, EP:124-129, CP:120-124, RI:379-384, RI:1067-1072, RI:2693-2695, RI:629-631, NR:357-362).
- **Now:** static review cannot confirm these (no network).
- **Wanted:** the claims are either re-verified or not asserted.
- **How (recommended default: leave the docs as is, no edit):** the evidence for each correction (doc line and date marker) is in place and the Done text is a historical plan record. A human with network may spot-check the five higher-risk ones: RI PR 5978 content, RI prompt-cache issue 16 reply timing, PV OpenCode `/zen/v1/models` live list, CP forced `tool_choice` 400, PV Anthropic 4-breakpoint/`ttl`. Alternative if any fails: restate the sentence with "as of 2026-11-08".
- **Test:** none.
- **Done when:** decision recorded in the commit message; no doc change required.
- **Risk / notes:** do not spend coding-agent time here without network.

## Rule violations to clean up

All are in [rule-violations.md](rule-violations.md), each listed once with one instruction. This area: none (docs only).
New code written for this plan must follow the same rules: no `unwrap`/`expect`/`panic!`, no indexing or slicing, and no `let _ =` on a `Result` or `Future` in non-test code.

## Rejected findings

- "Row 74/75: RM `llm-proxy-api` description wrong" — `README.md:18` says "read-only /usage API" and `crates/llm-proxy-api/src/routes.rs:66-70` are GET-only; the plan's Done already says "already correct". No change.
- "Row 70: Upstash row still in CO" — the products table is clean (CO:112-114 note). Only the appendix row remains; handled as optional 11-8.
- "Row 16 sweep: absolute claims remain (GA:212, CO:621, RI:332, NR:263, NR:646, NR:880)" — report verified these are qualified.
- "Rows 81, 82 NOT-STATICALLY-VERIFIABLE" — need network; see 11-9. Not a defect.
- "Numbers in RI:2635-2663 (261 / 114 / 238 / 5; 26 of 40; roy-tong 94) not re-derived" — evidence files are outside this plan's scope; the figures were copied from the audit. No action.
- "Rule violations in opened code" — none in non-test code; test-only unwrap/expect are allowed by AGENTS.md.
