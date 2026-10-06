Fixes [ENG-412](https://linear.app/acme/issue/ENG-412) — Invoice total ignores discount

> Opened by **Mage** at the request of auto (org setting).
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

2 files changed, +14 −3.

## Verification (fix gate, clean environment)
| Check | Result |
|---|---|
| Patch applies at `aaaaaaa` | pass |
| Repro script (2 fresh forks) | pass — signature `Expected: "$120.00" Received: "$100.00"` absent |
| `npm test` | exit 0 |
| Lint | n/a |

## Before / after

| Before (repro) | After (fix) |
|---|---|
| [before recording (Mage)](https://mage.example.com/artifacts/art_01j0000000e00800000000000y) | [after recording (Mage)](https://mage.example.com/artifacts/art_01j0000000e00800000000000z) |

[After (fix) video (Mage)](https://mage.example.com/artifacts/art_01j0000000e008000000000010)

[trace](https://mage.example.com/artifacts/art_01j0000000e00800000000000a) · [test log](https://mage.example.com/artifacts/art_01j0000000e00800000000000b) · [Mage run](https://mage.example.com/runs/run_01j0000000e00800000000000m)

<!-- mage:change-set=3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f run=run_01j0000000e00800000000000m gate=gat_01j0000000e00800000000000n -->
