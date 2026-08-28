---
description: SMP の同期方針（BKL と Locked の向き）。kernel を読むときに入る
paths:
  - "kernel/**/*.rs"
---

## 同期方針（SMP。BKL）

**本体は `docs/architecture.md` の「同期・並行性方針（SMP。BKL）」にある。**
決定は ADR-0023（BKL）・ADR-0004（`Locked<T>` の fail-fast）・
ADR-0027（TLB の世代）・ADR-0036（I/O 待ちは BKL を解いて眠る）。

**ここに規則を写さない。** **写していたので、実際に片方だけが古くなった**——
**ADR-0036 が I/O 待ちを設計し直した後も、両方が「S13 で設計し直す」と書いた
ままだった**（実測。2026-08-28。**片方を直したときに、もう片方に気づいた**）。
