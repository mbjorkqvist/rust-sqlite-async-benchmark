#!/bin/bash
# SQLite Async Benchmark Runner
# This script builds and runs the complete benchmark suite

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Default values
DURATION=60
OUTPUT_DIR="results"
WORKER_THREADS=4
READERS_PER_DB=3
WRITE_DELAYS="100,1000,10000"
NUM_DATABASES="1,10,50,100"
QUICK_MODE=false

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        -d|--duration)
            DURATION="$2"
            shift 2
            ;;
        -o|--output)
            OUTPUT_DIR="$2"
            shift 2
            ;;
        -w|--workers)
            WORKER_THREADS="$2"
            shift 2
            ;;
        -r|--readers)
            READERS_PER_DB="$2"
            shift 2
            ;;
        --write-delays)
            WRITE_DELAYS="$2"
            shift 2
            ;;
        --num-databases)
            NUM_DATABASES="$2"
            shift 2
            ;;
        --quick)
            QUICK_MODE=true
            DURATION=10
            WRITE_DELAYS="100,1000"
            NUM_DATABASES="1,10"
            shift
            ;;
        -h|--help)
            echo "SQLite Async Benchmark Runner"
            echo ""
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  -d, --duration SECS     Test duration per scenario (default: 60)"
            echo "  -o, --output DIR        Output directory (default: results)"
            echo "  -w, --workers N         Tokio worker threads (default: 4)"
            echo "  -r, --readers N         Readers per database (default: 3)"
            echo "  --write-delays MS,MS    Write delays to test (default: 100,1000,10000)"
            echo "  --num-databases N,N     Database counts to test (default: 1,10,50,100)"
            echo "  --quick                 Quick mode: 10s duration, fewer scenarios"
            echo "  -h, --help              Show this help"
            echo ""
            echo "Examples:"
            echo "  $0                              # Run full benchmark"
            echo "  $0 --quick                      # Quick test run"
            echo "  $0 -d 120 --num-databases 1,40  # 2-minute tests with specific DB counts"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

echo "╔══════════════════════════════════════════════════════════════════════════════╗"
echo "║              SQLite Async Benchmark Runner                                   ║"
echo "╚══════════════════════════════════════════════════════════════════════════════╝"
echo ""
echo "Configuration:"
echo "  Duration:        ${DURATION}s per scenario"
echo "  Worker threads:  ${WORKER_THREADS}"
echo "  Readers per DB:  ${READERS_PER_DB}"
echo "  Write delays:    ${WRITE_DELAYS}"
echo "  Database counts: ${NUM_DATABASES}"
echo "  Output dir:      ${OUTPUT_DIR}"
if [ "$QUICK_MODE" = true ]; then
    echo "  Mode:            QUICK (reduced scenarios)"
fi
echo ""

# Build the project
echo "━━━ Building project... ━━━"
cargo build --release

# Create output directory
mkdir -p "$OUTPUT_DIR"

# Run the benchmark
echo ""
echo "━━━ Running benchmark... ━━━"
./target/release/benchmark \
    --duration "$DURATION" \
    --output-dir "$OUTPUT_DIR" \
    --worker-threads "$WORKER_THREADS" \
    --readers-per-db "$READERS_PER_DB" \
    --write-delays "$WRITE_DELAYS" \
    --num-databases "$NUM_DATABASES"

# Find the latest CSV file
LATEST_CSV=$(ls -t "$OUTPUT_DIR"/benchmark_*.csv 2>/dev/null | head -1)
LATEST_HTML=$(ls -t "$OUTPUT_DIR"/charts_*.html 2>/dev/null | head -1)

echo ""
echo "╔══════════════════════════════════════════════════════════════════════════════╗"
echo "║                        BENCHMARK COMPLETE                                    ║"
echo "╚══════════════════════════════════════════════════════════════════════════════╝"
echo ""
echo "Results are in: $OUTPUT_DIR/"
echo ""
if [ -n "$LATEST_HTML" ]; then
    echo "To view the interactive charts:"
    echo "  open $LATEST_HTML"
    echo ""
fi
if [ -n "$LATEST_CSV" ]; then
    echo "To regenerate plots from existing CSV:"
    echo "  cargo run --release --bin plot -- --csv $LATEST_CSV"
    echo ""
    echo "To generate gnuplot charts:"
    echo "  gnuplot -e \"datafile='$LATEST_CSV'; outdir='$OUTPUT_DIR'\" templates/plot.gp"
fi

