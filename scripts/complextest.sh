#!/usr/bin/env bash
# End-to-end pub/sub pipeline test: 3 clients, 3 projects, encrypted file transfer.
#
# Fixed topology:
#   A listens [p1 p2 p3], sends to p1 (file_a1), p2 (file_a2)
#   B listens [p2 p3],     sends to p2 (file_b1), p3 (file_b2)
#   C listens [p3],        sends to p3 (file_c1), p3 (file_c2)
# Expected receives: A = b1 b2 c1 c2, B = a2 c1 c2, C = b2.
#
# Stacks: rust (all hubdatatraffic), python (all pydatatraffic), mixed (A,B rust; C python).
# Requires REDIS_URL and CRYPTO_KEY in the environment (REDIS_PASSWORD optional).

set -euo pipefail

TIMEOUT=60

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SCRIPT_PATH="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"

usage() {
    echo "Usage: $0 --stack rust|python|mixed" >&2
    exit 1
}

ORIG_ARGS=("$@")

STACK=""
while [ $# -gt 0 ]; do
    case "$1" in
        --stack)
            if [ $# -lt 2 ]; then
                usage
            fi
            STACK="$2"
            shift 2
            ;;
        *)
            usage
            ;;
    esac
done
case "$STACK" in
    rust | python | mixed) ;;
    *) usage ;;
esac

# --- environment -------------------------------------------------------------
: "${REDIS_URL:?REDIS_URL is required, e.g. redis://localhost:6379}"
: "${CRYPTO_KEY:?CRYPTO_KEY is required (64 hex chars; generate with --genkey)}"
export REDIS_PASSWORD="${REDIS_PASSWORD:-}"

# Python interpreter: prefer the repo venv so `make complextest` works as-is.
PYTHON="python"
[ -x "$ROOT/venv/bin/python" ] && PYTHON="$ROOT/venv/bin/python"

needs_rust=0
needs_python=0
case "$STACK" in
    rust) needs_rust=1 ;;
    python) needs_python=1 ;;
    mixed) needs_rust=1; needs_python=1 ;;
esac

# Build outside the timed window: a cold cargo build may exceed TIMEOUT.
if [ -z "${_CT_UNDER_TIMEOUT:-}" ]; then
    if [ "$needs_rust" = 1 ]; then
        (cd "$ROOT" && cargo build -p hubdatatraffic)
    fi
    if [ "$needs_python" = 1 ] && [ ! -e "$ROOT/pydatatraffic/libdatatraffic.so.1.x86_64" ]; then
        echo "ERROR: pydatatraffic/libdatatraffic.so.1.x86_64 not found; run 'make build-lib' first." >&2
        exit 1
    fi

    # Re-exec under a wall-clock timeout for the orchestration phase.
    _CT_UNDER_TIMEOUT=1
    export _CT_UNDER_TIMEOUT
    exec timeout --signal=TERM "$TIMEOUT" bash "$SCRIPT_PATH" "${ORIG_ARGS[@]}"
fi

cd "$ROOT"

WORK="$ROOT/complextest"
rm -rf "$WORK"
mkdir -p "$WORK/tmp" "$WORK/logs" "$WORK/out_a" "$WORK/out_b" "$WORK/out_c"

# --- temp files ---------------------------------------------------------------
TS="$(date +%s)"
for f in file_a1 file_a2 file_b1 file_b2 file_c1 file_c2; do
    echo "$f $TS" >"$WORK/tmp/$f.txt"
done

RUST_BIN="$ROOT/target/debug/hubdatatraffic"
[ -x "$RUST_BIN" ] || {
    echo "ERROR: $RUST_BIN not found (cargo build failed?)" >&2
    exit 1
}

run_rust()   { "$RUST_BIN" "$@"; }
run_python() { "$PYTHON" -m pydatatraffic.main "$@"; }

case "$STACK" in
    rust) RUNNER_A=run_rust; RUNNER_B=run_rust; RUNNER_C=run_rust ;;
    python) RUNNER_A=run_python; RUNNER_B=run_python; RUNNER_C=run_python ;;
    mixed) RUNNER_A=run_rust; RUNNER_B=run_rust; RUNNER_C=run_python ;;
esac

# --- listeners -----------------------------------------------------------------
LISTENER_PIDS=()
kill_listeners() {
    local pid
    for pid in "${LISTENER_PIDS[@]:-}"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
    done
}
trap kill_listeners EXIT
trap 'echo "ERROR: workflow timed out after ${TIMEOUT}s" >&2; exit 124' TERM

# Background a plain command (not a function) so $! is the real client PID.
# Rust accepts several projects in one --listen occurrence, Python needs the flag repeated.
start_listener() {
    # $1=runner $2=client_name $3=out_dir rest=projects
    local runner="$1" client="$2" outdir="$3"
    shift 3
    if [ "$runner" = run_python ]; then
        local args=("$PYTHON" -m pydatatraffic.main) p
        for p in "$@"; do
            args+=(--listen "$p")
        done
        "${args[@]}" --client "$client" --out-dir "$outdir" &
    else
        "$RUST_BIN" --client "$client" --listen "$@" --out-dir "$outdir" &
    fi
}

