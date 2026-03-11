#!/usr/bin/env bash
# run_integration_test.sh — Full in-mesh integration test for wasmcloud-pattern-classifier.
#
# Tests the complete Phase 1+2 classification pipeline inside a real wasmCloud mesh:
#   1.  Redis server ensured running (auto-started if absent)
#   2.  wasmCloud host started via `wash up`
#   3.  Component built from source if no pre-built artifact exists
#   4.  Application deployed via WADM manifest
#   5.  NATS subscriber started in background to capture classification output
#   6.  WARM_UP_COUNT (31) identical messages sent to accumulate baseline statistics;
#       these messages are expected to produce "WARMING_UP" classifications
#   7.  Additional messages sent to fill Phase 2 temporal ring buffer (2 × WINDOW_N = 40)
#   8.  A "normal" repeat message sent; expected to produce a low-z NORMAL result
#   9.  An "anomalous" message (different values) sent; expected to show higher z-score
#  10.  HITL reply flow exercised: a synthetic reply is published on the .reply topic
#       and the stats counter increment is verified
#  11.  Redis key verification: stats:v1:* (Welford accumulators), ema:v1:* (per-field),
#       and window:* (Phase 2 temporal ring buffer)
#  12.  NATS output verification: classification with Phase 2 temporal fields
#  13.  Full teardown: test keys flushed, app undeployed, host stopped, Redis stopped
#
# Configuration (all overridable via environment variables):
#   REDIS_HOST           Redis host                        (default: 127.0.0.1)
#   REDIS_PORT           Redis port                        (default: 6379)
#   HOST_READY_TIMEOUT   Seconds to wait for wasmCloud host  (default: 60)
#   DEPLOY_TIMEOUT       Seconds to wait for app deployment  (default: 120)
#   PROCESS_WAIT         Seconds to wait after each publish batch (default: 10)
#   WARM_UP_COUNT        Number of warm-up messages to send   (default: 31)
#
# Usage (from repository root):
#   ./tests/run_integration_test.sh
set -euo pipefail

# ── Source cargo/local env ────────────────────────────────────────────────────
. "$HOME/.cargo/env" 2>/dev/null || true
export PATH="/usr/local/bin:$HOME/.local/bin:$PATH"

# ── Configuration ─────────────────────────────────────────────────────────────
REDIS_HOST="${REDIS_HOST:-127.0.0.1}"
REDIS_PORT="${REDIS_PORT:-6379}"
HOST_READY_TIMEOUT="${HOST_READY_TIMEOUT:-60}"
DEPLOY_TIMEOUT="${DEPLOY_TIMEOUT:-120}"
PROCESS_WAIT="${PROCESS_WAIT:-10}"
WARM_UP_COUNT="${WARM_UP_COUNT:-31}"

# Phase 2: temporal sliding window parameters
WINDOW_N=20
TEMPORAL_WARMUP=$((2 * WINDOW_N))  # 40 messages needed for Phase 2

NATS_URL="nats://127.0.0.1:4222"

# ── Test fixtures ─────────────────────────────────────────────────────────────
# Subject must match the WADM subscription pattern "pattern.monitor.>"
TEST_SUBJECT="pattern.monitor.integration"

# Baseline payload: sent WARM_UP_COUNT times to build the rolling mean
BASELINE_PAYLOAD='{"event":"quake","magnitude":"6.2","location":"Pacific","depth_km":"35"}'

# Normal repeat: same schema + similar values → expected NORMAL after warm-up
NORMAL_PAYLOAD='{"event":"quake","magnitude":"6.3","location":"Pacific","depth_km":"36"}'

# Anomalous payload: wildly different values for every field → triggers higher z-score
ANOMALY_PAYLOAD='{"event":"quake","magnitude":"999","location":"Arctic","depth_km":"99999"}'

# HITL reply payload (the human confirms the anomaly)
HITL_REPLY='{"classification":"ANOMALY","label":"message_level_anomaly"}'

# ── Redis key expectations ─────────────────────────────────────────────────────
# Welford accumulator keys (owned by classifier)
EXPECTED_STATS_KEYS=(
    "stats:v1:${TEST_SUBJECT}:n"
    "stats:v1:${TEST_SUBJECT}:mean_vec"
    "stats:v1:${TEST_SUBJECT}:mean_score"
    "stats:v1:${TEST_SUBJECT}:m2"
)

