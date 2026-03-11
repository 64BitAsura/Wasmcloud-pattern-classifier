// ── WIT bindings (excluded from test builds to allow native compilation) ──────
#[cfg(not(test))]
wit_bindgen::generate!({ generate_all });

#[cfg(not(test))]
use embeddenator_io::from_bincode;
use embeddenator_io::to_bincode;
use embeddenator_vsa::{ReversibleVSAConfig, SparseVec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Minimum messages before the classifier makes confident assertions.
#[allow(dead_code)]
pub(crate) const MIN_SAMPLES: u64 = 30;

/// Z-score threshold above which a message is declared anomalous.
#[allow(dead_code)]
pub(crate) const Z_THRESHOLD: f32 = 2.5;

/// Cosine similarity below which a field is considered anomalous.
#[allow(dead_code)]
pub(crate) const FIELD_THRESHOLD: f32 = 0.70;

/// Combined confidence threshold below which HITL escalation fires.
#[allow(dead_code)]
pub(crate) const HITL_THRESHOLD: f32 = 0.70;

/// Weight of the message-level cosine signal in the combined confidence score.
pub(crate) const MSG_SIGNAL_WEIGHT: f32 = 0.60;

/// Epsilon to prevent division by zero when computing Z-scores.
pub(crate) const EPSILON: f32 = 1e-6;

/// Maximum number of solutions returned in the classification output.
pub(crate) const MAX_SOLUTIONS: usize = 5;

// ── Phase 2 constants ─────────────────────────────────────────────────────────

/// Number of bundles per sliding window.
pub(crate) const WINDOW_N: usize = 20;

/// Drift value above which the signal is considered "high".
pub(crate) const DRIFT_WARN: f32 = 0.30;

/// Drift rate (slope) above which drift is considered "accelerating".
pub(crate) const SLOPE_WARN: f32 = 0.02;

/// Number of recent drift values kept for linear regression.
#[allow(dead_code)]
pub(crate) const HISTORY_LEN: usize = 10;

#[cfg(not(test))]
/// Redis bucket shared with the encoder (both components use the same instance).
pub(crate) const BUCKET_ID: &str = "pattern-monitor-vectors";

#[cfg(not(test))]
/// NATS subject prefix for classification result publications.
pub(crate) const SUBJECT_CLASSIFIED: &str = "pattern.classified";

#[cfg(not(test))]
/// NATS subject prefix for HITL human-review alerts.
pub(crate) const SUBJECT_ALERT: &str = "pattern.alert.human";

#[cfg(not(test))]
/// Suffix appended to the alert subject to form the human-reply subject.
pub(crate) const REPLY_SUFFIX: &str = ".reply";

// ── Output types (JSON-serialised and published on NATS) ──────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct Solution {
    pub label: String,
    pub confidence: f32,
    pub source: String,
}

#[derive(Debug, Serialize)]
pub struct ClassifyResult {
    pub subject: String,
    pub classification: String,
    pub confidence: f32,
    pub solutions: Vec<Solution>,
    pub anomalous_fields: Vec<String>,
    pub message_count: u64,
    pub hitl_required: bool,
    pub phase: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temporal_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temporal_confidence: Option<f32>,
}

