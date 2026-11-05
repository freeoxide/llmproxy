# 11. Research corrections

- **Status:** Not started
- **Depends on:** Nothing
- **Audit rows:** None
- **Code-audit bugs:** None
- **Backlog:** None
- **Conflicts:** None

Correct the research so it can be used without reading the audit alongside it. The line
references are in the [audit report](../research/audit-2026-11-05/REPORT.md).

## To do

1. **Claims about llm-proxy.** Fix `competitive-gap-analysis.md` and
   `competitive-landscape-compendium.md` from "False or misleading claims". Line numbers
   in those docs were recorded at the audited commit, so re-check them first.
2. **Claims about other products.** Correct the "nobody ships X" claims and stale
   statuses from the same section.
3. **Paid and beta features.** Separate them from self-hostable ones, and restate the
   prevalence counts on that basis.
4. **Citations and demand.** Fix the wave 3 and 4 citations in `repo-intelligence.md` and
   `niche-repos.md`, and add the caveats from "How strong the demand evidence is".
5. **Provider notes.** Update `providers.md`, `core-protocol.md` and `endpoints.md`, add
   the missing current behaviour and date each section.
6. **Backlog.** In `compensation-backlog.json`, apply the corrections from "Backlog
   review", add the missing items and record each candidate's plan number.
7. **Housekeeping.** Work through "Doc housekeeping", including the root `README.md`
   description of `llm-proxy-api` and the rate-limit comment in `config.toml.example`.
8. **Remove the warning** in `docs/research/README.md` once the steps above are done.

## Done when

- Re-checking the audit's false claims finds each one corrected or removed.
- Every backlog candidate names a plan, or says why it's dropped.