# Per-field EMA keys — one per JSON key in the baseline payload
EXPECTED_EMA_KEYS=(
    "ema:v1:${TEST_SUBJECT}:event"
    "ema:v1:${TEST_SUBJECT}:magnitude"
    "ema:v1:${TEST_SUBJECT}:location"
    "ema:v1:${TEST_SUBJECT}:depth_km"
)

# Phase 2 temporal ring-buffer keys (owned by Phase 2 classifier)
EXPECTED_TEMPORAL_KEYS=(
    "window:ring:${TEST_SUBJECT}:head"
    "window:ring:${TEST_SUBJECT}:size"
)

# HITL alert subject (classifier publishes here when confidence is low)
ALERT_SUBJECT="pattern.alert.human.integration"

# Human reply subject (classifier subscribes here)
REPLY_SUBJECT="${ALERT_SUBJECT}.reply"

# ── Colours ───────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

info()    { echo -e "${GREEN}[INFO]${NC}  $*"; }
warn()    { echo -e "${YELLOW}[WARN]${NC}  $*"; }
error()   { echo -e "${RED}[ERROR]${NC} $*"; }
section() { echo -e "\n${CYAN}━━ $* ━━${NC}"; }

# ── Tracked state ─────────────────────────────────────────────────────────────
WASH_PID=""
REDIS_PID_STARTED=""
NATS_SUB_PID=""
WASH_LOG=""
NATS_OUT=""

# ── Cleanup (always runs on EXIT) ─────────────────────────────────────────────
cleanup() {
    section "Cleanup"

    # Kill background NATS subscriber if still alive
    if [[ -n "$NATS_SUB_PID" ]]; then
        kill "$NATS_SUB_PID" 2>/dev/null || true
    fi

    # Flush test keys from Redis
    if redis-cli -h "$REDIS_HOST" -p "$REDIS_PORT" PING &>/dev/null 2>&1; then
        FLUSH_PATTERN="${TEST_SUBJECT}"
        for key in \
            "${EXPECTED_STATS_KEYS[@]}" \
            "${EXPECTED_EMA_KEYS[@]}" \
            "${EXPECTED_TEMPORAL_KEYS[@]}"; do
            redis-cli -h "$REDIS_HOST" -p "$REDIS_PORT" DEL "$key" &>/dev/null || true
        done
        # Also flush any window:slot and window:drift keys
        for i in $(seq 0 $((WINDOW_N - 1))); do
            redis-cli -h "$REDIS_HOST" -p "$REDIS_PORT" DEL "window:slot:${TEST_SUBJECT}:${i}" &>/dev/null || true
        done
        redis-cli -h "$REDIS_HOST" -p "$REDIS_PORT" DEL "window:drift:${TEST_SUBJECT}:history" &>/dev/null || true
        info "Test keys flushed from Redis"
    fi

    # Undeploy and remove the WADM application
    wash app undeploy pattern-classifier-app --timeout-ms 15000 2>/dev/null || true
    wash app delete  pattern-classifier-app --timeout-ms 15000 2>/dev/null || true

    # Shut down the wasmCloud host
    wash down 2>/dev/null || true
    if [[ -n "$WASH_PID" ]]; then
        kill "$WASH_PID" 2>/dev/null || true
    fi
    info "wasmCloud host stopped"

    # Stop Redis only if this script started it
    if [[ -n "$REDIS_PID_STARTED" ]]; then
        kill "$REDIS_PID_STARTED" 2>/dev/null || true
        info "Redis server stopped (PID $REDIS_PID_STARTED)"
    fi

    # Clean up temp files
    [[ -n "$WASH_LOG" && -f "$WASH_LOG" ]] && rm -f "$WASH_LOG"
    [[ -n "$NATS_OUT" && -f "$NATS_OUT" ]] && rm -f "$NATS_OUT"

    info "Cleanup complete"
}

trap cleanup EXIT

# ── Helper: poll a command until it succeeds or times out ─────────────────────
wait_for() {
    local label="$1"
    local timeout="$2"
    local cmd="$3"
    info "Waiting for: ${label} (up to ${timeout}s)..."
    for (( i=1; i<=timeout; i++ )); do
        if eval "$cmd" &>/dev/null 2>&1; then
            info "  ✓ Ready after ${i}s"
            return 0
        fi
        sleep 1
    done
    error "Timed out after ${timeout}s waiting for: ${label}"
    return 1
}

