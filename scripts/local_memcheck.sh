#!/bin/bash
# Memory leak/error checking using valgrind with LD_PRELOAD for inictus
# Requires: valgrind, mimalloc-bench built locally, inictus built with c_api feature
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

# Configuration
PROCS=${1:-2}  # Lower thread count for valgrind (slow)
MIMALLOC_BENCH="${MIMALLOC_BENCH:-$PROJECT_ROOT/mimalloc-bench}"
INICTUS_LIB="${INICTUS_LIB:-$PROJECT_ROOT/target/release/libinictus.so}"
OUTPUT_DIR="${OUTPUT_DIR:-$PROJECT_ROOT/valgrind-reports}"

# Validate dependencies
if ! command -v valgrind &>/dev/null; then
    echo "ERROR: valgrind not found. Install with: sudo apt install valgrind"
    exit 1
fi

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

mkdir -p "$OUTPUT_DIR"
cd "$MIMALLOC_BENCH/out/bench"
DATA="$MIMALLOC_BENCH/bench"

echo ""
echo "=== inictus MEMCHECK - valgrind analysis ($PROCS threads) ==="
echo "Using: $INICTUS_LIB"
echo "Reports: $OUTPUT_DIR/"
echo ""

# Subset of benchmarks suitable for valgrind (shorter running)
BENCHMARKS=(
    "glibc-simple|glibc-simple"
    "cfrac|cfrac|17545186520507317056371138836327483792789528"
    "glibc-thread|glibc-thread|$PROCS"
    "larsonN|larson|5|8|1000|5000|100|4141|$PROCS"
    "mstressN|mstress|$PROCS|10|5"
    "alloc-test1|alloc-test|1"
    "malloc-large|malloc-large"
)

SUMMARY_FILE="$OUTPUT_DIR/summary.txt"
echo "=== Valgrind Memcheck Summary ===" > "$SUMMARY_FILE"
echo "Date: $(date)" >> "$SUMMARY_FILE"
echo "Threads: $PROCS" >> "$SUMMARY_FILE"
echo "" >> "$SUMMARY_FILE"

TOTAL_ERRORS=0
TOTAL_LEAKS=0

for bench in "${BENCHMARKS[@]}"; do
    IFS='|' read -ra parts <<< "$bench"
    name="${parts[0]}"
    binary="${parts[1]}"
    args="${parts[*]:2}"
    args="${args//|/ }"
    
    REPORT="$OUTPUT_DIR/${name}.valgrind.txt"
    printf "  %-18s" "$name..."
    
    # Run valgrind with leak checking
    if [[ "$args" == *"<"* ]]; then
        input_file="${args#*<}"
        input_file="${input_file## }"
        valgrind \
            --leak-check=full \
            --show-leak-kinds=all \
            --track-origins=yes \
            --error-exitcode=0 \
            --log-file="$REPORT" \
            env LD_PRELOAD="$INICTUS_LIB" ./"$binary" < "$input_file" >/dev/null 2>&1 || true
    else
        valgrind \
            --leak-check=full \
            --show-leak-kinds=all \
            --track-origins=yes \
            --error-exitcode=0 \
            --log-file="$REPORT" \
            env LD_PRELOAD="$INICTUS_LIB" ./"$binary" $args >/dev/null 2>&1 || true
    fi
    
    # Parse results
    errors=$(grep -oP 'ERROR SUMMARY: \K[0-9,]+' "$REPORT" | tr -d ',' || echo "0")
    definitely_lost=$(grep -oP 'definitely lost: \K[0-9,]+' "$REPORT" | tr -d ',' || echo "0")
    possibly_lost=$(grep -oP 'possibly lost: \K[0-9,]+' "$REPORT" | tr -d ',' || echo "0")
    
    if [ "$errors" = "0" ] && [ "$definitely_lost" = "0" ]; then
        printf "✓ clean\n"
        echo "$name: CLEAN" >> "$SUMMARY_FILE"
    else
        printf "✗ errors=%s definitely_lost=%s possibly_lost=%s\n" "$errors" "$definitely_lost" "$possibly_lost"
        echo "$name: ERRORS=$errors DEFINITELY_LOST=$definitely_lost POSSIBLY_LOST=$possibly_lost" >> "$SUMMARY_FILE"
        TOTAL_ERRORS=$((TOTAL_ERRORS + errors))
        TOTAL_LEAKS=$((TOTAL_LEAKS + definitely_lost))
    fi
done

echo "" >> "$SUMMARY_FILE"
echo "TOTAL: errors=$TOTAL_ERRORS definitely_lost=$TOTAL_LEAKS bytes" >> "$SUMMARY_FILE"

echo ""
echo "=== SUMMARY ==="
echo "Total errors:        $TOTAL_ERRORS"
echo "Total leaked bytes:  $TOTAL_LEAKS"
echo ""
echo "Full reports in: $OUTPUT_DIR/"
echo ""

if [ "$TOTAL_ERRORS" -gt 0 ] || [ "$TOTAL_LEAKS" -gt 0 ]; then
    echo "⚠ Issues detected! Review individual reports for details."
    exit 1
else
    echo "✓ All benchmarks passed memory checks."
    exit 0
fi
