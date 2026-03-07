# Integration Tests

Integration tests run the `pattern-classifier` component inside a **real wasmCloud
mesh** and verify the complete Phase 1 classification pipeline end-to-end:

```
NATS messages  →  component (VSA encode + classify)
                       ├─► Redis: stats:v1:*  (Welford accumulators)
                       ├─► Redis: ema:v1:*    (per-field running bundles)
                       └─► NATS:  pattern.classified.*  (result JSON)
```

## What the test does

| Step | Description |
|------|-------------|
| 1 | Verifies `wash`, `nats`, and `redis-cli` are available |
| 2 | Ensures Redis is running — auto-starts `redis-server` if absent |
| 3 | Builds the component with `wash build` if no pre-built `.wasm` exists |
| 4 | Starts a local wasmCloud host (`wash up`) |
| 5 | Deploys the WADM application manifest (`wadm.yaml`) |
| 6 | Starts a background NATS subscriber on `pattern.classified.>` |
| 7 | Sends `WARM_UP_COUNT` (31) identical messages — expects `WARMING_UP` responses |
| 8 | Sends one "normal" message (similar values) and one "anomalous" message (extreme values) |
| 9 | Exercises the HITL reply flow: publishes a human reply on the `.reply` topic |
| 10 | Verifies all expected Redis keys exist (`stats:v1:*` and `ema:v1:*`) |
| 11 | Verifies NATS output: at least one `NORMAL` or `ANOMALY` classification captured with all required JSON fields |
| 12 | Cleans up: flushes test keys, undeploys app, stops host, stops Redis (if started) |

## Prerequisites

| Tool | Purpose | Install |
|------|---------|---------|
| `wash` | wasmCloud CLI — build, start host, deploy app | `curl -s "https://packagecloud.io/install/repositories/wasmcloud/core/script.deb.sh" \| sudo bash && sudo apt-get install -y wash` |
| `nats` | Publish/subscribe to NATS subjects (`wash pub` removed in wash v0.40+) | `curl -sSL https://github.com/nats-io/natscli/releases/download/v0.1.6/nats-0.1.6-linux-amd64.zip -o /tmp/nats.zip && unzip -q /tmp/nats.zip -d /tmp/nats-dl && sudo cp /tmp/nats-dl/nats-0.1.6-linux-amd64/nats /usr/local/bin/nats` |
| `redis-server` | Redis (auto-started if not already running) | `sudo apt-get install -y redis-server` |
| `redis-cli` | Key verification | included in `redis-tools` / `redis-server` packages |

## Running

```bash
# From the repository root:
./tests/run_integration_test.sh
```

### Environment variable overrides

| Variable | Default | Description |
|----------|---------|-------------|
| `REDIS_HOST` | `127.0.0.1` | Redis host |
| `REDIS_PORT` | `6379` | Redis port |
| `HOST_READY_TIMEOUT` | `60` | Seconds to wait for wasmCloud host to start |
| `DEPLOY_TIMEOUT` | `120` | Seconds to wait for app to reach Deployed state |
| `PROCESS_WAIT` | `10` | Seconds to wait after each publish batch |
| `WARM_UP_COUNT` | `31` | Number of warm-up messages to send (must be > `MIN_SAMPLES=30`) |

Example with a remote Redis:
```bash
REDIS_HOST=10.0.0.5 REDIS_PORT=6380 ./tests/run_integration_test.sh
```

## Success criteria

### Redis keys (written by classifier)

| Key | What it holds |
|-----|--------------|
| `stats:v1:pattern.monitor.integration:n` | Message count (u64, LE bytes) |
| `stats:v1:pattern.monitor.integration:mean_vec` | Rolling mean bundle (`SparseVec`, bincode) |
| `stats:v1:pattern.monitor.integration:mean_score` | Welford mean of cosine scores (f32, LE bytes) |
| `stats:v1:pattern.monitor.integration:m2` | Welford M₂ accumulator (f32, LE bytes) |
| `ema:v1:pattern.monitor.integration:event` | Per-field EMA vector (`SparseVec`, bincode) |
| `ema:v1:pattern.monitor.integration:magnitude` | Per-field EMA vector |
| `ema:v1:pattern.monitor.integration:location` | Per-field EMA vector |
| `ema:v1:pattern.monitor.integration:depth_km` | Per-field EMA vector |

### NATS output (`pattern.classified.pattern.monitor.integration`)

Every classification message must contain these JSON fields:

| Field | Example | Description |
|-------|---------|-------------|
| `subject` | `"pattern.monitor.integration"` | Originating NATS subject |
| `classification` | `"NORMAL"` / `"ANOMALY"` / `"WARMING_UP"` | Current classification |
| `confidence` | `0.87` | Combined confidence in `[0, 1]` |
| `solutions` | `[{"label":"normal","confidence":0.9}]` | Ranked hypothesis list |
| `anomalous_fields` | `["magnitude"]` | Fields that triggered field-level anomaly |
| `message_count` | `32` | Total messages processed for this subject |
| `hitl_required` | `false` | Whether human escalation was needed |
| `phase` | `1` | Phase level that produced this result |

### HITL flow

When `hitl_required` is `true`, the classifier additionally publishes to
`pattern.alert.human.{leaf}` with `reply_to` set to `pattern.alert.human.{leaf}.reply`.
The test publishes a synthetic human reply to that `.reply` subject and verifies
the stats counter key is correctly sized (8 bytes = one u64).

## Failure diagnostics

On failure the script prints:

1. All Redis keys currently in the store (`KEYS *`)
2. The last 80 lines of the wasmCloud host log

Common failure modes:

| Symptom | Likely cause |
|---------|-------------|
| Application in `Failed` state | Redis not reachable, or provider OCI pull failed |
| Stats keys missing from Redis | Component linked to wrong Redis bucket; check `BUCKET_ID` in lib.rs |
| No NATS output captured | Subscriber started too late; try increasing `PROCESS_WAIT` |
| Only WARMING_UP in NATS output | `WARM_UP_COUNT` not high enough; must be > `MIN_SAMPLES` (30) |
| HITL alert key wrong size | Classifier did not process the `.reply` message; check subscriptions in `wadm.yaml` |

## Relationship to the encoder test

This test is self-contained — it does **not** require the
[`wasmcloud-pattern-encoder`](https://github.com/64BitAsura/wasmcloud-pattern-encoder)
to be deployed. The classifier re-encodes JSON deterministically using the same
`ReversibleVSAConfig::default()` as the encoder, so both components can share
the same Redis bucket without coordination.

The encoder's Redis keys (`semantic:v1:*`, `bundle:v1:*`) are not verified here;
they belong to the encoder's own integration test.