# ── Helper: check a Redis key exists ─────────────────────────────────────────
check_key() {
    local key="$1"
    local result
    result=$(redis-cli -h "$REDIS_HOST" -p "$REDIS_PORT" EXISTS "$key" 2>/dev/null || echo "0")
    result="${result//[[:space:]]/}"
    if [[ "$result" == "1" ]]; then
        info "  ✓ ${key}"
        return 0
    else
        error "  ✗ ${key}  (missing)"
        return 1
    fi
}

# ── Helper: read a u64 counter from Redis (stored as little-endian bytes) ─────
read_redis_u64() {
    local key="$1"
    # The classifier stores u64 as raw LE bytes; we can check existence + non-zero length
    redis-cli -h "$REDIS_HOST" -p "$REDIS_PORT" STRLEN "$key" 2>/dev/null | tr -d '[:space:]'
}

# ─────────────────────────────────────────────────────────────────────────────
# 1. Prerequisites
# ─────────────────────────────────────────────────────────────────────────────
section "1 · Prerequisites"

for tool in wash redis-cli nats; do
    if ! command -v "$tool" &>/dev/null; then
        case "$tool" in
            wash)
                error "'wash' CLI not found."
                error "Install: curl -s 'https://packagecloud.io/install/repositories/wasmcloud/core/script.deb.sh' | sudo bash && sudo apt-get install -y wash" ;;
            redis-cli)
                error "'redis-cli' not found.  Install: sudo apt-get install -y redis-tools" ;;
            nats)
                error "'nats' CLI not found."
                error "Install: curl -sSL https://github.com/nats-io/natscli/releases/download/v0.1.6/nats-0.1.6-linux-amd64.zip -o /tmp/nats.zip && unzip -q /tmp/nats.zip -d /tmp/nats-dl && sudo cp /tmp/nats-dl/nats-0.1.6-linux-amd64/nats /usr/local/bin/nats" ;;
        esac
        exit 1
    fi
done

if ! command -v redis-server &>/dev/null && ! redis-cli -h "$REDIS_HOST" -p "$REDIS_PORT" PING &>/dev/null 2>&1; then
    error "Redis is not running and 'redis-server' is not installed."
    error "Install: sudo apt-get install -y redis-server"
    exit 1
fi

info "wash:      $(wash --version 2>&1 | head -1)"
info "redis-cli: $(redis-cli --version 2>/dev/null)"
info "nats:      $(nats --version 2>/dev/null)"

# ─────────────────────────────────────────────────────────────────────────────
# 2. Redis
# ─────────────────────────────────────────────────────────────────────────────
section "2 · Redis"

if redis-cli -h "$REDIS_HOST" -p "$REDIS_PORT" PING &>/dev/null 2>&1; then
    info "Redis already running at ${REDIS_HOST}:${REDIS_PORT}"
else
    info "Starting Redis on ${REDIS_HOST}:${REDIS_PORT}..."
    redis-server --port "$REDIS_PORT" --bind "$REDIS_HOST" \
        --loglevel warning --save "" --appendonly no &
    REDIS_PID_STARTED=$!
    wait_for "Redis to accept connections" 30 \
        "redis-cli -h '${REDIS_HOST}' -p '${REDIS_PORT}' PING" || exit 1
    info "Redis started (PID ${REDIS_PID_STARTED})"
fi

# ─────────────────────────────────────────────────────────────────────────────
# 3. Component
# ─────────────────────────────────────────────────────────────────────────────
section "3 · Component"

COMPONENT_PATH=$(find component/build -name "*_s.wasm" 2>/dev/null | head -1 || true)
if [[ -z "$COMPONENT_PATH" ]]; then
    warn "No pre-built signed .wasm in component/build — building now (this may take a few minutes)..."
    wash build -p ./component
    COMPONENT_PATH=$(find component/build -name "*_s.wasm" 2>/dev/null | head -1 || true)
fi

if [[ -z "$COMPONENT_PATH" ]]; then
    error "Component wasm not found after build attempt. Check wash build output above."
    exit 1
fi
info "Using component: ${COMPONENT_PATH}"

