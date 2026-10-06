Fixes [ENG-412](https://linear.app/acme/issue/ENG-412) — Invoice total ignores discount

> Opened by **Mage** at the request of Dana Support.
> Human review required. Mage does not merge.

## Root cause
`computeTotal` filters line items by kind, dropping the discount row.

Evidence (at `aaaaaaa`):
- [`src/billing/total.ts:40-52`](https://github.com/acme/shop/blob/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/src/billing/total.ts#L40-L52)
- [`src/billing/discount.ts:10-14`](https://github.com/acme/shop/blob/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/src/billing/discount.ts#L10-L14)

## Change
> Include adjustment rows in the total.
>
> Also added a unit test. cc @​lead

2 files changed, +14 −3. Discarded by policy: `package-lock.json`, `dist/app.js`.
Rebased from `aaaaaaa` onto `bbbbbbb` and re-verified.

## Regression test
`.mage/regressions/eng-412-discount-row-excluded.spec.ts` — the script that reproduced the bug; it failed before this change and passes
after. Run: `npx playwright test .mage/regressions/eng-412-discount-row-excluded.spec.ts` (see `.mage/env.toml` for setup).

## Verification (fix gate, clean environment)
| Check | Result |
|---|---|
| Patch applies at `bbbbbbb` | pass |
| Repro script (1 fresh fork) | pass — signature `Expected: "$120.00" Received: "$100.00"` absent |
| `npm test` | exit 0 |
| `npm run lint` | pass |

## Before / after

| Before (repro) | After (fix) |
|---|---|
| — | ![after](https://github.com/user-attachments/assets/00000002-aaaa-4bbb-8ccc-dddddddddddd) |

[trace](https://mage.example.com/artifacts/art_01j0000000e00800000000000a) · [test log](https://mage.example.com/artifacts/art_01j0000000e00800000000000b) · [Mage run](https://mage.example.com/runs/run_01j0000000e00800000000000m)

<!-- mage:change-set=3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f run=run_01j0000000e00800000000000m gate=gat_01j0000000e00800000000000n -->
