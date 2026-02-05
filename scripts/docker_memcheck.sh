#!/bin/bash
# Docker-based valgrind memcheck runner for inictus (static linking)
set -euo pipefail

# Ensure UTF-8 output
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

PROCS=${1:-2}
cd /mimalloc-bench/out/bench
DATA=/mimalloc-bench/bench
OUTPUT_DIR="/valgrind-reports"

mkdir -p "$OUTPUT_DIR"

echo ""
echo "=== inictus MEMCHECK (Docker) - valgrind analysis ($PROCS threads) ==="
echo ""

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
# Benchmark definitions
# Format: "name|binary|args..."
# ─────────────────────────────────────────────────────────────────────────────
VALGRIND_BENCHMARKS=(
    # Memory correctness tests (run with valgrind)
    "glibc-simple|glibc-simple"
    "cfrac|cfrac|17545186520507317056371138836327483792789528"
    "glibc-thread|glibc-thread|$PROCS"
    "larsonN|larson|5|8|1000|5000|100|4141|$PROCS"
    "mstressN|mstress|$PROCS|10|5"
    "alloc-test1|alloc-test|1"
    "malloc-large|malloc-large"
)

# mleak runs WITHOUT valgrind (too slow), just checks for crashes and RSS
MLEAK_BENCHMARKS=(
    "mleak10|mleak|10"
    "mleak100|mleak|100"
)

# ─────────────────────────────────────────────────────────────────────────────
# Verify all inictus binaries before running
# ─────────────────────────────────────────────────────────────────────────────
echo "--- VERIFYING INICTUS BINARIES ---"
has_errors=0
verified_binaries=""

for bench in "${VALGRIND_BENCHMARKS[@]}" "${MLEAK_BENCHMARKS[@]}"; do
    IFS='|' read -r name binary args <<< "$bench"
    
    # Skip if already verified
    if [[ "$verified_binaries" == *"$binary"* ]]; then
        continue
    fi
    verified_binaries="$verified_binaries $binary"
    
    printf "  %-18s" "$binary..."
    if verify_inictus "./inictus/$binary"; then
        echo "✓ INICTUS (malloc='T')"
    else
        echo "✗ FAIL - malloc not statically linked!"
        has_errors=1
    fi
done

if [ "$has_errors" -eq 1 ]; then
    echo ""
    echo "ERROR: One or more inictus binaries failed verification!"
    exit 1
fi

echo ""
echo "All inictus binaries verified. Starting memcheck..."
echo ""

# ─────────────────────────────────────────────────────────────────────────────
# Run valgrind memcheck on each benchmark
# ─────────────────────────────────────────────────────────────────────────────
SUMMARY_FILE="$OUTPUT_DIR/summary.txt"
echo "=== Valgrind Memcheck Summary ===" > "$SUMMARY_FILE"
echo "Date: $(date)" >> "$SUMMARY_FILE"
echo "Threads: $PROCS" >> "$SUMMARY_FILE"
echo "" >> "$SUMMARY_FILE"

TOTAL_ERRORS=0
TOTAL_LEAKS=0

echo "--- RUNNING VALGRIND MEMCHECK ---"

for bench in "${VALGRIND_BENCHMARKS[@]}"; do
    IFS='|' read -ra parts <<< "$bench"
    name="${parts[0]}"
    binary="${parts[1]}"
    args="${parts[*]:2}"
    args="${args//|/ }"
    
    REPORT="$OUTPUT_DIR/${name}.valgrind.txt"
    printf "  %-18s" "$name..."
    
    # Run valgrind on statically-linked inictus binary
    if [[ "$args" == *"<"* ]]; then
        input_file="${args#*<}"
        input_file="${input_file## }"
        valgrind \
            --leak-check=full \
            --show-leak-kinds=all \
            --track-origins=yes \
            --error-exitcode=0 \
            --log-file="$REPORT" \
            ./inictus/"$binary" < "$input_file" >/dev/null 2>&1 || true
    else
        valgrind \
            --leak-check=full \
            --show-leak-kinds=all \
            --track-origins=yes \
            --error-exitcode=0 \
            --log-file="$REPORT" \
            ./inictus/"$binary" $args >/dev/null 2>&1 || true
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

# ─────────────────────────────────────────────────────────────────────────────
# Run mleak WITHOUT valgrind (too slow), just check for crashes and RSS growth
# ─────────────────────────────────────────────────────────────────────────────
echo ""
echo "--- RUNNING MLEAK (no valgrind, checking RSS) ---"

for bench in "${MLEAK_BENCHMARKS[@]}"; do
    IFS='|' read -ra parts <<< "$bench"
    name="${parts[0]}"
    binary="${parts[1]}"
    args="${parts[*]:2}"
    args="${args//|/ }"
    
    printf "  %-18s" "$name..."
    
    # Run with /usr/bin/time to capture RSS
    TIME_OUTPUT=$(/usr/bin/time -f "%E %M" ./inictus/"$binary" $args 2>&1 >/dev/null) || {
        echo "✗ CRASHED"
        echo "$name: CRASHED" >> "$SUMMARY_FILE"
        TOTAL_ERRORS=$((TOTAL_ERRORS + 1))
        continue
    }
    
    elapsed=$(echo "$TIME_OUTPUT" | tail -1 | awk '{print $1}')
    rss_kb=$(echo "$TIME_OUTPUT" | tail -1 | awk '{print $2}')
    rss_mb=$(echo "scale=1; $rss_kb / 1024" | bc 2>/dev/null || echo "?")
    
    printf "✓ %s (RSS: %sMB)\n" "$elapsed" "$rss_mb"
    echo "$name: PASSED time=$elapsed rss=${rss_mb}MB" >> "$SUMMARY_FILE"
done

echo "" >> "$SUMMARY_FILE"
echo "TOTAL: errors=$TOTAL_ERRORS definitely_lost=$TOTAL_LEAKS bytes" >> "$SUMMARY_FILE"

echo ""
echo "=== SUMMARY ==="
echo "Total errors:        $TOTAL_ERRORS"
echo "Total leaked bytes:  $TOTAL_LEAKS"
echo ""

# Copy reports to mounted volume if present
if [ -d "/output" ]; then
    cp -r "$OUTPUT_DIR"/* /output/
    echo "Reports copied to /output/"
fi

if [ "$TOTAL_ERRORS" -gt 0 ] || [ "$TOTAL_LEAKS" -gt 0 ]; then
    echo "⚠ Issues detected! Review individual reports for details."
    exit 1
else
    echo "✓ All benchmarks passed memory checks."
    exit 0
fi