#[derive(Debug, Serialize)]
pub struct HitlAlert {
    pub subject: String,
    pub payload: String,
    pub candidates: Vec<Solution>,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct HumanReply {
    pub classification: String,
    pub label: String,
}

// ── Pure encoding logic (testable on native target) ───────────────────────────

/// Parse a JSON object body and encode each key/value field as a bound VSA
/// hypervector.  Uses the same deterministic encoding as the encoder
/// (`ReversibleVSAConfig::default()`), so results are byte-identical.
pub(crate) fn encode_json_fields(body: &[u8]) -> Result<HashMap<String, SparseVec>, String> {
    let json: Value = serde_json::from_slice(body).map_err(|e| format!("JSON parse error: {e}"))?;
    let obj = json
        .as_object()
        .ok_or_else(|| "body is not a JSON object".to_string())?;
    let config = ReversibleVSAConfig::default();
    let mut fields = HashMap::new();
    for (key, value) in obj.iter() {
        let key_vec = SparseVec::encode_data(key.as_bytes(), &config, None);
        let val_vec = SparseVec::encode_data(value.to_string().as_bytes(), &config, None);
        fields.insert(key.clone(), key_vec.bind(&val_vec));
    }
    Ok(fields)
}

/// Bundle all field vectors into a single master bundle via VSA superposition.
/// Returns `None` when the map is empty.
pub(crate) fn build_master_bundle(fields: &HashMap<String, SparseVec>) -> Option<SparseVec> {
    let mut iter = fields.values();
    iter.next()
        .map(|first| iter.fold(first.clone(), |acc, v| acc.bundle(v)))
}

/// Welford online update step.
/// `n` must already be the updated count (incremented before calling).
/// Returns `(new_mean, new_m2)` where `m2` is the sum of squared deviations.
pub(crate) fn welford_update(n: u64, mean: f32, m2: f32, new_value: f32) -> (f32, f32) {
    let delta = new_value - mean;
    let new_mean = mean + delta / n as f32;
    let delta2 = new_value - new_mean;
    let new_m2 = m2 + delta * delta2;
    (new_mean, new_m2)
}

/// Compute the Z-score of `score` relative to the running distribution.
/// Returns `0.0` when `n < 2` (insufficient data for variance estimate).
pub(crate) fn compute_z_score(score: f32, mean: f32, m2: f32, n: u64) -> f32 {
    if n < 2 {
        return 0.0;
    }
    let variance = m2 / (n - 1) as f32;
    let stddev = variance.sqrt().max(EPSILON);
    (score - mean).abs() / stddev
}

/// Map a Z-score to a confidence value in `[0, 1)` using `1 − exp(−0.5·z²)`.
/// At `z = 0` → `0.0`; as `z → ∞` → `1.0`.
pub(crate) fn z_to_confidence(z: f32) -> f32 {
    1.0 - (-0.5 * z * z).exp()
}

/// Update a running bundle (approximate semantic centroid via VSA superposition).
/// Returns the new vector; on cold-start (`old_bundle = None`) returns a clone
/// of `new_vec`.
pub(crate) fn update_running_bundle(
    old_bundle: Option<&SparseVec>,
    new_vec: &SparseVec,
) -> SparseVec {
    match old_bundle {
        Some(old) => old.bundle(new_vec),
        None => new_vec.clone(),
    }
}

/// Compute combined confidence from the message-level and field-level signals.
pub(crate) fn combined_confidence(msg_confidence: f32, field_anomaly_fraction: f32) -> f32 {
    (MSG_SIGNAL_WEIGHT * msg_confidence + (1.0 - MSG_SIGNAL_WEIGHT) * field_anomaly_fraction)
        .clamp(0.0, 1.0)
}

/// Build the ordered solutions list from message and field signals.
/// `field_results` is a slice of `(field_name, cosine_score, is_anomaly)`.
pub(crate) fn build_solutions(
    is_msg_anomaly: bool,
    msg_confidence: f32,
    field_results: &[(String, f32, bool)],
) -> Vec<Solution> {
    let mut solutions: Vec<Solution> = Vec::new();

    for (field_name, score, is_anomaly) in field_results {
        if *is_anomaly {
            solutions.push(Solution {
                label: format!("field_spike:{field_name}"),
                confidence: (1.0 - score).clamp(0.0, 1.0),
                source: "phase1".to_string(),
            });
        }
    }

    if is_msg_anomaly {
        solutions.push(Solution {
            label: "message_level_anomaly".to_string(),
            confidence: msg_confidence,
            source: "phase1".to_string(),
        });
    }

    solutions.push(Solution {
        label: "normal".to_string(),
        confidence: (1.0 - msg_confidence).clamp(0.0, 1.0),
        source: "phase1".to_string(),
    });

    solutions.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    solutions.truncate(MAX_SOLUTIONS);
    solutions
}

// ── Phase 2: Temporal Layer ───────────────────────────────────────────────────

/// Result of the Phase 2 temporal drift analysis.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct TemporalResult {
    pub label: String,
    pub confidence: f32,
    pub drift: f32,
    pub drift_rate: f32,
}

/// Compute the slope of a linear regression fit to `values` indexed by position.
/// Returns `0.0` when fewer than 2 data points are available.
pub(crate) fn linear_slope(values: &[f32]) -> f32 {
    let n = values.len();
    if n < 2 {
        return 0.0;
    }
    let n_f = n as f32;
    let mut sum_x: f32 = 0.0;
    let mut sum_y: f32 = 0.0;
    let mut sum_xy: f32 = 0.0;
    let mut sum_x2: f32 = 0.0;
    for (i, &y) in values.iter().enumerate() {
        let x = i as f32;
        sum_x += x;
        sum_y += y;
        sum_xy += x * y;
        sum_x2 += x * x;
    }
    let denom = n_f * sum_x2 - sum_x * sum_x;
    if denom.abs() < EPSILON {
        return 0.0;
    }
    (n_f * sum_xy - sum_x * sum_y) / denom
}

/// Classify drift and drift_rate into a temporal label with confidence.
pub(crate) fn classify_drift(drift: f32, drift_rate: f32) -> (&'static str, f32) {
    if drift >= DRIFT_WARN && drift_rate >= SLOPE_WARN {
        ("regime_change", 0.88)
    } else if drift >= DRIFT_WARN {
        ("gradual_trend_shift", 0.75)
    } else if drift_rate >= SLOPE_WARN {
        ("emerging_trend", 0.60)
    } else {
        ("temporal_normal", 0.95)
    }
}

// ── wasmCloud component implementation (excluded from test builds) ─────────────

#[cfg(not(test))]
fn kv_err(e: crate::wasi::keyvalue::store::Error) -> String {
    use crate::wasi::keyvalue::store::Error;
    match e {
        Error::NoSuchStore => "keyvalue: no such store".to_string(),
        Error::AccessDenied => "keyvalue: access denied".to_string(),
        Error::Other(msg) => format!("keyvalue: {msg}"),
    }
}

