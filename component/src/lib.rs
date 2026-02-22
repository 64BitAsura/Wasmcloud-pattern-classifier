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
            });
        }
    }

    if is_msg_anomaly {
        solutions.push(Solution {
            label: "message_level_anomaly".to_string(),
            confidence: msg_confidence,
        });
    }

    solutions.push(Solution {
        label: "normal".to_string(),
        confidence: (1.0 - msg_confidence).clamp(0.0, 1.0),
    });

    solutions.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    solutions.truncate(MAX_SOLUTIONS);
    solutions
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
        return Ok(ClassifyResult {
            subject: subject.to_string(),
            classification: "WARMING_UP".to_string(),
            confidence: 0.5,
            solutions: vec![Solution {
                label: "warming_up".to_string(),
                confidence: 0.5,
            }],
            anomalous_fields: vec![],
            message_count: n,
            // HITL is not escalated during warm-up; the component intentionally
            // skips the alert for WARMING_UP results (see handle_message).
            hitl_required: false,
            phase: 1,
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

    log(
        Level::Info,
        "pattern-classifier",
        &format!(
            "subject='{}' class={} conf={:.3} z={:.3} fields={} anomalous={:?}",
            subject,
            classification,
            confidence,
            z,
            field_results.len(),
            anomalous_fields
        ),
    );

    Ok(ClassifyResult {
        subject: subject.to_string(),
        classification: classification.to_string(),
        confidence,
        solutions,
        anomalous_fields,
        message_count: n,
        hitl_required: confidence < HITL_THRESHOLD,
        phase: 1,
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
}
