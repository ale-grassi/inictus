#!/bin/bash
# Runs glibc baseline benchmarks during Docker build and caches results
# Runs each benchmark multiple times and averages the results
set -euo pipefail

export LANG=C.UTF-8
export LC_ALL=C.UTF-8

# Thread count: use BENCH_PROCS if set (from Docker build-arg), else nproc
PROCS=${BENCH_PROCS:-$(nproc)}
RUNS=${GLIBC_BENCH_RUNS:-3}  # Configurable: number of runs to average
cd /mimalloc-bench/out/bench

DATA=/mimalloc-bench/bench

# Benchmark definitions (must match docker_bench.sh)
BENCHMARKS=(
    "glibc-simple|glibc-simple"
    "cfrac|cfrac|17545186520507317056371138836327483792789528"
    "espresso|espresso|$DATA/espresso/largest.espresso"
    "barnes|barnes|<$DATA/barnes/input"
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

CACHE_FILE="/glibc_baseline_${PROCS}t.cache"

# ─────────────────────────────────────────────────────────────────────────────
# Verify all glibc binaries use external malloc (symbol type 'U')
# ─────────────────────────────────────────────────────────────────────────────
echo ""
echo "=== Verifying GLIBC binaries ($PROCS threads) ==="
echo ""

for bench in "${BENCHMARKS[@]}"; do
    IFS='|' read -ra parts <<< "$bench"
    name="${parts[0]}"
    binary="${parts[1]}"
    
    # Check if malloc is undefined (U = external/glibc)
    alloc_symbols="malloc calloc realloc free _Znwm _Znam _ZdlPv _ZdaPv"
    malloc_type=""
    
    for sym in $alloc_symbols; do
        malloc_type=$(nm -D -P "./$binary" 2>/dev/null | grep -E "^${sym}( |@)" | awk '{print $2}' | head -1 || true)
        if [ -n "$malloc_type" ]; then
            break
        fi
        malloc_type=$(nm -P "./$binary" 2>/dev/null | grep -E "^${sym}( |@)" | awk '{print $2}' | head -1 || true)
        if [ -n "$malloc_type" ]; then
            break
        fi
    done
    
    if [ "$malloc_type" = "U" ]; then
        printf "  %-18s ✓ GLIBC (malloc='U')\n" "$name"
    else
        printf "  %-18s ✗ FAIL (malloc='%s', expected 'U')\n" "$name" "$malloc_type"
        echo "ERROR: Binary $binary is not using glibc allocator!"
        exit 1
    fi
done

echo ""
echo "All binaries verified as glibc-linked."

# ─────────────────────────────────────────────────────────────────────────────
# Run benchmarks and average results
# ─────────────────────────────────────────────────────────────────────────────
echo ""
echo "=== Caching glibc baseline benchmarks ($PROCS threads, $RUNS runs each) ==="
echo ""

# Temporary file for per-run results
TEMP_RESULTS=$(mktemp)
trap "rm -f $TEMP_RESULTS" EXIT

for bench in "${BENCHMARKS[@]}"; do
    IFS='|' read -ra parts <<< "$bench"
    name="${parts[0]}"
    binary="${parts[1]}"
    args="${parts[*]:2}"
    args="${args//|/ }"
    
    printf "  %-18s" "$name..."
    
    total_secs=0
    total_rss=0
    
    for ((run=1; run<=RUNS; run++)); do
        # Handle input redirection for barnes
        if [[ "$args" == *"<"* ]]; then
            input_file="${args#*<}"
            input_file="${input_file## }"
            result=$( { /usr/bin/time -f "%E %M" ./"$binary" < "$input_file" 2>&1; } 2>&1 | tail -1 )
        else
            result=$( { /usr/bin/time -f "%E %M" ./"$binary" $args 2>&1; } 2>&1 | tail -1 )
        fi
        
        elapsed=$(echo "$result" | awk '{print $1}')
        rss=$(echo "$result" | awk '{print $2}')
        
        # Convert mm:ss.xx to seconds
        secs=$(echo "$elapsed" | awk -F: '{if (NF==2) print $1*60+$2; else print $1}')
        
        total_secs=$(echo "$total_secs + $secs" | bc)
        total_rss=$(echo "$total_rss + $rss" | bc)
    done
    
    # Calculate averages
    avg_secs=$(echo "scale=2; $total_secs / $RUNS" | bc)
    avg_rss=$(echo "scale=0; $total_rss / $RUNS" | bc)
    
    # Convert back to mm:ss.xx format for display
    if (( $(echo "$avg_secs >= 60" | bc -l) )); then
        mins=$(echo "scale=0; $avg_secs / 60" | bc)
        secs=$(echo "scale=2; $avg_secs - ($mins * 60)" | bc)
        avg_time=$(printf "%d:%05.2f" "$mins" "$secs")
    else
        avg_time=$(printf "0:%05.2f" "$avg_secs")
    fi
    
    printf "%s (%skb) [avg of %d runs]\n" "$avg_time" "$avg_rss" "$RUNS"
    echo "$name $avg_time $avg_rss" >> "$CACHE_FILE"
done

echo ""
echo "=== Glibc baseline cached to $CACHE_FILE ==="
echo ""
