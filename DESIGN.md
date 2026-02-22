# Pattern Classifier — Phased Architecture Design

This document explains the recommended three-phase implementation plan for the
`wasmcloud-pattern-classifier` component, which consumes the VSA hypervectors
produced by [`wasmcloud-pattern-encoder`](https://github.com/64BitAsura/wasmcloud-pattern-encoder)
and classifies patterns and anomalies in real-time across streaming JSON messages.

---

## Context: What the Encoder Already Gives You

Every JSON message that flows through the encoder produces two kinds of Redis
entries that the classifier can read directly:

| Redis key pattern | Contents | Written by encoder |
|-------------------|----------|--------------------|
| `semantic:v1:{field}` | bincode-serialised `SparseVec` for one field | ✅ |
| `bundle:v1:{subject}` | bincode-serialised master bundle of all fields | ✅ |

The classifier does **not** re-encode. It reads those existing vectors and
builds its statistical / temporal state on top of them.

---

## Phase 1 — Foundation: VSA Cosine Statistics + Per-Field EMA
*(Solutions 1 + 3 combined; ~88 % confidence)*

### Goal

Detect message-level anomalies and pinpoint which field(s) are responsible.
Escalate to a human reviewer when the classifier is not confident enough to
decide autonomously.

### How the Two Sub-Solutions Compose

```
New NATS message on pattern.monitor.{subject}
│
│   [Encoder — already deployed]
│   Encodes fields → stores semantic:v1:{field} and bundle:v1:{subject}
│
▼
[Classifier component — Phase 1]
│
├── Sub-solution 1: Message-level cosine statistics
│     ├─ Read  bundle:v1:{subject}            ← new master bundle (encoder wrote it)
│     ├─ Read  stats:v1:{subject}:mean_vec    ← rolling mean bundle (classifier owns)
│     ├─ Read  stats:v1:{subject}:n           ← message count so far
│     ├─ score = cosine(new_bundle, mean_vec)
│     ├─ Read  stats:v1:{subject}:mean_score, :var_score
│     ├─ z = (score - mean_score) / stddev_score
│     ├─ Update rolling mean_vec via VSA bundle accumulation
│     ├─ Update Welford online mean/variance for score
│     ├─ Write stats:v1:{subject}:* back to Redis
│     └─ message_label = "anomaly" if |z| > THRESHOLD else "normal"
│
├── Sub-solution 3: Per-field EMA vote
│     For each field key in the message:
│       ├─ Read  semantic:v1:{field}           ← new field vector (encoder wrote it)
│       ├─ Read  ema:v1:{subject}:{field}      ← field EMA vector (classifier owns)
│       ├─ field_score = cosine(new_field_vec, ema_vec)
│       ├─ Update ema:v1:{subject}:{field} ← α·new + (1−α)·old  (bincode, stored)
│       └─ field_vote = "anomaly" if field_score < FIELD_THRESHOLD else "normal"
│
├── Combine
│     combined_confidence = weighted_avg(z_based_conf, field_vote_fraction)
│     anomalous_fields    = [field for field if field_vote == "anomaly"]
│
├── Emit result to NATS: pattern.classified.{subject}
│     {
│       "subject":           "pattern.monitor.sensors",
│       "classification":    "ANOMALY",      // or "NORMAL"
│       "confidence":        0.91,
│       "solutions": [
│         { "label": "field_spike:magnitude", "confidence": 0.93 },
│         { "label": "normal",                "confidence": 0.07 }
│       ],
│       "anomalous_fields":  ["magnitude"],
│       "hitl_required":     false,
│       "phase":             1
│     }
│
└── HITL gate
      if combined_confidence < HITL_THRESHOLD (default 0.70):
        Emit to NATS: pattern.alert.human.{subject}
          {
            "subject":          "pattern.monitor.sensors",
            "raw_payload":      { …original JSON… },
            "candidate_labels": [
              { "label": "field_spike:magnitude", "confidence": 0.55 },
              { "label": "normal",                "confidence": 0.45 }
            ],
            "message": "Classifier confidence below threshold; human review requested."
          }
        Human publishes reply to pattern.alert.human.{subject}.reply
          { "classification": "ANOMALY", "label": "field_spike:magnitude" }
        Classifier ingests human reply → updates EMA and stats as a confirmed sample
```

### Redis keys owned by Phase 1

| Key | Type | Description |
|-----|------|-------------|
| `stats:v1:{subject}:n` | u64 (bincode) | Message count for Welford accumulator |
| `stats:v1:{subject}:mean_vec` | SparseVec (bincode) | Rolling mean bundle |
| `stats:v1:{subject}:mean_score` | f32 (bincode) | Welford mean of cosine scores |
| `stats:v1:{subject}:var_score` | f32 (bincode) | Welford variance of cosine scores |
| `ema:v1:{subject}:{field}` | SparseVec (bincode) | Per-field EMA vector |
| `ema:v1:{subject}:{field}:alpha` | f32 (bincode) | Adaptive EMA decay per field |

### NATS topics used by Phase 1

| Topic | Direction | Description |
|-------|-----------|-------------|
| `pattern.monitor.>` | subscribe | Receives encoded JSON (same topic as encoder) |
| `pattern.classified.{subject}` | publish | Emits classification result |
| `pattern.alert.human.{subject}` | publish | Emits HITL review request |
| `pattern.alert.human.{subject}.reply` | subscribe | Receives human classification |

### Warm-up behaviour

For the first `MIN_SAMPLES` messages (default 30), confidence is always emitted
as `0.5` and the `hitl_required` flag is set to `true`. This bootstraps the
rolling statistics safely before the classifier starts making assertions.

---

## Phase 2 — Temporal Layer: Sliding-Window Bundle Drift
*(Solution 2; ~78 % confidence; layered on top of Phase 1)*

### Why Phase 2 is a "layer on top" and not a replacement

Phase 1 compares every *individual* message against a long-running historical
baseline. It catches sudden spikes and field-level outliers well. But it cannot
detect:

- A **gradual drift** where no single message is a spike but the stream is
  slowly moving away from baseline (e.g. sensor calibration drift, seasonal
  trend).
- A **regime change** — the system switching from one normal operating mode to
  another (e.g. daytime vs. night-time traffic patterns).
- **Temporal clustering** — a pattern that only appears when looking at a
  sequence of messages together, not individually.

Phase 2 adds a second, independent classification signal using VSA *windowed
superposition*. Both signals are then merged into a single combined
`solutions` array with individual confidences — the human or downstream
consumer can see both dimensions of evidence.

### How Phase 2 slots in

```
[Phase 1 runs first — unchanged]
│
▼
[Phase 2 temporal layer — runs after Phase 1 in the same handler]
│
├── Ring-buffer maintenance (Redis)
│     ├─ Read  window:ring:{subject}:head   ← current ring-buffer write pointer
│     ├─ Read  window:ring:{subject}:size   ← current fill count
│     ├─ Write bundle:v1:{subject}          ← new bundle into slot [head % WINDOW_N]
│     │         (key: window:slot:{subject}:{head % WINDOW_N})
│     └─ Advance head, update size in Redis
│
├── Build window bundles
│     window_now  = bundle( slot[head-N .. head] )     // last N bundles
│     window_prev = bundle( slot[head-2N .. head-N] )  // previous N bundles
│
│     [Both computed by reading N SparseVec entries from Redis and calling
│      SparseVec::bundle() — same operation the encoder already uses]
│
├── Compute drift signal
│     drift         = 1.0 - cosine(window_now, window_prev)
│     drift_rate    = slope of last W (drift, time) pairs (linear regression)
│     drift_accel   = second derivative (is drift itself accelerating?)
│
├── Classify drift
│     temporal_label = match (drift, drift_rate, drift_accel):
│       high drift, high rate      → "regime_change"       confidence ~0.88
│       high drift, low rate       → "gradual_trend_shift" confidence ~0.75
│       low drift, accelerating    → "emerging_trend"      confidence ~0.60
│       low drift, stable          → "temporal_normal"     confidence ~0.95
│
└── Merge with Phase 1 result
      final solutions list = Phase1.solutions + Phase2.temporal_solutions
      overall_confidence   = max(phase1_conf, temporal_conf)
      hitl_required        = hitl_required || (temporal_conf < HITL_THRESHOLD)

      Emits to pattern.classified.{subject}:
      {
        "classification": "ANOMALY",
        "confidence":     0.88,
        "solutions": [
          { "label": "field_spike:magnitude",  "confidence": 0.93, "source": "phase1" },
          { "label": "regime_change",          "confidence": 0.88, "source": "phase2" },
          { "label": "normal",                 "confidence": 0.07, "source": "phase1" }
        ],
        "anomalous_fields": ["magnitude"],
        "hitl_required": false,
        "phase": 2
      }
```

### Redis keys owned by Phase 2

| Key | Type | Description |
|-----|------|-------------|
| `window:ring:{subject}:head` | u64 (bincode) | Ring-buffer write pointer |
| `window:ring:{subject}:size` | u64 (bincode) | Fill count (0 to WINDOW_N) |
| `window:slot:{subject}:{i}` | SparseVec (bincode) | Bundle stored in ring slot i |
| `window:drift:{subject}:history` | Vec<f32> (bincode) | Recent drift values for slope |

### What changes in the component

Phase 2 adds a second `classify_temporal()` function that runs *after* the
existing Phase 1 logic inside the same `handle_message` handler. The Phase 1
output struct gains two optional fields (`temporal_label`, `temporal_confidence`)
that are populated by Phase 2. If Phase 2 has not yet accumulated enough data
(fewer than `2 * WINDOW_N` messages), it emits `null` for temporal fields and
does not affect the HITL gate.

**Backwards compatibility:** Phase 2 only *reads* the `bundle:v1:{subject}` key
that the encoder already writes. It does not modify Phase 1's Redis keys.

---

## Phase 3 — LLM Second Opinion Chain
*(Solution 4; phase-2 enhancement; ~65 % confidence)*

### Why LLM is deferred

The VSA-based phases (1 and 2) are:
- Fully deterministic and reproducible
- Compile to `wasm32-wasip2` with no external dependencies
- Zero latency penalty beyond Redis reads
- Free to operate

An LLM call:
- Costs money per invocation
- Adds 1–3 s of latency
- Introduces non-determinism (same input → different output)
- Requires `wasi:http/outgoing-handler` capability in the WADM manifest
- Has a privacy surface (JSON payloads leave your infrastructure)

Because of this, the LLM is **only invoked when Phases 1 and 2 both produce
low confidence**, meaning the VSA classifier genuinely cannot decide. It sits
at position 3 in a graceful degradation chain *before* escalating to a human.

### The full decision chain (Phase 3 active)

```
Phase 1 confidence ≥ 0.75?  → emit result, done
         │ no
         ▼
Phase 2 confidence ≥ 0.75?  → emit result, done
         │ no
         ▼
Phase 3: LLM call
  Build prompt:
    "You are an anomaly detection assistant.
     The following JSON message was received on subject '{subject}'.
     VSA classifier produced these candidate labels with confidences:
       {solutions list from Phase 1+2}
     Field-level anomaly signals: {anomalous_fields}
     Temporal drift signal: {drift, drift_rate, temporal_label}

     Please classify this message. Respond ONLY with JSON:
     {
       'classification': 'NORMAL' | 'ANOMALY',
       'label': '<most likely label>',
       'confidence': <0.0–1.0>,
       'explanation': '<one sentence>'
     }"

  LLM response confidence ≥ 0.75?  → emit result + explanation, done
           │ no
           ▼
  Escalate to human: pattern.alert.human.{subject}
    Include: VSA solutions, LLM explanation, raw payload
    Human replies with confirmed label
    Confirmed label fed back into Phase 1 EMA and stats
```

### What changes in the WADM manifest

Phase 3 adds one capability link in `wadm.yaml`:

```yaml
- type: link
  properties:
    target:
      name: http-client
    namespace: wasi
    package: http
    interfaces: [outgoing-handler]
```

The LLM API base URL and key are injected via a named config secret — they
never appear in source code.

### Privacy considerations

Before calling the LLM, the classifier applies a configurable **field mask**:
fields whose names match `PII_FIELD_PATTERNS` (e.g. `["email", "user_id",
"name", "ip"]`) are replaced with `<redacted>` in the prompt. The raw values
never leave the wasmCloud host.

---

## Composition Summary

The three phases are designed so each one is **additive and non-breaking**:

```
Phase 1 only:   message-level + field-level anomaly detection
                → works immediately with 0 new dependencies

Phase 1 + 2:    adds temporal drift / regime-change detection
                → same wasm binary, same WADM manifest, 2 × WINDOW_N message
                   warm-up before temporal signals activate

Phase 1 + 2 + 3: adds LLM second opinion for low-confidence cases
                → adds wasi:http capability link + secret config
                   LLM only called when both prior phases are uncertain
                   human escalation as final fallback in all phases
```

The `phase` field in every emitted `pattern.classified.{subject}` message
tells downstream consumers which level of analysis was active when the
classification was made, enabling observability dashboards to track how often
each phase is reached over time.

---

## Decision Flow Diagram (All Phases Active)

```
NATS: pattern.monitor.{subject}
            │
            ▼
  ┌─────────────────────────┐
  │  Phase 1 (always runs)  │  VSA cosine stats + per-field EMA
  │  confidence ≥ 0.75?     │──YES──► emit classified + done
  └──────────┬──────────────┘
             │ NO
             ▼
  ┌─────────────────────────┐
  │  Phase 2 (if warm)      │  Sliding-window temporal drift
  │  confidence ≥ 0.75?     │──YES──► emit classified + done
  └──────────┬──────────────┘
             │ NO (or not warm yet)
             ▼
  ┌─────────────────────────┐
  │  Phase 3 (if enabled)   │  LLM second opinion via wasi:http
  │  confidence ≥ 0.75?     │──YES──► emit classified + explanation
  └──────────┬──────────────┘
             │ NO
             ▼
  ┌─────────────────────────────────────────────────┐
  │  HITL: pattern.alert.human.{subject}            │
  │  Includes all candidate labels + confidences    │
  │  + LLM explanation (if Phase 3 ran)             │
  │  Human reply → fed back as confirmed sample     │
  └─────────────────────────────────────────────────┘
```

---

## Next Steps

1. **Confirm this design** — reply with any changes or constraints.
2. **Implement Phase 1** — the classifier component scaffold, WIT world,
   WADM manifest, unit tests, and integration test.
3. **Validate Phase 1** end-to-end against a running encoder in a local
   wasmCloud host.
4. **Layer Phase 2** once Phase 1 is stable and producing useful signals.
5. **Evaluate Phase 3** need based on observed HITL escalation rate from
   Phase 1+2 in production.
