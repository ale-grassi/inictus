#!/bin/bash
# Docker benchmark runner with perf profiling
# Runs inside Docker container, outputs perf.data files
set -euo pipefail

export LANG=C.UTF-8
export LC_ALL=C.UTF-8

PROCS=${1:-$(nproc)}
PERF_OUTPUT="/perf-output"  # Mounted from host's perf-data-docker
cd /mimalloc-bench/out/bench
DATA=/mimalloc-bench/bench

echo ""
echo "=== inictus PROFILING benchmark - $PROCS threads ==="
echo "Output: $PERF_OUTPUT"
echo ""

# Verify debug symbols in inictus binaries
echo "--- Verifying perf build ---"
sample_binary="./inictus/larson"
if [ -f "$sample_binary" ]; then
    if file "$sample_binary" | grep -q "not stripped"; then
        echo "✓ Debug symbols: present (not stripped)"
    else
        echo "✗ WARNING: Binary appears stripped - stack traces may be incomplete"
    fi
    
    if readelf -S "$sample_binary" 2>/dev/null | grep -q "\.debug_info"; then
        echo "✓ DWARF debug info: present"
    else
        echo "✗ WARNING: No DWARF debug info found"
    fi
fi
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

mkdir -p "$PERF_OUTPUT"

# ─────────────────────────────────────────────────────────────────────────────
# Run inictus benchmarks with perf
# ─────────────────────────────────────────────────────────────────────────────
echo "[INICTUS (static) + perf]"

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
        perf record -g --call-graph dwarf -o "$perf_file" ./inictus/"$binary" < "$input_file" 2>&1 || true
    else
        perf record -g --call-graph dwarf -o "$perf_file" ./inictus/"$binary" $args 2>&1 || true
    fi
    
    # Make readable
    chmod 644 "$perf_file"
    
    echo "    Saved: $perf_file"
    echo ""
done

echo ""
echo "=== COPYING BINARIES FOR LOCAL ANALYSIS ==="
# Copy binaries to mounted /binaries-output (-> target/perf/docker/)
BINARIES_DIR="/binaries-output"
if [ -d "$BINARIES_DIR" ]; then
    for bench in "${BENCHMARKS[@]}"; do
        IFS='|' read -ra parts <<< "$bench"
        binary="${parts[1]}"
        cp "./inictus/$binary" "$BINARIES_DIR/" 2>/dev/null || true
    done
    echo "Binaries copied to: $BINARIES_DIR/"
else
    echo "WARNING: /binaries-output not mounted, skipping binary copy"
fi

echo ""
echo "=== PROFILING COMPLETE ==="
echo "Files in $PERF_OUTPUT:"
ls -la "$PERF_OUTPUT"
echo ""
echo "To analyze on host:"
echo "  perf report -i perf-data-docker/larsonN.perf.data"
echo ""