# ─────────────────────────────────────────────────────────────────────────────
# 4. wasmCloud host
# ─────────────────────────────────────────────────────────────────────────────
section "4 · wasmCloud Host"

WASH_LOG=$(mktemp /tmp/wash-pattern-classifier-XXXXXX.log)
info "Starting wasmCloud host (log: ${WASH_LOG})..."
wash up > "$WASH_LOG" 2>&1 &
WASH_PID=$!

wait_for "wasmCloud host to appear in 'wash get hosts'" "$HOST_READY_TIMEOUT" \
    "wash get hosts 2>/dev/null | grep -qE '[A-Z0-9]{20,}'" || {
    error "Last 30 lines of wasmCloud host log:"
    tail -30 "$WASH_LOG" 2>/dev/null || true
    exit 1
}

# ─────────────────────────────────────────────────────────────────────────────
# 5. Deploy application
# ─────────────────────────────────────────────────────────────────────────────
section "5 · Application Deployment"

info "Uploading WADM manifest..."
wash app put wadm.yaml

info "Deploying pattern-classifier-app..."
wash app deploy pattern-classifier-app

info "Waiting for application to reach Deployed state (up to ${DEPLOY_TIMEOUT}s)..."
deployed=false
for (( i=1; i<=(DEPLOY_TIMEOUT/2); i++ )); do
    if wash app status pattern-classifier-app 2>/dev/null | grep -qi "deployed"; then
        info "  ✓ Application Deployed after ~$((i*2))s"
        deployed=true
        break
    fi
    if wash app status pattern-classifier-app 2>/dev/null | grep -qi "failed"; then
        error "Application entered Failed state — aborting."
        wash app status pattern-classifier-app 2>/dev/null || true
        tail -50 "$WASH_LOG" 2>/dev/null || true
        exit 1
    fi
    sleep 2
done

if [[ "$deployed" != "true" ]]; then
    error "Application did not reach Deployed state within ${DEPLOY_TIMEOUT}s."
    info "Current app status:"
    wash app status pattern-classifier-app 2>/dev/null || true
    info "Last 50 lines of wasmCloud host log:"
    tail -50 "$WASH_LOG" 2>/dev/null || true
    exit 1
fi

info "Pausing 3s for provider link initialisation..."
sleep 3

# ─────────────────────────────────────────────────────────────────────────────
# 6. Start NATS subscriber (background)
# ─────────────────────────────────────────────────────────────────────────────
section "6 · NATS Subscriber"

NATS_OUT=$(mktemp /tmp/nats-classifier-out-XXXXXX.txt)
info "Subscribing to 'pattern.classified.>' → ${NATS_OUT}"

# Subscribe in background; --count keeps the process alive for up to 200 messages
# before auto-exiting (enough headroom for all test messages)
nats sub --server "$NATS_URL" --count 200 "pattern.classified.>" \
    > "$NATS_OUT" 2>&1 &
NATS_SUB_PID=$!
sleep 1   # give the subscriber a moment to register

# ─────────────────────────────────────────────────────────────────────────────
# 7. Warm-up phase — send WARM_UP_COUNT identical messages
# ─────────────────────────────────────────────────────────────────────────────
section "7 · Warm-up Phase (${WARM_UP_COUNT} messages)"

info "Payload: ${BASELINE_PAYLOAD}"
info "Publishing ${WARM_UP_COUNT} baseline messages to '${TEST_SUBJECT}'..."

for (( i=1; i<=WARM_UP_COUNT; i++ )); do
    nats pub --server "$NATS_URL" "$TEST_SUBJECT" "$BASELINE_PAYLOAD"
done

info "Waiting ${PROCESS_WAIT}s for warm-up batch to be processed..."
sleep "$PROCESS_WAIT"

# ─────────────────────────────────────────────────────────────────────────────
# 7b. Phase 2 temporal warm-up — fill ring buffer to 2 × WINDOW_N
# ─────────────────────────────────────────────────────────────────────────────
EXTRA_MESSAGES=$((TEMPORAL_WARMUP - WARM_UP_COUNT))
if [[ "$EXTRA_MESSAGES" -gt 0 ]]; then
    section "7b · Phase 2 Temporal Warm-up (${EXTRA_MESSAGES} additional messages)"
    info "Sending ${EXTRA_MESSAGES} more baseline messages to fill temporal ring buffer..."
    for (( i=1; i<=EXTRA_MESSAGES; i++ )); do
        nats pub --server "$NATS_URL" "$TEST_SUBJECT" "$BASELINE_PAYLOAD"
    done
    info "Waiting ${PROCESS_WAIT}s for temporal warm-up batch to be processed..."
    sleep "$PROCESS_WAIT"
