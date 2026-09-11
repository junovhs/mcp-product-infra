# CI minutes — read before touching .github/workflows

**Date:** 2026-09-11

## What happened

The junovhs GitHub account's free Actions allotment for September was fully consumed
(private repos get 2,000 min/month; Windows runners bill at 2x, macOS at 10x). GitHub now
refuses new hosted jobs on every private repo with:

> The job was not started because recent account payments have failed or your spending
> limit needs to be increased.

Nothing was charged. The spending limit is $0 and stays $0.

## Usage by repo, September 1–11 2026 (of ~$16 included)

| Repo | Gross | Source |
|---|---|---|
| cspkiller | $11.35 | `CI` on every push (30 pushes Sep 6–11) with an OS matrix; the macOS/Windows legs are the cost |
| ishoo | $0.60 | Build Windows binary / Build Linux package / Pages deploy |
| mcp-product-infra | $0.29 | public repo; standard Linux runners are free |
| pixpal | $0.09 | Pages deploys |
| shopify | $0.05 | daily scheduled "Mark stale issues" job |
| all others | $0.10 | |

**This repo:** $0.29; public, so mostly unaffected.

## Decision

No paid GitHub-hosted minutes, ever. CI moves to a **self-hosted runner** on the owner's
home machine (dual-boot Linux / Windows, idle during the day), registered under junovhs.

## Rules until the self-hosted runner exists

- A red ✗ on a push to a private repo is the refusal above, not a test failure. Expect it
  until Oct 1 (allotment reset) or until the runner is registered.
- Do **not** raise the spending limit or add a payment method.
- When editing a workflow: prefer `workflow_dispatch` over `push`; drop macOS/Windows
  matrix legs that are not strictly needed; target `runs-on: self-hosted` (Linux) or
  `runs-on: [self-hosted, windows]` once the runner is registered.

## Plan

Register one Linux and one Windows runner service on the home box
(repo → Settings → Actions → Runners → New self-hosted runner), then switch each
workflow's `runs-on`. Track that work in this repo's Ishoo store.

