# SQLite Async Benchmark

A benchmark comparing three approaches to handling SQLite operations in async Rust (Tokio):

1. **Direct Blocking** - SQLite calls made directly in async context (blocks Tokio worker threads)
2. **spawn_blocking** - SQLite calls wrapped in `tokio::task::spawn_blocking()`
3. **tokio-rusqlite** - Dedicated thread per database (using the `tokio-rusqlite` crate pattern)

The first approach is the one currently implemented in the [ICRC Rosetta](https://github.com/dfinity/ic/tree/f8cb61d9549193cd0b387a53b0d927bf9ac5e9de/rs/rosetta-api/icrc1) application. In production, slow response times to some query endpoints have been observed as the number of tokens (and thus databases) increases, leading to contention on the limited Tokio worker threads. ICRC Rosetta has one thread (the block synchronization thread) that regularly queries the ledger for new blocks and writes them to the database, as well as updating the balances of accounts affected by the transactions in the retrieved blocks. As part of servicing client query requests to the ICRC Rosetta HTTP endpoints, the databases are queried for account balances, transaction histories, and block data. With many databases and frequent writes, the blocking SQLite calls can lead to high latency for read queries, as the worker threads are blocked waiting for the database operations to complete. This benchmark aims to quantify the performance impact of each approach under varying workloads.

## Quick Start

```bash
# Run a quick test (10 seconds per scenario, fewer configurations)
./run_benchmark.sh --quick

# Run full benchmark (default: 60 seconds per scenario)
./run_benchmark.sh

# Custom configuration
./run_benchmark.sh --duration 120 --num-databases 1,10,40,100 --write-delays 100,1000,5000
```

## Command Line Options

### Benchmark Runner (`run_benchmark.sh`)

```
Options:
  -d, --duration SECS     Test duration per scenario (default: 60)
  -o, --output DIR        Output directory (default: results)
  -w, --workers N         Tokio worker threads (default: 4)
  -r, --readers N         Readers per database (default: 3)
  --write-delays MS,MS    Write delays to test (default: 100,1000,10000)
  --num-databases N,N     Database counts to test (default: 1,10,50,100)
  --quick                 Quick mode: 10s duration, fewer scenarios
  -h, --help              Show this help
```

### Direct Binary Usage

```bash
# Build
cargo build --release

# Run benchmark with custom settings
./target/release/benchmark \
    --duration 120 \
    --worker-threads 4 \
    --readers-per-db 3 \
    --write-delays 100,1000,10000 \
    --num-databases 1,10,50,100 \
    --output-dir results

# Parse existing CSV and regenerate plots
./target/release/plot --csv results/benchmark_YYYYMMDD_HHMMSS.csv
```

## Project Structure

```
.
├── src/
│   ├── main.rs          # Benchmark binary
│   └── plot.rs          # CSV parser and plot regenerator
├── templates/
│   ├── plot.gp          # Gnuplot template (works with CSV)
│   └── charts.html      # HTML chart template
├── results/             # Generated output (gitignored)
│   ├── benchmark_*.csv  # Raw data
│   ├── summary_*.md     # Markdown summary
│   ├── charts_*.html    # Interactive charts
│   └── ascii_*.txt      # Text charts
├── run_benchmark.sh     # Main runner script
└── README.md
```

## Output Files

After running the benchmark, you'll find in `results/` (which is gitignored):

| File | Description |
|------|-------------|
| `benchmark_YYYYMMDD_HHMMSS.csv` | Raw benchmark data in CSV format |
| `summary_YYYYMMDD_HHMMSS.md` | Human-readable summary with tables |
| `charts_YYYYMMDD_HHMMSS.html` | Interactive charts (open in browser) |
| `ascii_YYYYMMDD_HHMMSS.txt` | ASCII text charts |

## Viewing Results

### Interactive Charts

```bash
# macOS
open results/plots_*/charts.html

# Linux
xdg-open results/plots_*/charts.html
```

### Terminal Summary

The benchmark prints a summary to stdout. You can also view the markdown summary:

```bash
cat results/summary_*.md
```

### Using Gnuplot

```bash
# Use the template with your CSV file
gnuplot -e "datafile='results/benchmark_YYYYMMDD_HHMMSS.csv'; outdir='results'" templates/plot.gp
open results/benchmark_charts.png
```

## What the Benchmark Measures

### Metrics

- **Read Throughput**: Number of read operations completed per second
- **Write Throughput**: Number of write operations completed per second
- **Avg Read Latency**: Average time for a read operation
- **P99 Read Latency**: 99th percentile read latency (tail latency)
- **Max Read Latency**: Maximum observed read latency

### Test Scenarios

The benchmark tests each approach across:

- Different numbers of databases (simulating multi-token scenarios like ICRC Rosetta)
- Different write delays (simulating slow disk I/O or large database operations)

## Example Results

With 4 Tokio worker threads, 3 readers per database, and varying write delays:

### Write Completion Ratio - 1s write delay

The write completion ratio shows what percentage of expected writes actually completed. A low ratio indicates **worker
thread starvation** in direct blocking mode.

| DBs | Direct Blocking | spawn_blocking | tokio-rusqlite |
|-----|-----------------|----------------|----------------|
| 1   | 100%            | 100%           | 100%           |
| 10  | 27%             | 100%           | 100%           |
| 50  | 6%              | 100%           | 100%           |
| 100 | 2%              | 100%           | 100%           |

### Read Throughput (requests/sec) - 1s write delay

| DBs | Direct Blocking | spawn_blocking | tokio-rusqlite |
|-----|-----------------|----------------|----------------|
| 1   | ~3              | ~3             | ~3             |
| 10  | ~16             | ~30            | ~30            |
| 50  | ~32             | ~130           | ~150           |
| 100 | ~40             | ~256           | ~301           |

### Key Findings

1. **Direct Blocking
   ** suffers severely as database count increases - at 100 databases, only 2% of expected writes complete due to worker thread starvation
2. **spawn_blocking** scales better by offloading to the blocking thread pool
3. **tokio-rusqlite** provides the most predictable latency due to its dedicated-thread-per-database model

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                      Tokio Runtime                              │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐              │
│  │   Worker    │  │   Worker    │  │   Worker    │  ...         │
│  │   Thread    │  │   Thread    │  │   Thread    │              │
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘              │
│         │                │                │                     │
│  ┌──────┴────────────────┴────────────────┴──────┐              │
│  │           Approach 1: Direct Blocking         │              │
│  │  SQLite ops BLOCK these threads!              │              │
│  └───────────────────────────────────────────────┘              │
│                                                                 │
│  ┌───────────────────────────────────────────────┐              │
│  │        Approach 2: spawn_blocking             │              │
│  │  Offload to blocking thread pool ✓            │              │
│  │  ┌─────────┐ ┌─────────┐ ┌─────────┐          │              │
│  │  │Blocking │ │Blocking │ │Blocking │ ...      │              │
│  │  │ Thread  │ │ Thread  │ │ Thread  │          │              │
│  │  └─────────┘ └─────────┘ └─────────┘          │              │
│  └───────────────────────────────────────────────┘              │
│                                                                 │
│  ┌───────────────────────────────────────────────┐              │
│  │        Approach 3: tokio-rusqlite             │              │
│  │  Dedicated thread per database ✓✓             │              │
│  │  ┌─────────┐ ┌─────────┐ ┌─────────┐          │              │
│  │  │  DB 1   │ │  DB 2   │ │  DB 3   │ ...      │              │
│  │  │ Thread  │ │ Thread  │ │ Thread  │          │              │
│  │  └─────────┘ └─────────┘ └─────────┘          │              │
│  └───────────────────────────────────────────────┘              │
└─────────────────────────────────────────────────────────────────┘
```

## License

Apache-2.0