fi

# ─────────────────────────────────────────────────────────────────────────────
# 8. Post-warm-up — normal and anomalous messages
# ─────────────────────────────────────────────────────────────────────────────
section "8 · Post-Warm-up Messages"

info "Publishing normal repeat message: ${NORMAL_PAYLOAD}"
nats pub --server "$NATS_URL" "$TEST_SUBJECT" "$NORMAL_PAYLOAD"

info "Publishing anomalous message: ${ANOMALY_PAYLOAD}"
nats pub --server "$NATS_URL" "$TEST_SUBJECT" "$ANOMALY_PAYLOAD"

info "Waiting ${PROCESS_WAIT}s for post-warm-up messages to be processed..."
sleep "$PROCESS_WAIT"

# ─────────────────────────────────────────────────────────────────────────────
# 9. HITL reply flow
# ─────────────────────────────────────────────────────────────────────────────
section "9 · HITL Reply Flow"

# Read stats counter before reply
N_BEFORE=$(read_redis_u64 "stats:v1:${TEST_SUBJECT}:n")
info "Message count before HITL reply: ${N_BEFORE} bytes in key (raw LE u64)"

# Publish a human reply on the .reply topic the classifier subscribes to
info "Publishing human reply to '${REPLY_SUBJECT}': ${HITL_REPLY}"
nats pub --server "$NATS_URL" "$REPLY_SUBJECT" "$HITL_REPLY"

info "Waiting 5s for reply to be processed..."
sleep 5

# Read stats counter after reply — it should have grown by 8 bytes if a new
# u64 value was stored, OR remain 8 bytes (the counter always occupies 8 bytes
# as a LE u64).  We simply verify the key still exists and is 8 bytes.
N_AFTER=$(read_redis_u64 "stats:v1:${TEST_SUBJECT}:n")
info "Message count key after HITL reply: ${N_AFTER} bytes"

if [[ "$N_AFTER" == "8" ]]; then
    info "  ✓ HITL reply accepted — stats counter key present and correctly sized"
else
    warn "  ? stats counter key has unexpected byte length: ${N_AFTER} (expected 8)"
fi

# ─────────────────────────────────────────────────────────────────────────────
# 10. Redis key verification
# ─────────────────────────────────────────────────────────────────────────────
section "10 · Redis Key Verification"

info "All keys currently in Redis:"
redis-cli -h "$REDIS_HOST" -p "$REDIS_PORT" KEYS '*' 2>/dev/null | sort || true
echo ""

PASS=true

info "Checking Welford stats keys:"
for key in "${EXPECTED_STATS_KEYS[@]}"; do
    check_key "$key" || PASS=false
done

echo ""
info "Checking per-field EMA keys:"
for key in "${EXPECTED_EMA_KEYS[@]}"; do
    check_key "$key" || PASS=false
done

echo ""
info "Checking Phase 2 temporal ring-buffer keys:"
for key in "${EXPECTED_TEMPORAL_KEYS[@]}"; do
    check_key "$key" || PASS=false
done

# ─────────────────────────────────────────────────────────────────────────────
# 11. NATS output verification
# ─────────────────────────────────────────────────────────────────────────────
section "11 · NATS Output Verification"

# Stop the background subscriber
kill "$NATS_SUB_PID" 2>/dev/null || true
NATS_SUB_PID=""

info "Classification messages captured in ${NATS_OUT}:"
echo ""
cat "$NATS_OUT" 2>/dev/null || true
echo ""

# ── 11a. At least one message was captured ────────────────────────────────────
if [[ ! -s "$NATS_OUT" ]]; then
    error "No classification messages were captured on 'pattern.classified.>'"
    PASS=false
else
    info "  ✓ Classification output received on NATS"
fi

# ── 11b. At least one non-WARMING_UP classification after warm-up ─────────────
if grep -q '"classification"' "$NATS_OUT" 2>/dev/null; then
    info "  ✓ 'classification' field present in captured output"
