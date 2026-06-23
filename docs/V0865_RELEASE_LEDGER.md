# v0.8.65 Release Ledger

Updated: 2026-06-23 (Phase 2 triage + reconciliation), America/Los_Angeles.

This ledger tracks completion by concrete outcome: merged, replaced by a clean
PR, absorbed with evidence, closed after implementation evidence, or blocked
only by Hunter approval. It is reconciled against live `main` and the GitHub
`v0.8.65` milestone, not against titles/labels.

## Release-readiness verdict (read first)

**v0.8.65 is NOT release-ready.** A 25-issue reconciliation against `main`
(2026-06-23) found **0 issues closeable**: 18 are partially landed (a
foundation/slice merged, scope remains) and 7 are not started. The milestone
still contains large unbuilt architecture — the Fleet execution substrate,
provider descriptors/pricing/catalog/telemetry/context-budget engines, the
config-module split, and the README end-cap. Recent foundation PRs have landed
(route foundation #3458, Fleet profile types #3469, provider docs #3471,
readiness dashboard #3485, reasoning streams #3446, Qianfan #3425,
deepseek-anthropic #3449, model-picker search #3484), but the bulk of scope is
outstanding.

**Do not bump the version, tag, publish artifacts, create a GitHub Release, or
deploy the website.** Those are Hunter-gated *and* unwarranted until the
milestone is substantially complete and full release verification
(`cargo test --workspace`, web `lint`/`build`/`check:facts`, release build) is
green.

## Phase 1 — landed (prior session)

| Item | Status |
| --- | --- |
| #3468 WeCom `activeTurnId` fix | Merged |
| #3472 retired sub-agent refs | Merged |
| #3476 digest archive route | Merged |
| #3481 fact-drift CI gate (replaced #3473, closes #3415) | Merged |
| #3479 YOLO git tag probe fix | Merged |
| #3482 install script (closes #3477) | Merged |
| #3483 release ledger | Merged |

## Phase 2 — this session (2026-06-23)

### Merged
| PR | Outcome |
| --- | --- |
| #3485 provider readiness dashboard | Merged after green CI. Delivers the `ProviderDashboardRow` slice of issue **#3083** (issue stays open — capability badges, action menu, `/model` filtering remain). |
| #3491 prompt-mode boundary | Merged after green CI. Credit-preserving rebase of community PR **#3455** (@mvanhorn) onto live `main`; **closed #3387**. Mode/route changes now never come from prompt text. |

### Closed with evidence
| PR | Outcome |
| --- | --- |
| #3433 storage hardening | Closed — superseded by merged **#3450** (`8d74738257`), same 6 files. Sole delta (async `write_task_artifact_for`) invited as a focused follow-up. |
| #3455 prompt-mode boundary | Closed in favor of #3491 (rebased, credited). |
| #3492 route_resolver harvest | **Closed by author (me).** Harvested a *parallel* `crates/tui/src/route_resolver.rs` from the pre-#3458 milestone WIP. The canonical route foundation is `crates/config/src/route/resolver.rs` (PR #3458) — documented as *"the sole producer of ReadyRouteCandidate (#3384)"* and already consumed by the dashboard. Shipping a second resolver would be divergent machinery (forbidden by AGENTS.md). #3384's wiring should build on #3458, not this. |

### Reviewed — needs rework (credit preserved, left open)
| PR | Outcome |
| --- | --- |
| #3437 approval-modal prominence | Good idea; cannot land. Carries a **SECURITY.md `.net`→`.com` regression**, a stray `codewhale` submodule gitlink, and scratch pollution; the widget hunk references nonexistent `palette::SELECTION_BG` (selection styling is theme-aware). Reviewed with the exact path to a clean single-file PR. Aesthetic restyle of the security-critical approval surface → needs Hunter design sign-off. |
| #3440 site/mirror provenance | **v0.8.69-scoped**, not v0.8.65. Same SECURITY.md regression + pollution as #3437. Redirected to a clean v0.8.69 branch with only the 3 provenance files (`docs/CNB_MIRROR.md`, `web/components/footer.tsx`, `web/app/[locale]/install/page.tsx`). |

### Holds for Hunter (will not merge without your call)
| PR | Why |
| --- | --- |
| #3470 Orchestration disposition + Fleet RFC | Author-gated: *"maintainer's to bless — this is a constitution philosophy change."* `constitution.md` is the sole base prompt; the disposition ships to every agent turn. CLEAN/green; merge-ready the instant you bless it. (Optional: soften the unqualified "Always have work in flight" at constitution.md:390.) |
| #3452 refresh agent guidance | Good "Start With Live Truth" framing, but the rewrite **drops hard guardrails** present on `main`'s AGENTS.md: the `agent`-only sub-agent surface, "constitution.md is the sole base prompt", the "no speculative `spawn_blocking` freeze fix" note, and the known-flaky-suite papercuts. Re-add those before merge (or prefer the milestone WIP's additive refresh). Currently a draft. |
| #3432 bridge-core consolidation | DRAFT, CLEAN, but `integrations/` is **not exercised by any CI workflow**, so green CI does not test the refactor. Needs manual bridge tests (`npm --prefix integrations/bridge-core test` + per-bridge smoke) and ready-for-review before merge. |

### Out of v0.8.65 scope (parked correctly by their owners)
| PR | Scope |
| --- | --- |
| #3381 memory tags | v0.8.69/v0.9.0. Competing design with #2933 — needs a Hunter direction decision (flat-markdown tag index vs. typed v2 store). |
| #2933 hippocampal memory v2 | v0.9.0; maintainer deferred pending a design conversation. Three accepted unrelated fixes should be split out. |
| #2486 WhaleFlow cost tracking | v0.9.0 source branch; safe slices already harvested via merged #2821/#2827 (credited @AdityaVG13). |
| #2239 i18n Phase 1-4b | v0.8.71; branch ~1630 commits behind, infra superseded on `main`. Redirect to a fresh narrow branch (target #790). |

## Milestone issue reconciliation (25 open, 2026-06-23)

**0 closeable.** Disposition counts: 18 partial-keep-open, 7 not-started-keep-active.

| Issue | Disposition | Status (capability on `main`) | Evidence / next |
| --- | --- | --- | --- |
| #2300 (EPIC) multi-model compat + auto loadout | partial | docs slice only | PR #3471 docs; behavioral acceptance via #2608 |
| #2574 (EPIC) capability-aware fallback chain | partial | happy-path fallback present, not capability-aware | #2779 + harvest 662a459ee; rebase onto route foundation |
| #2608 (EPIC) separate facts/offerings/route resolution | partial | foundation merged | PR #3458 route/ module; #3485 consumer |
| #2961 (EPIC) normalize provider usage telemetry | partial | Anthropic mapping only | PR #3014; define `UsageTelemetry` schema |
| #2963 DeepSeek Anthropic-compatible spike | partial | plumbing merged | PR #3449 (`5b8a5ac0b`); run smoke + write comparison |
| #2984 Codex/ChatGPT OAuth route + usage | partial | infra present | #3485 readiness + 940ea2875; live-account verification pending |
| #3075 cross-provider `/model` search | partial | search slice live | PR #3484 (`19d217b3e`); route selections via resolver remains |
| #3083 `/provider` readiness dashboard | partial | row-model + dashboard live | PR #3485; capability badges/action menu/filtering remain |
| #3084 (EPIC) provider descriptors + conformance | partial | descriptor/type/resolver layer | PR #3458; ~1 of 6 scope items |
| #3085 (EPIC) PricingSku usage engine + provenance | partial | enum stub + display label | PR #3458/#3485; engine unstarted |
| #3086 (EPIC) resolved-route context budget service | partial | budget-math foundation | `context_budget.rs` (5634fa6f9); provenance contract remains |
| #3154 (EPIC) Fleet execution substrate | partial | ~7.7k-line subsystem + types | PR #3469 types; executor/model-class wiring remains |
| #3166 (EPIC) Fleet route parity smoke/soak/handoff | partial | thin smoke slice | blocked on #3167/#3205/#3384/#3154 |
| #3167 (EPIC) Fleet profiles roles/loadouts/delegation | partial | types + config plumbing | PR #3469 slice 1 |
| #3222 reasoning stream style overrides | partial | core fix landed | PR #3446 (`2a4c67afa`); route-explanation integration remains |
| #3357 Baidu Qianfan route fixture | partial | fixture merged | PR #3425 (`0861661b6`); deferred deps remain |
| #3384 resolve switches through ReadyRouteCandidate | partial | foundation only | PR #3458 (canonical resolver); **wiring is the remaining slice — build on #3458, not a parallel module** |
| #3439 GLM-5.2 provider route fixture | partial | works via Z.ai/OpenRouter, not bigmodel.cn | decide Zhipu first-class vs. redirect; reply to reporter |
| #1519 custom provider endpoints/models/auth | not started | foundation only | unblocked by #3458; consumer slice unstarted |
| #3087 README end-cap rewrite | not started | gated on architecture stabilizing | do last |
| #3205 (EPIC) Fleet model classes + route roles | not started | absent | unwired seams in #3458/#3469; needs build |
| #3311 split config modules around boundaries | not started | absent | now actionable post-#3458/#3384; schedule migration |
| #3367 user-defined personas as Fleet inputs | not started | absent | blocked until #3167/#3205 wired |
| #3385 provider-owned live catalogs + secret-free cache | not started | absent (offline metadata layer only) | build on #3383/#3428; wire live `/models` refresh |
| #3478 (EPIC) TUI transcript smoothing + UX polish | not started | absent | split into 2-3 testable slices; land a calm-transcript preset first |

## Worktree layout

| Worktree | Branch | Purpose |
| --- | --- | --- |
| `CodeWhale` | `milestone/v0.8.65-provider-model-routing` | **Dirty** WIP (route_catalog.rs/route_resolver.rs + config refactor). Untouched. Do not reset. Note: route_resolver predates #3458; route_catalog table-consolidation (#3311) needs a behavior-equivalence pass before any harvest. |
| `CodeWhale-trackd-routes` | `codex/v0.8.65-ledger-truth` | This ledger update; otherwise a clean `main` checkout for verification. |

Stale (merged) worktrees from prior sessions left intact: `CodeWhale-install-script`,
`CodeWhale-pr3473-fix`, `CodeWhale-v0865-release-ledger`, `CodeWhale-yolo-approval`,
`CodeWhale-v0865-ledger-update` (orphan ledger branch — superseded by this update).

## Blockers / decisions for Hunter

1. **Milestone is large and incomplete** — decide whether to (a) keep pushing
   v0.8.65 through more implementation cycles, or (b) re-milestone the unstarted
   EPICs (#3205, #3311, #3367, #3385, #3478) to a later version and ship a
   smaller v0.8.65.
2. **#3384 route wiring** — build `switch_provider`/`/provider` on the canonical
   `codewhale_config::route::RouteResolver` (#3458); do not harvest the parallel
   pre-#3458 WIP resolver.
3. **#3470 / #3452 / #3432** — philosophy bless / guardrail re-add / draft-ready +
   manual bridge verification (see Holds table).
4. **#3381 vs #2933** — pick the memory architecture direction.
5. **Version bump / tag / artifacts / GitHub Release / website deploy** — Hunter
   approval only, and not yet warranted.
