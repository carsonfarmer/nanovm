# Batch 5 — synthetic planning exercises

`planning.py` completes NH30/NH32/NH39/NH40/NH43/NH48 as offline calculators and
generators. Every input must say `"basis":"synthetic"`; every output repeats it.
No host, filesystem allocation, live price, production workload or power sampling
is performed. Python's standard library is sufficient.

```sh
python3 -B experiments/nice-to-haves/planning.py experiments/nice-to-haves/fixtures/planning.json
```

Each input is a JSON object with `basis`, `kind`, and the fields below. Invalid
input exits 2. Successful calculation exits 0 even when a budget is exceeded;
read `over_budget`. Numeric assumptions are supplied explicitly, without defaults.

| kind | Required fields | Meaning / limits |
| --- | --- | --- |
| `amplification` | `block_bytes`, `metadata_bytes_per_file`, `file_bytes` array | Round each nonempty file up to a block, then add per-file metadata including empty files. No shared blocks, sparse holes, compression, journals, filesystem metadata rules or backup implementation. Zero logical bytes → null ratio. |
| `cardinality` | `metric_families`, `series_per_labelset`, `label_value_counts` array, `series_budget` | Cartesian product upper bound, not observed combinations. Supply classic histogram bucket/count/sum expansion in series_per_labelset yourself. Empty labels give one labelset; a zero cardinality gives zero combinations. Exact Python integers; report signed-64-bit overflow explicitly. |
| `cost` | `hours`, `nodes`, `watts_per_node`, `cost_per_kwh`, `cloud_cost_per_node_hour` | kWh = nodes × watts × hours / 1000. Electricity and cloud costs are **separate alternatives**, never added. Same caller-chosen currency; no tax, network, cooling, reserved discounts, carbon or current-price claims. |
| `queue` | `jobs` array of `arrival_ms`, `service_ms` | One initially idle FIFO nonpreemptive server, no drops. Ties keep input order. start = max(arrival, previous finish). Integer milliseconds. No host admission, worker concurrency or queue-cap simulation. |
| `order` | `pairs` (positive even), `seed` | Exactly half AB and half BA, shuffled. A/B names are local variant aliases. Warm and reset both consistently; randomization cannot remove all drift/carryover. Not a significance test. |
| `popularity` | `objects`, `requests`, `seed`, `distribution` | Emits zero-based object sequence plus exact counts; uniform, Zipf or hotset sampling **with replacement**. Zipf additionally requires `exponent` 0–10. Hotset requires `hot_objects` strictly between 0 and objects, and `hot_share` 0–1. Probabilities, not exact quotas. No network traffic. |

Arrays/jobs/objects/requests/pairs are capped at 10,000; cardinality dimensions at
32. Individual numeric assumptions are capped at 1e12. Python arbitrarily large
cardinality totals are intentional; JavaScript consumers should parse such numbers
as lossless integers. Costs are floating-point estimates, not billing arithmetic.
Seeds reproduce on the tested Python 3.11 toolchain; archive the generated sequence
and Python version rather than promising identical random-library behavior forever.

Seven public-contract tests pass. Independent exact cases establish:

- `[0,1,4096,4097]` bytes at 4096-byte blocks and 128-byte metadata: 8,194 logical,
  16,384 data allocation +512 metadata =16,896 synthetic physical bytes. Rounding
  the sum instead of each file would be wrong.
- 3 metric families ×7 series ×2×5 labels =210: budget 210 passes, 209 exceeds.
  A 21×10²⁴ case retains exact integers and flags signed-64-bit overflow.
- 3 nodes ×250 W ×4 h =3 kWh; electricity at 0.20 is 0.60, cloud at 0.50/node-hour
  is 6.00, not a combined 6.60 claim.
- Arrivals/services `(2,4),(3,2),(3,0),(12,1)` yield waits `[0,3,5,0]`, finishes
  `[6,8,8,13]`, mean wait 2 ms. An idle gap resets accumulated waiting.
- Seeded orders have exactly 10 AB/10 BA pairs; replay and distinct-seed controls
  pass. Popularity counts reconcile exactly with each emitted sequence, hot shares
  0 and 1 stay in the appropriate partition, and the Zipf fixture is skewed.

Remaining independent work includes offline artifact review, language toolchain
probes and more local fixtures. None of these estimates is a performance result.
