#!/bin/bash
# Local benchmark runner with perf profiling
# Uses perf build (frame pointers + debug symbols) for proper stack traces
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

# Configuration
PROCS=${1:-$(nproc)}
MIMALLOC_BENCH="${MIMALLOC_BENCH:-$PROJECT_ROOT/mimalloc-bench}"
INICTUS_LIB="${INICTUS_LIB:-$PROJECT_ROOT/target/perf/libinictus.so}"
PERF_OUTPUT="${PERF_OUTPUT:-$PROJECT_ROOT/perf-data-local}"

# Validate paths
if [ ! -d "$MIMALLOC_BENCH/out/bench" ]; then
    echo "ERROR: mimalloc-bench not found at $MIMALLOC_BENCH/out/bench"
    echo "Build it first: cd mimalloc-bench && ./build-bench-env.sh all"
    exit 1
fi

if [ ! -f "$INICTUS_LIB" ]; then
    echo "ERROR: libinictus.so not found at $INICTUS_LIB"
    echo "Build it first: make perf"
    exit 1
fi

# Verify debug symbols exist
echo "--- Verifying perf build ---"
if file "$INICTUS_LIB" | grep -q "not stripped"; then
    echo "✓ Debug symbols: present (not stripped)"
else
    echo "✗ WARNING: Binary appears stripped - stack traces may be incomplete"
    echo "  Rebuild with: make perf"
fi

# Check for debug info section
if readelf -S "$INICTUS_LIB" 2>/dev/null | grep -q "\.debug_info"; then
    echo "✓ DWARF debug info: present"
else
    echo "✗ WARNING: No DWARF debug info found"
fi

# Create output directory
mkdir -p "$PERF_OUTPUT"

cd "$MIMALLOC_BENCH/out/bench"
DATA="$MIMALLOC_BENCH/bench"

echo ""
echo "=== inictus PROFILING benchmark - $PROCS threads ==="
echo "Using: $INICTUS_LIB"
echo "Output: $PERF_OUTPUT"
echo ""

# ─────────────────────────────────────────────────────────────────────────────
# Benchmark definitions - COMMENT OUT ENTIRE LINES TO SKIP BENCHMARKS
# Format: "name|binary|args..."
# ─────────────────────────────────────────────────────────────────────────────
BENCHMARKS=(
    # Single-threaded
    "glibc-simple|glibc-simple"
    "cfrac|cfrac|17545186520507317056371138836327483792789528"
    "espresso|espresso|$DATA/espresso/largest.espresso"
    "barnes|barnes|<$DATA/barnes/input"
    
    # Multi-threaded
    "glibc-thread|glibc-thread|$PROCS"
    "larsonN|larson|5|8|1000|5000|100|4141|$PROCS"
    "larsonN-sized|larson-sized|5|8|1000|5000|100|4141|$PROCS"
    "mstressN|mstress|$PROCS|50|25"
    "rptestN|rptest|$PROCS|0|1|2|500|1000|100|8|16000"
    "xmalloc-testN|xmalloc-test|-w|$PROCS|-t|5|-s|64"
    "cache-scratch1|cache-scratch|1|1000|1|2000000|$PROCS"
    "alloc-test1|alloc-test|1"
    "sh6benchN|sh6bench|$PROCS"
    "sh8benchN|sh8bench|$PROCS"
    "malloc-large|malloc-large"
)

# ─────────────────────────────────────────────────────────────────────────────
# Run benchmarks with perf
# ─────────────────────────────────────────────────────────────────────────────

for bench in "${BENCHMARKS[@]}"; do
    IFS='|' read -ra parts <<< "$bench"
    name="${parts[0]}"
    binary="${parts[1]}"
    args="${parts[*]:2}"
    args="${args//|/ }"
    
    echo ">>> Profiling $name..."
    perf_file="$PERF_OUTPUT/${name}.perf.data"
    
    # Handle stdin redirection (e.g., barnes uses "< input")
    if [[ "$args" == *"<"* ]]; then
        input_file="${args#*<}"
        input_file="${input_file## }"  # trim leading space
        LD_PRELOAD="$INICTUS_LIB" perf record -g --call-graph dwarf -o "$perf_file" ./"$binary" < "$input_file" 2>&1 || true
    else
        LD_PRELOAD="$INICTUS_LIB" perf record -g --call-graph dwarf -o "$perf_file" ./"$binary" $args 2>&1 || true
    fi
    
    echo "    Saved: $perf_file"
    echo ""
done

echo "=== PROFILING COMPLETE ==="
echo ""
echo "To view results:"
echo "  perf report -i $PERF_OUTPUT/<name>.perf.data"
echo ""
echo "To generate flamegraph:"
echo "  perf script -i $PERF_OUTPUT/<name>.perf.data | stackcollapse-perf.pl | flamegraph.pl > flamegraph.svg"
echo ""
