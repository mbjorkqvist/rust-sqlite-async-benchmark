# SQLite Async Benchmark

A comprehensive benchmark comparing three approaches to handling SQLite operations in async Rust applications.

## Background

When using SQLite in an async Rust application (like ICRC Rosetta with 40+ token databases), blocking SQLite operations can starve the Tokio async runtime's worker threads, degrading overall performance.

This benchmark compares three approaches:

1. **Direct Blocking** - SQLite calls made directly in async context (current ICRC Rosetta approach)
2. **spawn_blocking** - SQLite calls wrapped in `tokio::task::spawn_blocking()`
3. **tokio-rusqlite** - Dedicated thread per database using the `tokio-rusqlite` crate

## Prerequisites

- Rust 1.70+ (with cargo)
- ~20 minutes for the full benchmark (3 scenarios × 2 minutes × 3 approaches)

## Quick Start

### Run the Full Benchmark

```bash
# Clone or navigate to this directory
cd rosetta-like_spawn_blocking_tokio_rusqlite_benchmarking

# Build in release mode (important for accurate results!)
cargo build --release

# Run the benchmark (~18 minutes)
cargo run --release
```

### Run a Quick Test (modify test duration)

Edit `src/main.rs` and change:
```rust
let test_duration_secs: u64 = 120; // Change to 10 for quick testing
```

Then run:
```bash
cargo run --release
```

## Configuration Parameters

The benchmark uses these default settings (can be modified in `src/main.rs`):

| Parameter | Default | Description |
|-----------|---------|-------------|
| `worker_threads` | 4 | Tokio worker threads |
| `num_databases` | 8 | Number of SQLite databases (simulates tokens) |
| `readers_per_db` | 3 | Reader tasks per database |
| `test_duration_secs` | 120 | Duration per scenario (seconds) |
| Write delays | 100ms, 1s, 10s | Simulated write operation durations |

## What the Benchmark Measures

For each approach and write delay combination:

- **Read Throughput**: Number of read requests completed
- **Write Throughput**: Number of write operations completed
- **P99 Read Latency**: 99th percentile read latency (tail latency)
- **Max Read Latency**: Maximum observed read latency
- **Average Read Latency**: Mean read latency

## Understanding the Results

### Why Direct Blocking is Problematic

With direct blocking:
- SQLite operations block Tokio worker threads
- If `num_databases >= worker_threads`, the runtime can be completely starved
- Read requests queue up waiting for blocked workers

### Why spawn_blocking Helps

With `spawn_blocking`:
- SQLite operations run on Tokio's blocking thread pool
- Worker threads stay free for async work
- Higher P99 latency due to mutex contention in the pool

### Why tokio-rusqlite is Best

With `tokio-rusqlite`:
- One dedicated thread per database
- Natural queuing via channel (matches SQLite's single-writer model)
- Predictable latency ≈ write duration
- No thread pool contention

## Expected Results Summary

| Write Delay | Read Improvement | Write Improvement | P99 Latency Winner |
|-------------|------------------|-------------------|-------------------|
| 100ms | 1.6x | 3.1x | tokio-rusqlite |
| 1s | 1.8x | 3.1x | tokio-rusqlite |
| 10s | 1.9x | 2.4x | tokio-rusqlite |

See [RESULTS.md](RESULTS.md) for detailed benchmark results.

## Project Structure

```
.
├── Cargo.toml          # Project dependencies
├── README.md           # This file
├── RESULTS.md          # Detailed benchmark results
└── src/
    └── main.rs         # Benchmark code
```

## Dependencies

- `rusqlite` (0.29) - SQLite bindings
- `tokio` (1.x) - Async runtime
- `tokio-rusqlite` (0.4) - Async SQLite wrapper
- `futures` (0.3) - Async utilities

## License

Apache-2.0

## Related

- [tokio-rusqlite crate](https://crates.io/crates/tokio-rusqlite)
- [Tokio spawn_blocking documentation](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html)