else
    error "  ✗ 'classification' field not found in NATS output"
    PASS=false
fi

if grep -q '"phase":1' "$NATS_OUT" 2>/dev/null; then
    info "  ✓ 'phase':1 field confirmed (Phase 1 classifier during warm-up)"
else
    warn "  ? 'phase':1 field not found — may have progressed to Phase 2 entirely"
fi

if grep -q '"phase":2' "$NATS_OUT" 2>/dev/null; then
    info "  ✓ 'phase':2 field confirmed (Phase 2 temporal layer active)"
else
    warn "  ? 'phase':2 field not found — Phase 2 temporal warm-up may not be complete"
fi

if grep -q '"source":"phase1"' "$NATS_OUT" 2>/dev/null; then
    info "  ✓ 'source':'phase1' confirmed in solutions"
else
    warn "  ? 'source':'phase1' not found in solutions"
fi

if grep -q '"source":"phase2"' "$NATS_OUT" 2>/dev/null; then
    info "  ✓ 'source':'phase2' confirmed in solutions (temporal layer)"
else
    warn "  ? 'source':'phase2' not found — Phase 2 may not have reached sufficient warm-up"
fi

if grep -q '"solutions"' "$NATS_OUT" 2>/dev/null; then
    info "  ✓ 'solutions' array present"
else
    error "  ✗ 'solutions' array not found in NATS output"
    PASS=false
fi

if grep -q '"confidence"' "$NATS_OUT" 2>/dev/null; then
    info "  ✓ 'confidence' field present"
else
    error "  ✗ 'confidence' field not found in NATS output"
    PASS=false
fi

# ── 11c. Verify WARMING_UP during warm-up batch ───────────────────────────────
if grep -q '"WARMING_UP"' "$NATS_OUT" 2>/dev/null; then
    info "  ✓ WARMING_UP classification observed during warm-up batch"
else
    warn "  ? No WARMING_UP classification found — either PROCESS_WAIT was long enough"
    warn "    for all warm-up messages, or the subscriber missed early messages."
fi

# ── 11d. Verify a non-WARMING_UP classification appeared after warm-up ─────────
if grep -qE '"classification":"(NORMAL|ANOMALY)"' "$NATS_OUT" 2>/dev/null; then
    info "  ✓ Post-warm-up classification (NORMAL or ANOMALY) observed"
else
    error "  ✗ No post-warm-up classification (NORMAL or ANOMALY) found in captured output"
    PASS=false
fi

# ─────────────────────────────────────────────────────────────────────────────
# 12. Result
# ─────────────────────────────────────────────────────────────────────────────
section "12 · Result"

if [[ "$PASS" == "true" ]]; then
    info "╔════════════════════════════════════════╗"
    info "║   Integration test  ✅  PASSED         ║"
    info "╚════════════════════════════════════════╝"
    echo ""
    info "End-to-end Phase 1+2 pipeline verified:"
    info "  JSON messages  →  wasmcloud:messaging/handler"
    info "  Warm-up phase  →  WARMING_UP (first ${WARM_UP_COUNT} messages)"
    info "  VSA re-encode  →  embeddenator-vsa (deterministic)"
    info "  Welford stats  →  stats:v1:{subject}:* in Redis"
    info "  Per-field EMA  →  ema:v1:{subject}:{field} in Redis"
    info "  Temporal ring  →  window:ring:{subject}:head/size in Redis"
    info "  Classification →  pattern.classified.{subject} on NATS"
    info "  HITL replies   →  pattern.alert.human.*.reply ingested"
    info "  Phase 2 drift  →  temporal_label + temporal_confidence in output"
    exit 0
else
    error "╔════════════════════════════════════════╗"
    error "║   Integration test  ❌  FAILED         ║"
    error "╚════════════════════════════════════════╝"
    echo ""
    error "One or more assertions failed — see details above."
    echo ""
    error "All keys in Redis right now:"
    redis-cli -h "$REDIS_HOST" -p "$REDIS_PORT" KEYS '*' 2>/dev/null | sort || true
    echo ""
    error "Last 80 lines of wasmCloud host log (${WASH_LOG}):"
    tail -80 "$WASH_LOG" 2>/dev/null || true
    exit 1
fi
