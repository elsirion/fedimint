# LN Latency Bench

Rust tool for benchmarking Lightning Network payment latency in Fedimint federations. Tests internal (same-federation) and cross-federation payments, comparing LNv1 vs LNv2 protocols and iroh vs HTTPS transports.

## Usage

```bash
ln-latency-bench \
  --fed-a-dir /path/to/fed-a \
  --fed-b-dir /path/to/fed-b \
  --iterations 10 \
  --amount-msat 10000 \
  --timeout-secs 120 \
  --gateway-ids <comma-separated-pubkeys> \
  --lnv2-gateway "https://gateway.example.com/v1"
```

### Options

| Flag | Default | Description |
|---|---|---|
| `--fed-a-dir` | required | Data directory for federation A (existing `fmcli` client dir) |
| `--fed-b-dir` | required | Data directory for federation B |
| `--iterations` | 100 | Number of iterations per scenario |
| `--amount-msat` | 100000 | Payment amount in millisatoshis |
| `--timeout-secs` | 180 | Per-payment timeout |
| `--gateway-ids` | (all) | Comma-separated gateway pubkeys to filter by |
| `--lnv2-gateway` | (none) | Gateway URL for LNv2 scenarios (bypasses server-side registration) |

### Prerequisites

Both federation data directories must already exist (joined via `fmcli join-federation`) and have sufficient balance for the test payments.

## Test Matrix

The tool automatically builds scenarios from available gateways:

**Internal payments** (same federation, settles via consensus without LN routing):
- `internal_A_v1`, `internal_B_v1` — LNv1 on each federation
- `internal_A_v2` — LNv2 (if federation supports it and `--lnv2-gateway` is set)
- `internal_A_v2send_v1recv`, `internal_A_v1send_v2recv` — mixed protocol

**Cross-federation payments** (routes through Lightning Network):
- `cross_A_to_B_v1`, `cross_B_to_A_v1` — LNv1 both directions
- `cross_A_to_B_v2send`, `cross_B_to_A_v2recv` — v2 on the side that supports it

Scenarios are **interleaved** (not batched) to control for time-of-day and network effects.

## Output

Per-scenario statistics for each phase (`invoice_create`, `ln_pay`, `await_recv`) and total:
- Median, p90, p95
- IQR (interquartile range) — robust spread measure
- σ (standard deviation)
- Success/failure count

## Findings

### Latency Budget (typical cross-fed payment ~8s)

| Phase | Time | % | What happens |
|---|---|---|---|
| `invoice_create` | ~0.5s | ~7% | Submit offer to federation consensus |
| `ln_pay` — consensus | ~2s | ~25% | Submit contract, wait for acceptance |
| `ln_pay` — LN routing | ~5-6s | **~65%** | Gateway routes HTLC over Lightning |
| `await_recv` | ~0-1.5s | ~8% | Receiver claims e-cash |

### LNv1 vs LNv2 (cross-federation, fair comparison)

| Direction | v1 Median | v2 Median | Delta |
|---|---|---|---|
| A→B | 7.8s | **6.7s** (v2 send) | v2 ~1s faster |
| B→A | 7.4s | **7.3s** (v2 recv) | ~same |

v2 send is faster because its contract structure allows the gateway to settle more efficiently. For internal payments, v1→v1 wins (3.6s) because the federation detects same-federation payments and skips LN routing entirely. Mixed v1/v2 breaks this detection.

### Iroh vs HTTPS

Medians are comparable once iroh connections are warm. Iroh has worse tail latency (p95 2-3x higher) and ~5% failure rate due to connection resets that cascade across subsequent operations.

### Network Location

Client-to-guardian latency scales all phases. Internal payments ranged from 3.1s (best network) to 8.3s (worst). Cross-fed payments are more stable since LN routing dominates.