#[cfg(not(test))]
fn load_vec(bucket: &crate::wasi::keyvalue::store::Bucket, key: &str) -> Option<SparseVec> {
    bucket
        .get(key)
        .ok()
        .flatten()
        .and_then(|bytes| from_bincode::<SparseVec>(&bytes).ok())
}

#[cfg(not(test))]
fn save_vec(
    bucket: &crate::wasi::keyvalue::store::Bucket,
    key: &str,
    vec: &SparseVec,
) -> Result<(), String> {
    let bytes = to_bincode(vec).map_err(|e| format!("bincode error: {e}"))?;
    bucket.set(key, &bytes).map_err(kv_err)
}

#[cfg(not(test))]
fn load_u64(bucket: &crate::wasi::keyvalue::store::Bucket, key: &str) -> u64 {
    bucket
        .get(key)
        .ok()
        .flatten()
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
        .unwrap_or(0)
}

#[cfg(not(test))]
fn save_u64(bucket: &crate::wasi::keyvalue::store::Bucket, key: &str, val: u64) {
    let _ = bucket.set(key, &val.to_le_bytes());
}

#[cfg(not(test))]
fn load_f32(bucket: &crate::wasi::keyvalue::store::Bucket, key: &str) -> f32 {
    bucket
        .get(key)
        .ok()
        .flatten()
        .and_then(|b| b.try_into().ok())
        .map(f32::from_le_bytes)
        .unwrap_or(0.0)
}

#[cfg(not(test))]
fn save_f32(bucket: &crate::wasi::keyvalue::store::Bucket, key: &str, val: f32) {
    let _ = bucket.set(key, &val.to_le_bytes());
}

/// Load a drift history vector from Redis (bincode-serialised `Vec<f32>`).
#[cfg(not(test))]
fn load_drift_history(bucket: &crate::wasi::keyvalue::store::Bucket, subject: &str) -> Vec<f32> {
    let key = format!("window:drift:{subject}:history");
    bucket
        .get(&key)
        .ok()
        .flatten()
        .and_then(|bytes| from_bincode::<Vec<f32>>(&bytes).ok())
        .unwrap_or_default()
}

/// Save drift history vector to Redis.
#[cfg(not(test))]
fn save_drift_history(
    bucket: &crate::wasi::keyvalue::store::Bucket,
    subject: &str,
    history: &[f32],
) {
    let key = format!("window:drift:{subject}:history");
    if let Ok(bytes) = to_bincode(&history.to_vec()) {
        let _ = bucket.set(&key, &bytes);
    }
}

/// Phase 2 temporal classification using sliding-window bundle drift.
#[cfg(not(test))]
fn classify_temporal(
    bucket: &crate::wasi::keyvalue::store::Bucket,
    subject: &str,
    new_bundle: &SparseVec,
) -> TemporalResult {
    // ── Ring-buffer maintenance ──────────────────────────────────────────────
    let head_key = format!("window:ring:{subject}:head");
    let size_key = format!("window:ring:{subject}:size");

    let head = load_u64(bucket, &head_key);
    let size = load_u64(bucket, &size_key);

    // Store the new bundle in the ring slot
    let slot_index = (head as usize) % WINDOW_N;
    let slot_key = format!("window:slot:{subject}:{slot_index}");
    let _ = save_vec(bucket, &slot_key, new_bundle);

    let new_head = head.wrapping_add(1);
    let new_size = (size + 1).min(2 * WINDOW_N as u64);
    save_u64(bucket, &head_key, new_head);
    save_u64(bucket, &size_key, new_size);

    // ── Warm-up gate: need at least 2 full windows ──────────────────────────
    if new_size < 2 * WINDOW_N as u64 {
        return TemporalResult {
            label: "temporal_warming_up".to_string(),
            confidence: 0.5,
            drift: 0.0,
            drift_rate: 0.0,
        };
    }

    // ── Build window_now and window_prev from ring slots ────────────────────
    let mut window_now_vecs: Vec<SparseVec> = Vec::with_capacity(WINDOW_N);
    let mut window_prev_vecs: Vec<SparseVec> = Vec::with_capacity(WINDOW_N);

    for i in 0..WINDOW_N {
        // window_now: last WINDOW_N slots before new_head
        let now_idx = ((new_head as usize)
            .wrapping_sub(WINDOW_N - i)
            .wrapping_sub(1))
            % WINDOW_N;
        let now_key = format!("window:slot:{subject}:{now_idx}");
        if let Some(v) = load_vec(bucket, &now_key) {
            window_now_vecs.push(v);
        }

        // window_prev: WINDOW_N slots before window_now
        let prev_idx = ((new_head as usize)
            .wrapping_sub(2 * WINDOW_N - i)
            .wrapping_sub(1))
            % WINDOW_N;
        let prev_key = format!("window:slot:{subject}:{prev_idx}");
        if let Some(v) = load_vec(bucket, &prev_key) {
            window_prev_vecs.push(v);
        }
    }

    // Bundle the windows
    let window_now = window_now_vecs
        .iter()
        .skip(1)
        .fold(window_now_vecs.first().cloned(), |acc, v| {
            acc.map(|a| a.bundle(v))
        });
    let window_prev = window_prev_vecs
        .iter()
        .skip(1)
        .fold(window_prev_vecs.first().cloned(), |acc, v| {
            acc.map(|a| a.bundle(v))
        });

    // Compute drift
    let drift = match (&window_now, &window_prev) {
        (Some(now), Some(prev)) => (1.0 - now.cosine(prev) as f32).max(0.0),
        _ => 0.0,
    };

    // ── Maintain drift history for slope estimation ─────────────────────────
    let mut history = load_drift_history(bucket, subject);
    history.push(drift);
    if history.len() > HISTORY_LEN {
        history.remove(0);
    }
    save_drift_history(bucket, subject, &history);

    // ── Classify ────────────────────────────────────────────────────────────
    let drift_rate = linear_slope(&history);
    let (label, confidence) = classify_drift(drift, drift_rate);

    TemporalResult {
        label: label.to_string(),
        confidence,
        drift,
        drift_rate,
    }
}

