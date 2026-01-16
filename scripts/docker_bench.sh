#!/bin/bash
# Runs inside Docker container built by Dockerfile.mimalloc-bench
set -euo pipefail

# Ensure UTF-8 output
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

PROCS=${1:-$(nproc)}
cd /mimalloc-bench/out/bench

echo ""
echo "=== inictus benchmark comparison - $PROCS threads ==="
echo ""

# Temp files for results
GLIBC_RESULTS=$(mktemp)
INICTUS_RESULTS=$(mktemp)
trap "rm -f $GLIBC_RESULTS $INICTUS_RESULTS" EXIT

DATA=/mimalloc-bench/bench
GLIBC_CACHE="/glibc_baseline.cache"

# ─────────────────────────────────────────────────────────────────────────────
# Benchmark definitions - COMMENT OUT ENTIRE LINES TO SKIP BENCHMARKS
# Format: "name|command|args..."
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
# Fingerprint verification - using nm symbol types
#
# Symbol types:
#   T = Symbol is DEFINED in text (code) section - malloc is INSIDE the binary
#   U = Symbol is UNDEFINED - malloc comes from external library (libc.so)
# ─────────────────────────────────────────────────────────────────────────────
verify_inictus() {
    local binary=$1
    
    local alloc_symbols="malloc calloc realloc free _Znwm _Znam _ZdlPv _ZdaPv"
    local malloc_type=""
    
    for sym in $alloc_symbols; do
        malloc_type=$(nm -D -P "$binary" 2>/dev/null | grep -E "^${sym}( |@)" | awk '{print $2}' | head -1 || true)
        if [ -n "$malloc_type" ]; then
            break
        fi
        malloc_type=$(nm -P "$binary" 2>/dev/null | grep -E "^${sym}( |@)" | awk '{print $2}' | head -1 || true)
        if [ -n "$malloc_type" ]; then
            break
        fi
    done
    
    if [ "$malloc_type" = "T" ] || [ "$malloc_type" = "t" ]; then
        return 0
    else
        return 1
    fi
}

# ─────────────────────────────────────────────────────────────────────────────
# Check for cached glibc baseline
# ─────────────────────────────────────────────────────────────────────────────
if [ -f "$GLIBC_CACHE" ]; then
    echo "[GLIBC BASELINE] Using cached results (verified at build time)"
    echo ""
    cp "$GLIBC_CACHE" "$GLIBC_RESULTS"
    
    # Only verify inictus binaries when cache exists
    echo "--- VERIFYING INICTUS BINARIES ---"
    has_errors=0
    
    for bench in "${BENCHMARKS[@]}"; do
        if [[ "$bench" =~ ^# ]] || [[ -z "$bench" ]]; then
            continue
        fi
        
        IFS='|' read -r name binary args <<< "$bench"
        
        printf "  %-18s" "$name..."
        if verify_inictus "./inictus/$binary"; then
            echo "✓ INICTUS (malloc='T')"
        else
            echo "✗ FAIL"
            has_errors=1
        fi
    done
    
    if [ "$has_errors" -eq 1 ]; then
        echo ""
        echo "ERROR: One or more inictus binaries failed verification!"
        exit 1
    fi
    
    echo ""
    echo "All inictus binaries verified. Starting benchmarks..."
else
    echo "ERROR: Glibc cache not found at $GLIBC_CACHE"
    echo "This Docker image may have been built incorrectly."
    exit 1
fi

echo ""
echo "--- RUNNING BENCHMARKS ---"

# ─────────────────────────────────────────────────────────────────────────────
# Run inictus
# ─────────────────────────────────────────────────────────────────────────────
echo ""
echo "[INICTUS (static)]"

for bench in "${BENCHMARKS[@]}"; do
    IFS='|' read -ra parts <<< "$bench"
    name="${parts[0]}"
    binary="${parts[1]}"
    args="${parts[*]:2}"
    args="${args//|/ }"
    
    printf "  %-18s" "$name..."
    
    if [[ "$args" == *"<"* ]]; then
        input_file="${args#*<}"
        input_file="${input_file## }"
        result=$( { /usr/bin/time -f "%E %M" ./inictus/"$binary" < "$input_file" 2>&1; } 2>&1 | tail -1 )
    else
        result=$( { /usr/bin/time -f "%E %M" ./inictus/"$binary" $args 2>&1; } 2>&1 | tail -1 )
    fi
    
    elapsed=$(echo "$result" | awk '{print $1}')
    rss=$(echo "$result" | awk '{print $2}')
    printf "%s (%skb)\n" "$elapsed" "$rss"
    echo "$name $elapsed $rss" >> "$INICTUS_RESULTS"
done

echo ""

# ─────────────────────────────────────────────────────────────────────────────
# Summary comparison table
# ─────────────────────────────────────────────────────────────────────────────
echo ""
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
echo "Note: GLIBC times are cached averages from build (3 runs)"
echo ""
