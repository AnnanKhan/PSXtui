# PSX Pre-Breakout Research

Run on 23 August 2026 against the local PSXtui cache through 21 August 2026.

## Protocol

- Daily bars from 26 July 2021 through 21 August 2026.
- 129 currently listed equities after requiring 900 bars, median turnover above
  PKR 3M, at least 90% true-high/low coverage in the tradable period, and no
  unadjusted single-session price jump above 40%.
- The strategy itself requires rolling 20-day median turnover above PKR 20M.
- Signals fill at the next session's open.
- Initial probe: 33% of the symbol sleeve. Confirmed breakout add: 67%.
- Commission: 10 bp per side. Slippage: 5 bp per side.
- Three anchored walk-forward folds. One pooled parameter set is chosen across
  the universe in each training fold; no per-symbol fitting.
- Each symbol receives an equal static capital sleeve. Idle sleeve capital
  remains in cash.

## Full-sample defaults

This is descriptive, not validation.

| Metric | Result |
|---|---:|
| Equal-weight mean return | +0.18% |
| Median symbol return | 0.00% |
| Trades | 12 |
| Confirmed breakouts | 50.0% |
| Win rate | 41.7% |
| Average trade return | +1.91% |
| Median trade return | -0.75% |

## Walk-forward results

| Fold | Test period | Trades | Win rate | Mean sleeve return | Average trade | Median trade |
|---|---|---:|---:|---:|---:|---:|
| 1 | 2024-01-19 to 2024-12-03 | 14 | 50.0% | +0.30% | +2.88% | +0.66% |
| 2 | 2024-12-04 to 2025-10-13 | 6 | 16.7% | +0.14% | +0.33% | -2.78% |
| 3 | 2025-10-14 to 2026-08-21 | 10 | 60.0% | +0.19% | +2.17% | +1.05% |

| Aggregate | Result |
|---|---:|
| Compounded equal-weight OOS return | +0.63% |
| KSE100 over the OOS span | +179.96% |
| OOS trades | 30 |
| OOS win rate | 46.7% |
| All-trade average return | +2.13% |
| All-trade median return | -0.15% |
| Confirmed-breakout trades | 12 |
| Confirmed average return | +8.17% |
| Confirmed median return | +4.21% |

Best fold-symbol outcomes were HBL (+29.9%), UNITY (+19.2%), OGDC (+17.2%),
PPL (+10.4%), and FFC (+9.9%). The worst were NPL (-6.0%), FATIMA (-3.6%),
EPCL (-2.3%), BAHL (-1.7%), and KAPCO (-1.7%).

## Verdict

The anticipatory probe did not demonstrate a dependable standalone edge: the
median out-of-sample trade was -0.15%, and only 30 trades occurred. Confirmed
breakouts were materially stronger, with a +4.21% median, but that subgroup has
only 12 observations and is not enough to establish statistical reliability.

The strategy is useful as a selective alert and staged execution rule, not as
a complete capital-allocation system. Its low exposure left it far behind the
KSE100 during a very strong benchmark period. Survivorship bias also remains
because the PSX symbol master contains current listings rather than a
point-in-time historical universe.

## Reproduce

```sh
cargo run --release --example pre_breakout_research
cargo run --release --example pre_breakout_research -- --diagnose
```