/// Ingest a confirmed human classification reply and update the baseline stats.
#[cfg(not(test))]
fn handle_human_reply(
    bucket: &crate::wasi::keyvalue::store::Bucket,
    subject: &str,
    reply_body: &[u8],
) {
    use crate::wasi::logging::logging::{log, Level};

    let reply: HumanReply = match serde_json::from_slice(reply_body) {
        Ok(r) => r,
        Err(e) => {
            log(
                Level::Warn,
                "pattern-classifier",
                &format!("invalid human reply JSON: {e}"),
            );
            return;
        }
    };

    log(
        Level::Info,
        "pattern-classifier",
        &format!(
            "human confirmed for '{}': classification={} label={}",
            subject, reply.classification, reply.label
        ),
    );

    // Increment the message count so confirmed samples count toward warm-up.
    let n_key = format!("stats:v1:{subject}:n");
    let n = load_u64(bucket, &n_key).saturating_add(1);
    save_u64(bucket, &n_key, n);
}

/// Core classification logic for a single message.
#[cfg(not(test))]
fn classify_message(
    bucket: &crate::wasi::keyvalue::store::Bucket,
    subject: &str,
    body: &[u8],
) -> Result<ClassifyResult, String> {
    use crate::wasi::logging::logging::{log, Level};

    // ── Step 0: Parse JSON and encode fields ──────────────────────────────────
    let fields = match encode_json_fields(body) {
        Ok(f) if f.is_empty() => {
            log(
                Level::Warn,
                "pattern-classifier",
                "empty JSON object — skipping",
            );
            return Ok(ClassifyResult {
                subject: subject.to_string(),
                classification: "SKIPPED".to_string(),
                confidence: 0.0,
                solutions: vec![],
                anomalous_fields: vec![],
                message_count: 0,
                hitl_required: false,
                phase: 1,
                temporal_label: None,
                temporal_confidence: None,
            });
        }
        Ok(f) => f,
        Err(e) => {
            log(
                Level::Warn,
                "pattern-classifier",
                &format!("JSON error — skipping: {e}"),
            );
            return Err(e);
        }
    };

    let new_bundle =
        build_master_bundle(&fields).ok_or_else(|| "failed to build master bundle".to_string())?;

    // ── Step 1: Load and increment message counter ────────────────────────────
    let n_key = format!("stats:v1:{subject}:n");
    let n = load_u64(bucket, &n_key).saturating_add(1);

    // ── Warm-up gate: accumulate baseline without asserting anomalies ─────────
    if n <= MIN_SAMPLES {
        let mean_vec_key = format!("stats:v1:{subject}:mean_vec");
        let old_mean = load_vec(bucket, &mean_vec_key);
        let new_mean_vec = update_running_bundle(old_mean.as_ref(), &new_bundle);
        if let Err(e) = save_vec(bucket, &mean_vec_key, &new_mean_vec) {
            log(
                Level::Warn,
                "pattern-classifier",
                &format!("save mean_vec error: {e}"),
            );
        }
        for (field_name, field_vec) in &fields {
            let ema_key = format!("ema:v1:{subject}:{field_name}");
            let old_ema = load_vec(bucket, &ema_key);
            let new_ema = update_running_bundle(old_ema.as_ref(), field_vec);
            if let Err(e) = save_vec(bucket, &ema_key, &new_ema) {
                log(
                    Level::Warn,
                    "pattern-classifier",
                    &format!("save ema '{field_name}' error: {e}"),
                );
            }
        }
        save_u64(bucket, &n_key, n);
        log(
            Level::Debug,
            "pattern-classifier",
            &format!("warming up ({n}/{MIN_SAMPLES}) for '{subject}'"),
        );

        // Phase 2: feed the ring buffer even during warm-up
        let temporal = classify_temporal(bucket, subject, &new_bundle);
        let (temporal_label, temporal_confidence) = if temporal.label == "temporal_warming_up" {
            (None, None)
        } else {
            (Some(temporal.label), Some(temporal.confidence))
        };

        return Ok(ClassifyResult {
            subject: subject.to_string(),
            classification: "WARMING_UP".to_string(),
            confidence: 0.5,
            solutions: vec![Solution {
                label: "warming_up".to_string(),
                confidence: 0.5,
                source: "phase1".to_string(),
            }],
            anomalous_fields: vec![],
            message_count: n,
            // HITL is not escalated during warm-up; the component intentionally
            // skips the alert for WARMING_UP results (see handle_message).
            hitl_required: false,
            phase: 1,
            temporal_label,
            temporal_confidence,
        });
    }

    // ── Step 2: Message-level cosine statistics (Sub-solution 1) ─────────────
    let mean_vec_key = format!("stats:v1:{subject}:mean_vec");
    let mean_score_key = format!("stats:v1:{subject}:mean_score");
    let m2_key = format!("stats:v1:{subject}:m2");

    let old_mean_vec = load_vec(bucket, &mean_vec_key);
    let old_mean_score = load_f32(bucket, &mean_score_key);
    let old_m2 = load_f32(bucket, &m2_key);

    let score = match &old_mean_vec {
        Some(mean) => new_bundle.cosine(mean) as f32,
        None => 1.0_f32,
    };

    let (new_mean_score, new_m2) = welford_update(n, old_mean_score, old_m2, score);
    let z = compute_z_score(score, new_mean_score, new_m2, n);
    let msg_confidence = z_to_confidence(z);
    let is_msg_anomaly = z > Z_THRESHOLD;

    let new_mean_vec = update_running_bundle(old_mean_vec.as_ref(), &new_bundle);
    save_vec(bucket, &mean_vec_key, &new_mean_vec)?;
    save_f32(bucket, &mean_score_key, new_mean_score);
    save_f32(bucket, &m2_key, new_m2);
    save_u64(bucket, &n_key, n);

    // ── Step 3: Per-field running-bundle comparison (Sub-solution 3) ──────────
    let mut field_results: Vec<(String, f32, bool)> = Vec::new();
    for (field_name, field_vec) in &fields {
        let ema_key = format!("ema:v1:{subject}:{field_name}");
        let old_ema = load_vec(bucket, &ema_key);

        let field_score = match &old_ema {
            Some(ema) => field_vec.cosine(ema) as f32,
            None => 1.0_f32,
        };

        let new_ema = update_running_bundle(old_ema.as_ref(), field_vec);
        if let Err(e) = save_vec(bucket, &ema_key, &new_ema) {
            log(
                Level::Warn,
                "pattern-classifier",
                &format!("save ema '{field_name}' error: {e}"),
            );
        }

        let is_field_anomaly = field_score < FIELD_THRESHOLD;
        field_results.push((field_name.clone(), field_score, is_field_anomaly));
    }

    // ── Step 4: Combine signals ───────────────────────────────────────────────
    let anomaly_count = field_results.iter().filter(|(_, _, a)| *a).count();
    let field_anomaly_fraction = if field_results.is_empty() {
        0.0
    } else {
        anomaly_count as f32 / field_results.len() as f32
    };

    let confidence = combined_confidence(msg_confidence, field_anomaly_fraction);

    let classification = if is_msg_anomaly || field_anomaly_fraction > 0.5 {
        "ANOMALY"
    } else {
        "NORMAL"
    };

    let anomalous_fields: Vec<String> = field_results
        .iter()
        .filter(|(_, _, a)| *a)
        .map(|(name, _, _)| name.clone())
        .collect();

    let solutions = build_solutions(is_msg_anomaly, msg_confidence, &field_results);

    // ── Step 5: Phase 2 — temporal drift classification ───────────────────────
    let temporal = classify_temporal(bucket, subject, &new_bundle);
    let temporal_warming = temporal.label == "temporal_warming_up";

    // Merge Phase 2 temporal solution into the solutions list
    let mut merged_solutions = solutions;
    if !temporal_warming {
        merged_solutions.push(Solution {
            label: temporal.label.clone(),
            confidence: temporal.confidence,
            source: "phase2".to_string(),
        });
        merged_solutions.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        merged_solutions.truncate(MAX_SOLUTIONS);
    }

    // Determine overall confidence and HITL requirement
    let overall_confidence = if temporal_warming {
        confidence
    } else {
        confidence.max(temporal.confidence)
    };

    let hitl_required = if temporal_warming {
        confidence < HITL_THRESHOLD
    } else {
        confidence < HITL_THRESHOLD || temporal.confidence < HITL_THRESHOLD
    };

    let phase: u8 = if temporal_warming { 1 } else { 2 };

    let (temporal_label, temporal_confidence) = if temporal_warming {
        (None, None)
    } else {
        (Some(temporal.label), Some(temporal.confidence))
    };

    log(
        Level::Info,
        "pattern-classifier",
        &format!(
            "subject='{}' class={} conf={:.3} z={:.3} fields={} anomalous={:?} phase={}",
            subject,
            classification,
            overall_confidence,
            z,
            field_results.len(),
            anomalous_fields,
            phase
        ),
    );

    Ok(ClassifyResult {
        subject: subject.to_string(),
        classification: classification.to_string(),
        confidence: overall_confidence,
        solutions: merged_solutions,
        anomalous_fields,
        message_count: n,
        hitl_required,
        phase,
        temporal_label,
        temporal_confidence,
    })
}

