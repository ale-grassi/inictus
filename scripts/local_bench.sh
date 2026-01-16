#!/bin/bash
# Local benchmark runner using LD_PRELOAD for inictus
# Requires: mimalloc-bench built locally, inictus built with c_api feature
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

# Configuration
PROCS=${1:-$(nproc)}
MIMALLOC_BENCH="${MIMALLOC_BENCH:-$PROJECT_ROOT/mimalloc-bench}"
INICTUS_LIB="${INICTUS_LIB:-$PROJECT_ROOT/target/release/libinictus.so}"

# Validate paths
if [ ! -d "$MIMALLOC_BENCH/out/bench" ]; then
    echo "ERROR: mimalloc-bench not found at $MIMALLOC_BENCH/out/bench"
    echo "Build it first: cd mimalloc-bench && ./build-bench-env.sh all"
    exit 1
fi

if [ ! -f "$INICTUS_LIB" ]; then
    echo "ERROR: libinictus.so not found at $INICTUS_LIB"
    echo "Build it first: cargo build --release --features c_api,dynamic"
    exit 1
fi

cd "$MIMALLOC_BENCH/out/bench"
DATA="$MIMALLOC_BENCH/bench"

echo ""
echo "=== inictus LOCAL benchmark - $PROCS threads ==="
echo "Using: $INICTUS_LIB"
echo ""

# Temp files for results
GLIBC_RESULTS=$(mktemp)
INICTUS_RESULTS=$(mktemp)
trap "rm -f $GLIBC_RESULTS $INICTUS_RESULTS" EXIT

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
# Run benchmarks
# ─────────────────────────────────────────────────────────────────────────────

run_bench() {
    local binary=$1
    local args=$2
    local use_inictus=$3
    
    local cmd
    if [ "$use_inictus" = "true" ]; then
        cmd="LD_PRELOAD=$INICTUS_LIB"
    else
        cmd=""
    fi
    
    if [[ "$args" == *"<"* ]]; then
        input_file="${args#*<}"
        input_file="${input_file## }"
        result=$( { env $cmd /usr/bin/time -f "%E %M" ./"$binary" < "$input_file" 2>&1; } 2>&1 | tail -1 )
    else
        result=$( { env $cmd /usr/bin/time -f "%E %M" ./"$binary" $args 2>&1; } 2>&1 | tail -1 )
    fi
    
    echo "$result"
}

echo "[GLIBC BASELINE]"
for bench in "${BENCHMARKS[@]}"; do
    IFS='|' read -ra parts <<< "$bench"
    name="${parts[0]}"
    binary="${parts[1]}"
    args="${parts[*]:2}"
    args="${args//|/ }"
    
    printf "  %-18s" "$name..."
    result=$(run_bench "$binary" "$args" "false")
    elapsed=$(echo "$result" | awk '{print $1}')
    rss=$(echo "$result" | awk '{print $2}')
    printf "%s (%skb)\n" "$elapsed" "$rss"
    echo "$name $elapsed $rss" >> "$GLIBC_RESULTS"
done

echo ""
echo "[INICTUS (LD_PRELOAD)]"
for bench in "${BENCHMARKS[@]}"; do
    IFS='|' read -ra parts <<< "$bench"
    name="${parts[0]}"
    binary="${parts[1]}"
    args="${parts[*]:2}"
    args="${args//|/ }"
    
    printf "  %-18s" "$name..."
    result=$(run_bench "$binary" "$args" "true")
    elapsed=$(echo "$result" | awk '{print $1}')
    rss=$(echo "$result" | awk '{print $2}')
    printf "%s (%skb)\n" "$elapsed" "$rss"
    echo "$name $elapsed $rss" >> "$INICTUS_RESULTS"
done

echo ""

# ─────────────────────────────────────────────────────────────────────────────
# Summary comparison table
# ─────────────────────────────────────────────────────────────────────────────
echo "=== SUMMARY COMPARISON ==="
echo ""

{
    echo "BENCHMARK|GLIBC_TIME|GLIBC_RSS|INICTUS_TIME|INICTUS_RSS|SPEEDUP"
    echo "---------|----------|---------|------------|-----------|-------"
    
    while read -r name g_time g_rss; do
        i_line=$(grep "^$name " "$INICTUS_RESULTS" 2>/dev/null || echo "$name - -")
        i_time=$(echo "$i_line" | awk '{print $2}')
        i_rss=$(echo "$i_line" | awk '{print $3}')
        
        g_rss_mb=$(echo "scale=1; $g_rss / 1024" | bc 2>/dev/null || echo "?")
        i_rss_mb=$(echo "scale=1; $i_rss / 1024" | bc 2>/dev/null || echo "?")
        
        g_secs=$(echo "$g_time" | awk -F: '{if (NF==2) print $1*60+$2; else print $1}' 2>/dev/null || echo "0")
        i_secs=$(echo "$i_time" | awk -F: '{if (NF==2) print $1*60+$2; else print $1}' 2>/dev/null || echo "0")
        
        if [ "$i_secs" != "0" ] && [ "$g_secs" != "0" ]; then
            speedup=$(echo "scale=2; $g_secs / $i_secs" | bc 2>/dev/null || echo "?")
            if [ "$(echo "$speedup > 1.02" | bc 2>/dev/null)" = "1" ]; then
                speedup_str="${speedup}x ✓"
            elif [ "$(echo "$speedup < 0.98" | bc 2>/dev/null)" = "1" ]; then
                speedup_str="${speedup}x ✗"
            else
                speedup_str="~1.00x"
            fi
        else
            speedup_str="-"
        fi
        
        echo "$name|$g_time|${g_rss_mb}MB|$i_time|${i_rss_mb}MB|$speedup_str"
    done < "$GLIBC_RESULTS"
} | column -t -s '|'

echo ""
echo "Legend: ✓ = inictus faster, ✗ = glibc faster, ~ = within 2%"
echo ""