start_listener "$RUNNER_A" client_a "$WORK/out_a" p1 p2 p3 >"$WORK/logs/listener_a.log" 2>&1
PID_A=$!
LISTENER_PIDS+=("$PID_A")
start_listener "$RUNNER_B" client_b "$WORK/out_b" p2 p3 >"$WORK/logs/listener_b.log" 2>&1
PID_B=$!
LISTENER_PIDS+=("$PID_B")
start_listener "$RUNNER_C" client_c "$WORK/out_c" p3 >"$WORK/logs/listener_c.log" 2>&1
PID_C=$!
LISTENER_PIDS+=("$PID_C")

sleep 2 # allow subscriptions to settle

check_alive() {
    local label="$1" pid="$2" log="$3"
    if ! kill -0 "$pid" 2>/dev/null; then
        echo "ERROR: listener $label died at startup:" >&2
        cat "$log" >&2
        exit 1
    fi
}
check_alive a "$PID_A" "$WORK/logs/listener_a.log"
check_alive b "$PID_B" "$WORK/logs/listener_b.log"
check_alive c "$PID_C" "$WORK/logs/listener_c.log"

# --- sends (sequential) ---------------------------------------------------------
SEND_TIMES="$WORK/tmp/send_times.tsv"   # file<TAB>start_ns<TAB>end_ns, one row per send
: >"$SEND_TIMES"

run_send() {
    # $1=runner $2=client_name $3=file $4=project $5=label
    local runner="$1" client="$2" file="$3" project="$4" label="$5"
    local log="$WORK/logs/send_$label.log"
    local start_ns end_ns
    start_ns=$(date +%s%N)
    if ! "$runner" --client "$client" \
        --send-file "$WORK/tmp/$file.txt" --project "$project" >"$log" 2>&1; then
        echo "ERROR: send $label ($file -> $project) failed:" >&2
        cat "$log" >&2
        exit 1
    fi
    end_ns=$(date +%s%N)
    printf '%s\t%s\t%s\n' "$file" "$start_ns" "$end_ns" >>"$SEND_TIMES"
}

SEND_START=$(date +%s%N)
run_send "$RUNNER_A" client_a file_a1 p1 a1
run_send "$RUNNER_A" client_a file_a2 p2 a2
run_send "$RUNNER_B" client_b file_b1 p2 b1
run_send "$RUNNER_B" client_b file_b2 p3 b2
run_send "$RUNNER_C" client_c file_c1 p3 c1
run_send "$RUNNER_C" client_c file_c2 p3 c2
SEND_END=$(date +%s%N)
echo "send phase: $(( (SEND_END - SEND_START) / 1000000 )) ms"

sleep 2 # allow in-flight messages to be delivered and saved

kill_listeners

# --- receive latency -------------------------------------------------------------
# Per received file: time from the moment its send was issued (sender process start) until it
# appears on a receiver. min/avg/max across all received copies of every logical file.
"$PYTHON" - "$WORK" <<'PY'
import os, sys

work = sys.argv[1]
send_start_ns = {}
tsv = os.path.join(work, "tmp", "send_times.tsv")
if os.path.exists(tsv):
    with open(tsv) as fh:
        for line in fh:
            parts = line.rstrip("\n").split("\t")
            if len(parts) >= 2 and parts[1]:
                send_start_ns[parts[0]] = int(parts[1])

deltas = []
for d in ("out_a", "out_b", "out_c"):
    pdir = os.path.join(work, d)
    if not os.path.isdir(pdir):
        continue
    for name in os.listdir(pdir):
        fp = os.path.join(pdir, name)
        if not os.path.isfile(fp):
            continue
        key = name[:-4] if name.endswith(".txt") else name
        if key in send_start_ns:
            delta_ms = (os.path.getmtime(fp) - send_start_ns[key] / 1e9) * 1000.0
            deltas.append(delta_ms)

if deltas:
    print(
        "recv latency (send-issued -> file on receiver): "
        "min=%.0f avg=%.0f max=%.0f ms (n=%d)"
        % (min(deltas), sum(deltas) / len(deltas), max(deltas), len(deltas))
    )
else:
    print("recv latency: no received files matched a send")
PY

# --- results ---------------------------------------------------------------------
echo "=== RESULTS ==="
for d in out_a out_b out_c; do
    echo "--- $d ---"
    ls -A "$WORK/$d" || true
done

FAILURES=0
check_client() {
    # $1=label $2=out_dir rest=expected file names
    local label="$1" dir="$2"
    shift 2
    local expected=("$@")
    local exp act f
    exp="$(printf '%s\n' "${expected[@]}" | sort)"
    act="$(ls -A "$dir" | sort)"
    if [ "$exp" != "$act" ]; then
        echo "FAIL $label: file set mismatch (expected: $(echo "$exp" | tr '\n' ' '), actual: $(echo "$act" | tr '\n' ' '))" >&2
        FAILURES=$((FAILURES + 1))
        return
    fi
    for f in "${expected[@]}"; do
        if ! cmp -s "$WORK/tmp/$f" "$dir/$f"; then
            echo "FAIL $label: content mismatch for $f" >&2
            FAILURES=$((FAILURES + 1))
            return
        fi
    done
    echo "PASS $label (${#expected[@]} files)"
}

check_client A "$WORK/out_a" file_b1.txt file_b2.txt file_c1.txt file_c2.txt
check_client B "$WORK/out_b" file_a2.txt file_c1.txt file_c2.txt
check_client C "$WORK/out_c" file_b2.txt

if [ "$FAILURES" -gt 0 ]; then
    echo "RESULT: FAIL ($FAILURES of 3 clients)" >&2
    exit 1
fi
echo "RESULT: PASS (stack=$STACK)"