#[cfg(not(test))]
struct PatternClassifier;

#[cfg(not(test))]
impl crate::exports::wasmcloud::messaging::handler::Guest for PatternClassifier {
    fn handle_message(
        msg: crate::exports::wasmcloud::messaging::handler::BrokerMessage,
    ) -> Result<(), String> {
        use crate::wasi::keyvalue::store;
        use crate::wasi::logging::logging::{log, Level};
        use crate::wasmcloud::messaging::consumer;

        let subject = msg.subject.clone();

        log(
            Level::Info,
            "pattern-classifier",
            &format!(
                "received message on '{}' ({} bytes)",
                subject,
                msg.body.len()
            ),
        );

        let bucket = store::open(BUCKET_ID).map_err(kv_err)?;

        // Route: HITL human reply vs. regular stream message
        if subject.ends_with(REPLY_SUFFIX) {
            // Derive the stream subject from the reply subject.
            // e.g. "pattern.alert.human.sensors.reply" → base = "sensors"
            let base = subject
                .strip_suffix(REPLY_SUFFIX)
                .and_then(|s| s.strip_prefix(&format!("{SUBJECT_ALERT}.")))
                .unwrap_or(&subject);
            handle_human_reply(&bucket, base, &msg.body);
            return Ok(());
        }

        // Classify the message
        let result = classify_message(&bucket, &subject, &msg.body)?;

        // Publish classification result
        let result_json =
            serde_json::to_string(&result).map_err(|e| format!("serialise result: {e}"))?;
        consumer::publish(&consumer::BrokerMessage {
            subject: format!("{SUBJECT_CLASSIFIED}.{subject}"),
            body: result_json.into_bytes(),
            reply_to: None,
        })
        .map_err(|e| format!("publish classified: {e}"))?;

        // HITL escalation when confidence is below the threshold
        if result.hitl_required && result.classification != "WARMING_UP" {
            let leaf = subject.strip_prefix("pattern.monitor.").unwrap_or(&subject);
            let alert_subject = format!("{SUBJECT_ALERT}.{leaf}");
            let reply_subject = format!("{alert_subject}{REPLY_SUFFIX}");

            let alert = HitlAlert {
                subject: subject.clone(),
                payload: String::from_utf8_lossy(&msg.body).to_string(),
                candidates: result.solutions.clone(),
                message: format!(
                    "Classifier confidence {:.2} is below threshold {:.2}; human review requested.",
                    result.confidence, HITL_THRESHOLD
                ),
            };
            let alert_json =
                serde_json::to_string(&alert).map_err(|e| format!("serialise alert: {e}"))?;
            consumer::publish(&consumer::BrokerMessage {
                subject: alert_subject,
                body: alert_json.into_bytes(),
                reply_to: Some(reply_subject),
            })
            .map_err(|e| format!("publish alert: {e}"))?;

            log(
                Level::Warn,
                "pattern-classifier",
                &format!(
                    "HITL alert sent for '{}' (confidence={:.2})",
                    subject, result.confidence
                ),
            );
        }

        Ok(())
    }
}

#[cfg(not(test))]
export!(PatternClassifier);

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── encode_json_fields ────────────────────────────────────────────────────

    #[test]
    fn test_encode_json_fields_parses_object() {
        let body = br#"{"event":"quake","magnitude":"6.2"}"#;
        let fields = encode_json_fields(body).expect("should parse");
        assert_eq!(fields.len(), 2);
        assert!(fields.contains_key("event"));
        assert!(fields.contains_key("magnitude"));
    }

    #[test]
    fn test_encode_json_fields_rejects_array() {
        assert!(encode_json_fields(b"[1,2,3]").is_err());
    }

    #[test]
    fn test_encode_json_fields_rejects_invalid_json() {
        let err = encode_json_fields(b"not json").unwrap_err();
        assert!(err.contains("JSON parse error"), "got: {err}");
    }

    #[test]
    fn test_encode_json_fields_rejects_non_object() {
        assert!(encode_json_fields(br#""just a string""#).is_err());
    }

    #[test]
    fn test_encode_json_fields_deterministic() {
        let body = br#"{"sensor":"temperature","value":"42.5"}"#;
        let a = encode_json_fields(body).unwrap();
        let b = encode_json_fields(body).unwrap();
        for key in a.keys() {
            let av = to_bincode(&a[key]).unwrap();
            let bv = to_bincode(&b[key]).unwrap();
            assert_eq!(av, bv, "field '{key}' must be deterministic");
        }
    }

    // ── build_master_bundle ───────────────────────────────────────────────────

    #[test]
    fn test_build_master_bundle_multi_field() {
        let fields = encode_json_fields(br#"{"a":"1","b":"2","c":"3"}"#).unwrap();
        assert!(build_master_bundle(&fields).is_some());
    }

    #[test]
    fn test_build_master_bundle_single_field() {
        let fields = encode_json_fields(br#"{"only":"field"}"#).unwrap();
        assert!(build_master_bundle(&fields).is_some());
    }

    #[test]
    fn test_build_master_bundle_empty() {
        let empty: HashMap<String, SparseVec> = HashMap::new();
        assert!(build_master_bundle(&empty).is_none());
    }

    // ── Welford stats ─────────────────────────────────────────────────────────

    #[test]
    fn test_welford_single_sample() {
        let (mean, m2) = welford_update(1, 0.0, 0.0, 0.8);
        assert!((mean - 0.8).abs() < 1e-5, "mean={mean}");
        assert!(m2.abs() < 1e-5, "m2={m2}");
    }

    #[test]
    fn test_welford_two_samples() {
        // n=1: value=0.8 → mean=0.8, m2=0
        let (m1, m2_1) = welford_update(1, 0.0, 0.0, 0.8);
        // n=2: value=0.6 → mean=0.7, m2=0.02
        let (m2, m2_2) = welford_update(2, m1, m2_1, 0.6);
        assert!((m2 - 0.7).abs() < 1e-5, "mean={m2}");
        assert!((m2_2 - 0.02).abs() < 1e-5, "m2={m2_2}");
    }

    #[test]
    fn test_compute_z_score_insufficient_data() {
        assert_eq!(compute_z_score(0.5, 0.8, 0.02, 1), 0.0);
    }

    #[test]
    fn test_compute_z_score_at_mean() {
        // score == mean → z = 0
        let z = compute_z_score(0.7, 0.7, 0.02, 10);
        assert!(z.abs() < 1e-5, "z={z}");
    }

    #[test]
    fn test_z_to_confidence_at_zero() {
        let c = z_to_confidence(0.0);
        assert!(c.abs() < 1e-5, "c={c}");
    }

    #[test]
    fn test_z_to_confidence_large_z() {
        let c = z_to_confidence(5.0);
        assert!(c > 0.99, "c={c}");
    }

    // ── update_running_bundle ─────────────────────────────────────────────────

    #[test]
    fn test_update_running_bundle_cold_start() {
        let fields = encode_json_fields(br#"{"k":"v"}"#).unwrap();
        let vec = fields.values().next().unwrap();
        let result = update_running_bundle(None, vec);
        let a = to_bincode(&result).unwrap();
        let b = to_bincode(vec).unwrap();
        assert_eq!(a, b, "cold-start must equal input vector");
    }

    // ── combined_confidence ───────────────────────────────────────────────────

    #[test]
    fn test_combined_confidence_msg_only() {
        let c = combined_confidence(0.9, 0.0);
        let expected = 0.6 * 0.9;
        assert!((c - expected).abs() < 1e-5, "c={c}");
    }

    #[test]
    fn test_combined_confidence_field_only() {
        let c = combined_confidence(0.0, 1.0);
        let expected = 0.4;
        assert!((c - expected).abs() < 1e-5, "c={c}");
    }

    // ── build_solutions ───────────────────────────────────────────────────────

    #[test]
    fn test_build_solutions_anomaly() {
        let field_results = vec![
            ("magnitude".to_string(), 0.3, true),
            ("location".to_string(), 0.85, false),
        ];
        let solutions = build_solutions(true, 0.9, &field_results);
        let labels: Vec<&str> = solutions.iter().map(|s| s.label.as_str()).collect();
        assert!(labels.contains(&"field_spike:magnitude"));
        assert!(labels.contains(&"message_level_anomaly"));
        assert!(labels.contains(&"normal"));
    }

    #[test]
    fn test_build_solutions_normal() {
        let field_results = vec![
            ("a".to_string(), 0.95, false),
            ("b".to_string(), 0.90, false),
        ];
        let solutions = build_solutions(false, 0.1, &field_results);
        assert_eq!(solutions[0].label, "normal");
    }

    #[test]
    fn test_build_solutions_capped_at_max() {
        let field_results: Vec<_> = (0..10).map(|i| (format!("f{i}"), 0.2, true)).collect();
        let solutions = build_solutions(true, 0.9, &field_results);
        assert!(solutions.len() <= MAX_SOLUTIONS);
    }

    // ── VSA cosine via SparseVec ──────────────────────────────────────────────

    #[test]
    fn test_cosine_self_similarity() {
        let fields = encode_json_fields(br#"{"sensor":"temperature","val":"42"}"#).unwrap();
        let vec = fields.values().next().unwrap();
        let sim = vec.cosine(vec) as f32;
        assert!(sim > 0.99, "self-cosine should be ~1.0, got {sim}");
    }

    #[test]
    fn test_cosine_different_fields_lower_than_self() {
        let f1 = encode_json_fields(br#"{"sensor":"temperature"}"#).unwrap();
        let f2 = encode_json_fields(br#"{"event":"earthquake"}"#).unwrap();
        let v1 = f1.values().next().unwrap();
        let v2 = f2.values().next().unwrap();
        let cross = v1.cosine(v2) as f32;
        let self_sim = v1.cosine(v1) as f32;
        assert!(cross < self_sim, "cross={cross} self_sim={self_sim}");
    }

    // ── Phase 2: linear_slope ─────────────────────────────────────────────────

    #[test]
    fn test_linear_slope_increasing() {
        // Monotonically increasing drift history → positive slope
        let history = vec![0.0, 0.1, 0.2, 0.3, 0.4];
        let slope = linear_slope(&history);
        assert!(slope > 0.09, "slope should be ~0.1, got {slope}");
    }

    #[test]
    fn test_linear_slope_flat() {
        // Flat drift history → slope ≈ 0
        let history = vec![0.5, 0.5, 0.5, 0.5, 0.5];
        let slope = linear_slope(&history);
        assert!(slope.abs() < 1e-5, "slope should be ~0, got {slope}");
    }

    #[test]
    fn test_linear_slope_single_point() {
        // Fewer than 2 points → 0.0
        let history = vec![0.3];
        let slope = linear_slope(&history);
        assert_eq!(slope, 0.0);
    }

    #[test]
    fn test_linear_slope_empty() {
        let slope = linear_slope(&[]);
        assert_eq!(slope, 0.0);
    }

    // ── Phase 2: classify_drift ───────────────────────────────────────────────

    #[test]
    fn test_classify_temporal_regime_change() {
        let (label, conf) = classify_drift(0.40, 0.05);
        assert_eq!(label, "regime_change");
        assert!((conf - 0.88).abs() < 1e-5, "conf={conf}");
    }

    #[test]
    fn test_classify_temporal_gradual() {
        let (label, conf) = classify_drift(0.35, 0.01);
        assert_eq!(label, "gradual_trend_shift");
        assert!((conf - 0.75).abs() < 1e-5, "conf={conf}");
    }

    #[test]
    fn test_classify_temporal_emerging() {
        let (label, conf) = classify_drift(0.10, 0.05);
        assert_eq!(label, "emerging_trend");
        assert!((conf - 0.60).abs() < 1e-5, "conf={conf}");
    }

    #[test]
    fn test_classify_temporal_normal() {
        let (label, conf) = classify_drift(0.10, 0.01);
        assert_eq!(label, "temporal_normal");
        assert!((conf - 0.95).abs() < 1e-5, "conf={conf}");
    }

    // ── Phase 2: ring buffer / window logic ───────────────────────────────────

    #[test]
    fn test_ring_buffer_wrap_around() {
        // Verify that head % WINDOW_N wraps correctly
        for head in 0..(3 * WINDOW_N) {
            let slot = head % WINDOW_N;
            assert!(slot < WINDOW_N, "slot={slot} must be < WINDOW_N={WINDOW_N}");
        }
        // Specific wrap boundary
        assert_eq!(WINDOW_N % WINDOW_N, 0);
        assert_eq!((WINDOW_N + 1) % WINDOW_N, 1);
        assert_eq!((2 * WINDOW_N - 1) % WINDOW_N, WINDOW_N - 1);
    }

    #[test]
    fn test_drift_zero_for_identical_windows() {
        // Two identical bundles → cosine ≈ 1.0, drift ≈ 0.0
        let fields = encode_json_fields(br#"{"sensor":"temperature","value":"42"}"#).unwrap();
        let bundle = build_master_bundle(&fields).unwrap();
        let drift = 1.0 - bundle.cosine(&bundle) as f32;
        assert!(
            drift.abs() < 0.05,
            "identical bundles should have drift ≈ 0, got {drift}"
        );
    }

    #[test]
    fn test_drift_high_for_different_windows() {
        // Two very different bundles → lower cosine, higher drift
        let f1 = encode_json_fields(br#"{"sensor":"temperature","value":"42"}"#).unwrap();
        let f2 = encode_json_fields(br#"{"event":"earthquake","magnitude":"9.9"}"#).unwrap();
        let b1 = build_master_bundle(&f1).unwrap();
        let b2 = build_master_bundle(&f2).unwrap();
        let cosine = b1.cosine(&b2) as f32;
        let drift = (1.0 - cosine).max(0.0);
        // Different bundles should have higher drift than identical ones
        let self_drift = 1.0 - b1.cosine(&b1) as f32;
        assert!(
            drift > self_drift,
            "different bundles drift={drift} should exceed self drift={self_drift}"
        );
    }

    #[test]
    fn test_window_bundle_warming_up_size() {
        // Phase 2 requires 2 × WINDOW_N messages before producing temporal results.
        // Before that, size < 2*WINDOW_N indicates warming up.
        let required = 2 * WINDOW_N as u64;
        assert_eq!(required, 40, "need 40 messages for Phase 2 warm-up");

        // Simulate size counter growth
        for msg in 1..=50u64 {
            let size = msg.min(required);
            if msg < required {
                assert!(size < required, "msg={msg}: should still be warming up");
            } else {
                assert_eq!(size, required, "msg={msg}: should be at capacity");
            }
        }
    }
}
