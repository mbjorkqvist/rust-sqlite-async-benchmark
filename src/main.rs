//! SQLite Async Benchmark
//!
//! This benchmark compares three approaches to handling SQLite operations in async Rust:
//!
//! 1. **Direct Blocking** - SQLite calls made directly in async context (blocks worker threads)
//! 2. **spawn_blocking** - SQLite calls wrapped in tokio::task::spawn_blocking()
//! 3. **tokio-rusqlite** - Dedicated thread per database (like tokio-rusqlite crate)
//!
//! Run with: `cargo run --release -- --help`

use chrono::Local;
use clap::Parser;
use csv::Writer;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_rusqlite::Connection as AsyncConnection;

/// SQLite Async Benchmark - Compare direct blocking, spawn_blocking, and tokio-rusqlite
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Test duration in seconds per scenario
    #[arg(short = 'd', long, default_value_t = 120)]
    duration: u64,

    /// Output directory for results (CSV, graph, summary)
    #[arg(short = 'o', long, default_value = "results")]
    output_dir: PathBuf,

    /// Number of Tokio worker threads
    #[arg(short = 'w', long, default_value_t = 4)]
    worker_threads: usize,

    /// Readers per database
    #[arg(short = 'r', long, default_value_t = 3)]
    readers_per_db: usize,

    /// Write delays to test (comma-separated, in ms)
    #[arg(long, default_value = "100,1000,10000", value_delimiter = ',')]
    write_delays: Vec<u64>,

    /// Number of databases to test (comma-separated)
    #[arg(long, default_value = "1,10,50,100", value_delimiter = ',')]
    num_databases: Vec<usize>,
}

// ============================================================================
// CSV Result Structure
// ============================================================================

#[derive(Debug, Clone, Serialize)]
struct BenchmarkResult {
    approach: String,
    num_databases: usize,
    write_delay_ms: u64,
    test_duration_secs: u64,
    worker_threads: usize,
    readers_per_db: usize,
    // Read metrics
    read_requests_completed: u64,
    read_throughput_per_sec: f64,
    avg_read_latency_ms: f64,
    p50_read_latency_ms: f64,
    p99_read_latency_ms: f64,
    max_read_latency_ms: f64,
    // Write metrics
    write_ops_completed: u64,
    write_throughput_per_sec: f64,
    avg_write_latency_ms: f64,
    p99_write_latency_ms: f64,
    // Starvation indicator: ratio of expected vs actual throughput
    write_completion_ratio: f64,
}

// ============================================================================
// Database Operations
// ============================================================================

fn setup_database(conn: &Connection) {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS blocks (
            id INTEGER PRIMARY KEY,
            hash TEXT NOT NULL,
            parent_hash TEXT,
            timestamp INTEGER NOT NULL,
            data BLOB
        );
        CREATE INDEX IF NOT EXISTS idx_blocks_timestamp ON blocks(timestamp);
        CREATE INDEX IF NOT EXISTS idx_blocks_hash ON blocks(hash);
        
        CREATE TABLE IF NOT EXISTS transactions (
            id INTEGER PRIMARY KEY,
            block_id INTEGER NOT NULL,
            from_account TEXT NOT NULL,
            to_account TEXT NOT NULL,
            amount INTEGER NOT NULL,
            FOREIGN KEY (block_id) REFERENCES blocks(id)
        );
        CREATE INDEX IF NOT EXISTS idx_tx_from ON transactions(from_account);
        CREATE INDEX IF NOT EXISTS idx_tx_to ON transactions(to_account);
        ",
    )
    .expect("Failed to create schema");

    for i in 0..100 {
        conn.execute(
            "INSERT INTO blocks (hash, parent_hash, timestamp, data) VALUES (?1, ?2, ?3, ?4)",
            params![
                format!("hash_{}", i),
                if i > 0 { Some(format!("hash_{}", i - 1)) } else { None },
                1000000 + i,
                vec![0u8; 256],
            ],
        )
        .expect("Failed to insert block");

        for j in 0..5 {
            conn.execute(
                "INSERT INTO transactions (block_id, from_account, to_account, amount) VALUES (?1, ?2, ?3, ?4)",
                params![
                    i + 1,
                    format!("account_{}", (i + j) % 50),
                    format!("account_{}", (i + j + 1) % 50),
                    1000 + j,
                ],
            )
            .expect("Failed to insert transaction");
        }
    }
}

fn do_write_operation_with_delay(conn: &Connection, block_num: u64, delay_ms: u64) {
    if delay_ms > 0 {
        std::thread::sleep(Duration::from_millis(delay_ms));
    }

    conn.execute(
        "INSERT INTO blocks (hash, parent_hash, timestamp, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            format!("hash_{}", block_num + 1000),
            format!("hash_{}", block_num + 999),
            2000000 + block_num as i64,
            vec![0u8; 256],
        ],
    )
    .expect("Failed to insert block");

    let block_id = conn.last_insert_rowid();
    for j in 0..3 {
        conn.execute(
            "INSERT INTO transactions (block_id, from_account, to_account, amount) VALUES (?1, ?2, ?3, ?4)",
            params![
                block_id,
                format!("account_{}", (block_num + j) % 50),
                format!("account_{}", (block_num + j + 1) % 50),
                2000 + j,
            ],
        )
        .expect("Failed to insert transaction");
    }
}

fn do_read_operation(conn: &Connection) -> usize {
    let mut stmt = conn
        .prepare_cached(
            "SELECT b.hash, b.timestamp, t.from_account, t.to_account, t.amount 
             FROM blocks b 
             JOIN transactions t ON t.block_id = b.id 
             WHERE b.timestamp > ?1 
             ORDER BY b.timestamp DESC 
             LIMIT 50",
        )
        .expect("Failed to prepare statement");

    let rows: Vec<(String, i64, String, String, i64)> = stmt
        .query_map(params![1000000], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
        })
        .expect("Query failed")
        .filter_map(|r| r.ok())
        .collect();

    rows.len()
}

// ============================================================================
// Internal Result Structure
// ============================================================================

struct TestResults {
    read_requests_completed: u64,
    write_ops_completed: u64,
    read_latencies_us: Vec<u64>,
    write_latencies_us: Vec<u64>,
}

impl TestResults {
    fn new() -> Self {
        Self {
            read_requests_completed: 0,
            write_ops_completed: 0,
            read_latencies_us: Vec::new(),
            write_latencies_us: Vec::new(),
        }
    }

    fn compute_percentile(latencies: &[u64], percentile: f64) -> f64 {
        if latencies.is_empty() {
            return 0.0;
        }
        let idx = ((latencies.len() as f64 * percentile / 100.0) as usize).min(latencies.len() - 1);
        latencies[idx] as f64 / 1000.0 // Convert to ms
    }

    fn compute_avg(latencies: &[u64]) -> f64 {
        if latencies.is_empty() {
            return 0.0;
        }
        let sum: u64 = latencies.iter().sum();
        sum as f64 / latencies.len() as f64 / 1000.0 // Convert to ms
    }

    fn compute_max(latencies: &[u64]) -> f64 {
        latencies.iter().max().copied().unwrap_or(0) as f64 / 1000.0
    }
}

// ============================================================================
// Approach 1: Direct Blocking
// ============================================================================

fn run_direct_blocking(
    worker_threads: usize,
    num_databases: usize,
    readers_per_db: usize,
    test_duration_secs: u64,
    write_delay_ms: u64,
) -> TestResults {
    let databases: Vec<Arc<Mutex<Connection>>> = (0..num_databases)
        .map(|_| {
            let conn = Connection::open_in_memory().expect("Failed to open database");
            setup_database(&conn);
            Arc::new(Mutex::new(conn))
        })
        .collect();

    let read_latencies = Arc::new(Mutex::new(Vec::new()));
    let write_latencies = Arc::new(Mutex::new(Vec::new()));
    let read_count = Arc::new(AtomicU64::new(0));
    let write_count = Arc::new(AtomicU64::new(0));
    let stop_flag = Arc::new(AtomicBool::new(false));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let mut handles = Vec::new();

        // Writers - measure from task start (includes queue time via yield)
        for (db_idx, db) in databases.iter().enumerate() {
            let db = db.clone();
            let stop = stop_flag.clone();
            let count = write_count.clone();
            let latencies = write_latencies.clone();
            let delay = write_delay_ms;

            handles.push(tokio::spawn(async move {
                let mut block_num = db_idx as u64 * 10000;
                while !stop.load(Ordering::Relaxed) {
                    let start = Instant::now();
                    // This blocks the Tokio worker thread!
                    {
                        let c = db.lock().unwrap();
                        do_write_operation_with_delay(&c, block_num, delay);
                    }
                    let elapsed_us = start.elapsed().as_micros() as u64;
                    latencies.lock().unwrap().push(elapsed_us);
                    count.fetch_add(1, Ordering::Relaxed);
                    block_num += 1;
                    // Yield to allow other tasks to run
                    tokio::task::yield_now().await;
                }
            }));
        }

        // Readers - measure from task start
        for db in databases.iter() {
            for _ in 0..readers_per_db {
                let db = db.clone();
                let stop = stop_flag.clone();
                let count = read_count.clone();
                let latencies = read_latencies.clone();

                handles.push(tokio::spawn(async move {
                    while !stop.load(Ordering::Relaxed) {
                        let start = Instant::now();
                        // This blocks the Tokio worker thread!
                        {
                            let c = db.lock().unwrap();
                            do_read_operation(&c);
                        }
                        let elapsed_us = start.elapsed().as_micros() as u64;
                        latencies.lock().unwrap().push(elapsed_us);
                        count.fetch_add(1, Ordering::Relaxed);
                        // Small delay between reads
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }));
            }
        }

        tokio::time::sleep(Duration::from_secs(test_duration_secs)).await;
        stop_flag.store(true, Ordering::Relaxed);

        // Give tasks time to finish their current operation
        let timeout = Duration::from_millis(write_delay_ms + 1000);
        for handle in handles {
            let _ = tokio::time::timeout(timeout, handle).await;
        }
    });

    let mut read_lats = read_latencies.lock().unwrap().clone();
    let mut write_lats = write_latencies.lock().unwrap().clone();
    read_lats.sort_unstable();
    write_lats.sort_unstable();

    TestResults {
        read_requests_completed: read_count.load(Ordering::Relaxed),
        write_ops_completed: write_count.load(Ordering::Relaxed),
        read_latencies_us: read_lats,
        write_latencies_us: write_lats,
    }
}

// ============================================================================
// Approach 2: spawn_blocking
// ============================================================================

fn run_spawn_blocking(
    worker_threads: usize,
    num_databases: usize,
    readers_per_db: usize,
    test_duration_secs: u64,
    write_delay_ms: u64,
) -> TestResults {
    let databases: Vec<Arc<Mutex<Connection>>> = (0..num_databases)
        .map(|_| {
            let conn = Connection::open_in_memory().expect("Failed to open database");
            setup_database(&conn);
            Arc::new(Mutex::new(conn))
        })
        .collect();

    let read_latencies = Arc::new(Mutex::new(Vec::new()));
    let write_latencies = Arc::new(Mutex::new(Vec::new()));
    let read_count = Arc::new(AtomicU64::new(0));
    let write_count = Arc::new(AtomicU64::new(0));
    let stop_flag = Arc::new(AtomicBool::new(false));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let mut handles = Vec::new();

        // Writers with spawn_blocking
        for (db_idx, db) in databases.iter().enumerate() {
            let db = db.clone();
            let stop = stop_flag.clone();
            let count = write_count.clone();
            let latencies = write_latencies.clone();
            let delay = write_delay_ms;

            handles.push(tokio::spawn(async move {
                let mut block_num = db_idx as u64 * 10000;
                while !stop.load(Ordering::Relaxed) {
                    let start = Instant::now();
                    let db = db.clone();
                    let bn = block_num;

                    // Offload to blocking thread pool
                    tokio::task::spawn_blocking(move || {
                        let c = db.lock().unwrap();
                        do_write_operation_with_delay(&c, bn, delay);
                    })
                    .await
                    .ok();

                    let elapsed_us = start.elapsed().as_micros() as u64;
                    latencies.lock().unwrap().push(elapsed_us);
                    count.fetch_add(1, Ordering::Relaxed);
                    block_num += 1;
                }
            }));
        }

        // Readers with spawn_blocking
        for db in databases.iter() {
            for _ in 0..readers_per_db {
                let db = db.clone();
                let stop = stop_flag.clone();
                let count = read_count.clone();
                let latencies = read_latencies.clone();

                handles.push(tokio::spawn(async move {
                    while !stop.load(Ordering::Relaxed) {
                        let start = Instant::now();
                        let db = db.clone();

                        // Offload to blocking thread pool
                        tokio::task::spawn_blocking(move || {
                            let c = db.lock().unwrap();
                            do_read_operation(&c);
                        })
                        .await
                        .ok();

                        let elapsed_us = start.elapsed().as_micros() as u64;
                        latencies.lock().unwrap().push(elapsed_us);
                        count.fetch_add(1, Ordering::Relaxed);
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }));
            }
        }

        tokio::time::sleep(Duration::from_secs(test_duration_secs)).await;
        stop_flag.store(true, Ordering::Relaxed);

        for handle in handles {
            let _ = handle.await;
        }
    });

    let mut read_lats = read_latencies.lock().unwrap().clone();
    let mut write_lats = write_latencies.lock().unwrap().clone();
    read_lats.sort_unstable();
    write_lats.sort_unstable();

    TestResults {
        read_requests_completed: read_count.load(Ordering::Relaxed),
        write_ops_completed: write_count.load(Ordering::Relaxed),
        read_latencies_us: read_lats,
        write_latencies_us: write_lats,
    }
}

// ============================================================================
// Approach 3: tokio-rusqlite
// ============================================================================

fn run_tokio_rusqlite(
    worker_threads: usize,
    num_databases: usize,
    readers_per_db: usize,
    test_duration_secs: u64,
    write_delay_ms: u64,
) -> TestResults {
    let read_latencies = Arc::new(Mutex::new(Vec::new()));
    let write_latencies = Arc::new(Mutex::new(Vec::new()));
    let read_count = Arc::new(AtomicU64::new(0));
    let write_count = Arc::new(AtomicU64::new(0));
    let stop_flag = Arc::new(AtomicBool::new(false));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        // Create tokio-rusqlite connections
        let databases: Vec<Arc<AsyncConnection>> = futures::future::join_all(
            (0..num_databases).map(|_| async {
                let conn = AsyncConnection::open_in_memory()
                    .await
                    .expect("Failed to open async database");

                conn.call(|conn| {
                    setup_database(conn);
                    Ok(())
                })
                .await
                .expect("Failed to setup database");

                Arc::new(conn)
            }),
        )
        .await;

        let mut handles = Vec::new();

        // Writers using tokio-rusqlite
        for (db_idx, db) in databases.iter().enumerate() {
            let db = db.clone();
            let stop = stop_flag.clone();
            let count = write_count.clone();
            let latencies = write_latencies.clone();
            let delay = write_delay_ms;

            handles.push(tokio::spawn(async move {
                let mut block_num = db_idx as u64 * 10000;
                while !stop.load(Ordering::Relaxed) {
                    let start = Instant::now();
                    let bn = block_num;

                    // tokio-rusqlite handles the threading
                    db.call(move |conn| {
                        do_write_operation_with_delay(conn, bn, delay);
                        Ok(())
                    })
                    .await
                    .ok();

                    let elapsed_us = start.elapsed().as_micros() as u64;
                    latencies.lock().unwrap().push(elapsed_us);
                    count.fetch_add(1, Ordering::Relaxed);
                    block_num += 1;
                }
            }));
        }

        // Readers using tokio-rusqlite
        for db in databases.iter() {
            for _ in 0..readers_per_db {
                let db = db.clone();
                let stop = stop_flag.clone();
                let count = read_count.clone();
                let latencies = read_latencies.clone();

                handles.push(tokio::spawn(async move {
                    while !stop.load(Ordering::Relaxed) {
                        let start = Instant::now();

                        db.call(|conn| Ok(do_read_operation(conn))).await.ok();

                        let elapsed_us = start.elapsed().as_micros() as u64;
                        latencies.lock().unwrap().push(elapsed_us);
                        count.fetch_add(1, Ordering::Relaxed);
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }));
            }
        }

        tokio::time::sleep(Duration::from_secs(test_duration_secs)).await;
        stop_flag.store(true, Ordering::Relaxed);

        for handle in handles {
            let _ = handle.await;
        }

        drop(databases);
    });

    let mut read_lats = read_latencies.lock().unwrap().clone();
    let mut write_lats = write_latencies.lock().unwrap().clone();
    read_lats.sort_unstable();
    write_lats.sort_unstable();

    TestResults {
        read_requests_completed: read_count.load(Ordering::Relaxed),
        write_ops_completed: write_count.load(Ordering::Relaxed),
        read_latencies_us: read_lats,
        write_latencies_us: write_lats,
    }
}

// ============================================================================
// Main
// ============================================================================

fn main() {
    let args = Args::parse();

    // Create output directory
    std::fs::create_dir_all(&args.output_dir).expect("Failed to create output directory");

    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    let csv_path = args.output_dir.join(format!("benchmark_{}.csv", timestamp));
    let summary_path = args.output_dir.join(format!("summary_{}.md", timestamp));

    println!("\n");
    println!("╔══════════════════════════════════════════════════════════════════════════════╗");
    println!("║              SQLite Async Benchmark - Comprehensive Test                     ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════╝");

    println!();
    println!("Configuration:");
    println!("  Tokio worker threads: {}", args.worker_threads);
    println!("  Readers per DB:       {}", args.readers_per_db);
    println!("  Test duration:        {} seconds", args.duration);
    println!("  Database counts:      {:?}", args.num_databases);
    println!("  Write delays (ms):    {:?}", args.write_delays);
    println!("  Output directory:     {}", args.output_dir.display());
    println!();

    let total_tests = args.num_databases.len() * args.write_delays.len() * 3;
    let estimated_time = total_tests as u64 * args.duration;
    println!(
        "  Total tests: {} (estimated time: {} minutes)",
        total_tests,
        estimated_time / 60
    );
    println!();

    // Collect all results
    let mut all_results: Vec<BenchmarkResult> = Vec::new();

    let mut test_num = 0;
    for &num_dbs in &args.num_databases {
        for &write_delay in &args.write_delays {
            let delay_name = format_delay(write_delay);

            // Calculate expected writes (1 writer per DB, each write takes write_delay ms)
            let expected_writes_per_db = (args.duration * 1000) / write_delay.max(1);
            let expected_total_writes = expected_writes_per_db * num_dbs as u64;

            println!(
                "\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
            );
            println!(
                "  SCENARIO: {} databases, {} write delay (expected ~{} writes)",
                num_dbs, delay_name, expected_total_writes
            );
            println!(
                "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
            );

            // Direct Blocking
            test_num += 1;
            println!(
                "\n  [{}/{}] Running DIRECT BLOCKING...",
                test_num, total_tests
            );
            let start = Instant::now();
            let results = run_direct_blocking(
                args.worker_threads,
                num_dbs,
                args.readers_per_db,
                args.duration,
                write_delay,
            );
            println!("  ✓ Completed in {:.1}s", start.elapsed().as_secs_f64());
            println!(
                "    Reads: {}, Writes: {} ({:.1}% of expected)",
                results.read_requests_completed,
                results.write_ops_completed,
                results.write_ops_completed as f64 / expected_total_writes as f64 * 100.0
            );
            all_results.push(to_benchmark_result(
                "direct_blocking",
                num_dbs,
                write_delay,
                expected_total_writes,
                &args,
                &results,
            ));

            // spawn_blocking
            test_num += 1;
            println!(
                "\n  [{}/{}] Running SPAWN_BLOCKING...",
                test_num, total_tests
            );
            let start = Instant::now();
            let results = run_spawn_blocking(
                args.worker_threads,
                num_dbs,
                args.readers_per_db,
                args.duration,
                write_delay,
            );
            println!("  ✓ Completed in {:.1}s", start.elapsed().as_secs_f64());
            println!(
                "    Reads: {}, Writes: {} ({:.1}% of expected)",
                results.read_requests_completed,
                results.write_ops_completed,
                results.write_ops_completed as f64 / expected_total_writes as f64 * 100.0
            );
            all_results.push(to_benchmark_result(
                "spawn_blocking",
                num_dbs,
                write_delay,
                expected_total_writes,
                &args,
                &results,
            ));

            // tokio-rusqlite
            test_num += 1;
            println!(
                "\n  [{}/{}] Running TOKIO-RUSQLITE...",
                test_num, total_tests
            );
            let start = Instant::now();
            let results = run_tokio_rusqlite(
                args.worker_threads,
                num_dbs,
                args.readers_per_db,
                args.duration,
                write_delay,
            );
            println!("  ✓ Completed in {:.1}s", start.elapsed().as_secs_f64());
            println!(
                "    Reads: {}, Writes: {} ({:.1}% of expected)",
                results.read_requests_completed,
                results.write_ops_completed,
                results.write_ops_completed as f64 / expected_total_writes as f64 * 100.0
            );
            all_results.push(to_benchmark_result(
                "tokio_rusqlite",
                num_dbs,
                write_delay,
                expected_total_writes,
                &args,
                &results,
            ));
        }
    }

    // Write CSV
    println!("\n\n  Writing results to CSV: {}", csv_path.display());
    write_csv(&csv_path, &all_results).expect("Failed to write CSV");

    // Write summary
    println!("  Writing summary to: {}", summary_path.display());
    write_summary(&summary_path, &args, &all_results).expect("Failed to write summary");

    // Generate plot
    println!("  Generating plots...");
    generate_plots(&args.output_dir, &all_results, &timestamp.to_string());

    println!("\n");
    println!("╔══════════════════════════════════════════════════════════════════════════════╗");
    println!("║                           BENCHMARK COMPLETE                                 ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════╝");
    println!();
    println!("  Output files in {}:", args.output_dir.display());
    println!("    - benchmark_{}.csv   (raw data)", timestamp);
    println!("    - summary_{}.md      (markdown summary)", timestamp);
    println!("    - charts_{}.html     (interactive charts)", timestamp);
    println!("    - ascii_{}.txt       (text charts)", timestamp);
    println!();
    println!("  To generate gnuplot charts:");
    println!(
        "    gnuplot -e \"datafile='{}'; outdir='{}'\" templates/plot.gp",
        csv_path.display(),
        args.output_dir.display()
    );
    println!();
}

fn format_delay(delay_ms: u64) -> String {
    if delay_ms >= 1000 {
        format!("{}s", delay_ms / 1000)
    } else {
        format!("{}ms", delay_ms)
    }
}

fn to_benchmark_result(
    approach: &str,
    num_databases: usize,
    write_delay_ms: u64,
    expected_writes: u64,
    args: &Args,
    results: &TestResults,
) -> BenchmarkResult {
    BenchmarkResult {
        approach: approach.to_string(),
        num_databases,
        write_delay_ms,
        test_duration_secs: args.duration,
        worker_threads: args.worker_threads,
        readers_per_db: args.readers_per_db,
        // Read metrics
        read_requests_completed: results.read_requests_completed,
        read_throughput_per_sec: results.read_requests_completed as f64 / args.duration as f64,
        avg_read_latency_ms: TestResults::compute_avg(&results.read_latencies_us),
        p50_read_latency_ms: TestResults::compute_percentile(&results.read_latencies_us, 50.0),
        p99_read_latency_ms: TestResults::compute_percentile(&results.read_latencies_us, 99.0),
        max_read_latency_ms: TestResults::compute_max(&results.read_latencies_us),
        // Write metrics
        write_ops_completed: results.write_ops_completed,
        write_throughput_per_sec: results.write_ops_completed as f64 / args.duration as f64,
        avg_write_latency_ms: TestResults::compute_avg(&results.write_latencies_us),
        p99_write_latency_ms: TestResults::compute_percentile(&results.write_latencies_us, 99.0),
        // Starvation indicator
        write_completion_ratio: if expected_writes > 0 {
            results.write_ops_completed as f64 / expected_writes as f64
        } else {
            1.0
        },
    }
}

fn write_csv(
    path: &PathBuf,
    results: &[BenchmarkResult],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = Writer::from_path(path)?;
    for result in results {
        writer.serialize(result)?;
    }
    writer.flush()?;
    Ok(())
}

fn write_summary(
    path: &PathBuf,
    args: &Args,
    results: &[BenchmarkResult],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = File::create(path)?;

    writeln!(file, "# SQLite Async Benchmark Results")?;
    writeln!(file)?;
    writeln!(
        file,
        "Generated: {}",
        Local::now().format("%Y-%m-%d %H:%M:%S")
    )?;
    writeln!(file)?;
    writeln!(file, "## Configuration")?;
    writeln!(file)?;
    writeln!(file, "| Parameter | Value |")?;
    writeln!(file, "|-----------|-------|")?;
    writeln!(file, "| Worker threads | {} |", args.worker_threads)?;
    writeln!(file, "| Readers per DB | {} |", args.readers_per_db)?;
    writeln!(file, "| Test duration | {} seconds |", args.duration)?;
    writeln!(file, "| Database counts | {:?} |", args.num_databases)?;
    writeln!(file, "| Write delays (ms) | {:?} |", args.write_delays)?;
    writeln!(file)?;

    writeln!(file, "## Key Insight: Write Completion Ratio")?;
    writeln!(file)?;
    writeln!(file, "The **write completion ratio** shows what percentage of expected writes actually completed.")?;
    writeln!(file, "A low ratio indicates worker thread starvation in direct blocking mode.")?;
    writeln!(file)?;

    // Group results by write delay
    for &write_delay in &args.write_delays {
        writeln!(
            file,
            "## Results for {} write delay",
            format_delay(write_delay)
        )?;
        writeln!(file)?;

        writeln!(file, "### Write Completion (Starvation Indicator)")?;
        writeln!(file)?;
        writeln!(
            file,
            "| DBs | Direct Blocking | spawn_blocking | tokio-rusqlite |"
        )?;
        writeln!(file, "|-----|-----------------|----------------|----------------|")?;

        for &num_dbs in &args.num_databases {
            let direct = results
                .iter()
                .find(|r| {
                    r.approach == "direct_blocking"
                        && r.num_databases == num_dbs
                        && r.write_delay_ms == write_delay
                })
                .map(|r| r.write_completion_ratio * 100.0)
                .unwrap_or(0.0);
            let spawn = results
                .iter()
                .find(|r| {
                    r.approach == "spawn_blocking"
                        && r.num_databases == num_dbs
                        && r.write_delay_ms == write_delay
                })
                .map(|r| r.write_completion_ratio * 100.0)
                .unwrap_or(0.0);
            let tokio = results
                .iter()
                .find(|r| {
                    r.approach == "tokio_rusqlite"
                        && r.num_databases == num_dbs
                        && r.write_delay_ms == write_delay
                })
                .map(|r| r.write_completion_ratio * 100.0)
                .unwrap_or(0.0);

            writeln!(
                file,
                "| {} | {:.1}% | {:.1}% | {:.1}% |",
                num_dbs, direct, spawn, tokio
            )?;
        }
        writeln!(file)?;

        writeln!(file, "### Read Throughput (requests/sec)")?;
        writeln!(file)?;
        writeln!(
            file,
            "| DBs | Direct Blocking | spawn_blocking | tokio-rusqlite |"
        )?;
        writeln!(file, "|-----|-----------------|----------------|----------------|")?;

        for &num_dbs in &args.num_databases {
            let direct = results
                .iter()
                .find(|r| {
                    r.approach == "direct_blocking"
                        && r.num_databases == num_dbs
                        && r.write_delay_ms == write_delay
                })
                .map(|r| r.read_throughput_per_sec)
                .unwrap_or(0.0);
            let spawn = results
                .iter()
                .find(|r| {
                    r.approach == "spawn_blocking"
                        && r.num_databases == num_dbs
                        && r.write_delay_ms == write_delay
                })
                .map(|r| r.read_throughput_per_sec)
                .unwrap_or(0.0);
            let tokio = results
                .iter()
                .find(|r| {
                    r.approach == "tokio_rusqlite"
                        && r.num_databases == num_dbs
                        && r.write_delay_ms == write_delay
                })
                .map(|r| r.read_throughput_per_sec)
                .unwrap_or(0.0);

            writeln!(
                file,
                "| {} | {:.1} | {:.1} | {:.1} |",
                num_dbs, direct, spawn, tokio
            )?;
        }
        writeln!(file)?;

        writeln!(file, "### P99 Read Latency (ms)")?;
        writeln!(file)?;
        writeln!(
            file,
            "| DBs | Direct Blocking | spawn_blocking | tokio-rusqlite |"
        )?;
        writeln!(file, "|-----|-----------------|----------------|----------------|")?;

        for &num_dbs in &args.num_databases {
            let direct = results
                .iter()
                .find(|r| {
                    r.approach == "direct_blocking"
                        && r.num_databases == num_dbs
                        && r.write_delay_ms == write_delay
                })
                .map(|r| r.p99_read_latency_ms)
                .unwrap_or(0.0);
            let spawn = results
                .iter()
                .find(|r| {
                    r.approach == "spawn_blocking"
                        && r.num_databases == num_dbs
                        && r.write_delay_ms == write_delay
                })
                .map(|r| r.p99_read_latency_ms)
                .unwrap_or(0.0);
            let tokio = results
                .iter()
                .find(|r| {
                    r.approach == "tokio_rusqlite"
                        && r.num_databases == num_dbs
                        && r.write_delay_ms == write_delay
                })
                .map(|r| r.p99_read_latency_ms)
                .unwrap_or(0.0);

            writeln!(
                file,
                "| {} | {:.1} | {:.1} | {:.1} |",
                num_dbs, direct, spawn, tokio
            )?;
        }
        writeln!(file)?;
    }

    writeln!(file, "## Analysis")?;
    writeln!(file)?;
    writeln!(file, "### Why Direct Blocking Shows High Read Throughput")?;
    writeln!(file)?;
    writeln!(file, "When direct blocking has a **low write completion ratio** (e.g., 2% of expected writes),")?;
    writeln!(file, "it means writer tasks are starving for worker threads. The reads that DO complete")?;
    writeln!(file, "are opportunistic - they grab the lock during brief windows when writers are")?;
    writeln!(file, "waiting for thread scheduling.")?;
    writeln!(file)?;
    writeln!(file, "This is **not** better performance - it's a symptom of broken concurrency!")?;
    writeln!(file)?;
    writeln!(file, "### Recommendation")?;
    writeln!(file)?;
    writeln!(file, "- **tokio-rusqlite** provides the most predictable latency and fair scheduling")?;
    writeln!(file, "- **spawn_blocking** is a good improvement over direct blocking")?;
    writeln!(
        file,
        "- **direct_blocking** should be avoided for long-running operations"
    )?;

    Ok(())
}

fn generate_plots(output_dir: &PathBuf, results: &[BenchmarkResult], timestamp: &str) {
    // Generate ASCII plots (simple text charts)
    let ascii_path = output_dir.join(format!("ascii_{}.txt", timestamp));
    generate_ascii_plots(&ascii_path, results);

    // Generate self-contained HTML visualization
    let html_path = output_dir.join(format!("charts_{}.html", timestamp));
    generate_html_chart(&html_path, results);

    println!("    - ASCII charts: {}", ascii_path.display());
    println!("    - HTML charts:  {}", html_path.display());
    println!("    - For gnuplot:  Use templates/plot.gp with the CSV file");
}

fn generate_ascii_plots(path: &PathBuf, results: &[BenchmarkResult]) {
    let mut file = File::create(path).expect("Failed to create ASCII plots file");

    // Get unique values
    let delays: Vec<u64> = results
        .iter()
        .map(|r| r.write_delay_ms)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let num_dbs: Vec<usize> = results
        .iter()
        .map(|r| r.num_databases)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    writeln!(file, "SQLite Async Benchmark Results").ok();
    writeln!(file, "==============================\n").ok();

    for delay in &delays {
        writeln!(file, "\n{}", "=".repeat(80)).ok();
        writeln!(
            file,
            "WRITE COMPLETION RATIO - {} write delay",
            format_delay(*delay)
        )
        .ok();
        writeln!(file, "(Low % = worker thread starvation)").ok();
        writeln!(file, "{}\n", "=".repeat(80)).ok();

        writeln!(
            file,
            "{:>10} {:>15} {:>15} {:>15}",
            "DBs", "Direct", "spawn_block", "tokio-rsql"
        )
        .ok();
        writeln!(file, "{}", "-".repeat(60)).ok();

        for &dbs in &num_dbs {
            let direct = results
                .iter()
                .find(|r| {
                    r.approach == "direct_blocking"
                        && r.num_databases == dbs
                        && r.write_delay_ms == *delay
                })
                .map(|r| r.write_completion_ratio * 100.0)
                .unwrap_or(0.0);
            let spawn = results
                .iter()
                .find(|r| {
                    r.approach == "spawn_blocking"
                        && r.num_databases == dbs
                        && r.write_delay_ms == *delay
                })
                .map(|r| r.write_completion_ratio * 100.0)
                .unwrap_or(0.0);
            let tokio = results
                .iter()
                .find(|r| {
                    r.approach == "tokio_rusqlite"
                        && r.num_databases == dbs
                        && r.write_delay_ms == *delay
                })
                .map(|r| r.write_completion_ratio * 100.0)
                .unwrap_or(0.0);

            writeln!(
                file,
                "{:>10} {:>14.1}% {:>14.1}% {:>14.1}%",
                dbs, direct, spawn, tokio
            )
            .ok();
        }

        writeln!(file, "\n{}", "=".repeat(80)).ok();
        writeln!(
            file,
            "READ THROUGHPUT (req/sec) - {} write delay",
            format_delay(*delay)
        )
        .ok();
        writeln!(file, "{}\n", "=".repeat(80)).ok();

        writeln!(
            file,
            "{:>10} {:>15} {:>15} {:>15}",
            "DBs", "Direct", "spawn_block", "tokio-rsql"
        )
        .ok();
        writeln!(file, "{}", "-".repeat(60)).ok();

        for &dbs in &num_dbs {
            let direct = results
                .iter()
                .find(|r| {
                    r.approach == "direct_blocking"
                        && r.num_databases == dbs
                        && r.write_delay_ms == *delay
                })
                .map(|r| r.read_throughput_per_sec)
                .unwrap_or(0.0);
            let spawn = results
                .iter()
                .find(|r| {
                    r.approach == "spawn_blocking"
                        && r.num_databases == dbs
                        && r.write_delay_ms == *delay
                })
                .map(|r| r.read_throughput_per_sec)
                .unwrap_or(0.0);
            let tokio = results
                .iter()
                .find(|r| {
                    r.approach == "tokio_rusqlite"
                        && r.num_databases == dbs
                        && r.write_delay_ms == *delay
                })
                .map(|r| r.read_throughput_per_sec)
                .unwrap_or(0.0);

            writeln!(
                file,
                "{:>10} {:>15.1} {:>15.1} {:>15.1}",
                dbs, direct, spawn, tokio
            )
            .ok();
        }
    }
}

fn generate_html_chart(path: &PathBuf, results: &[BenchmarkResult]) {
    let mut file = File::create(path).expect("Failed to create HTML file");

    let mut delays: Vec<u64> = results.iter().map(|r| r.write_delay_ms).collect();
    delays.sort();
    delays.dedup();

    let mut dbs: Vec<usize> = results.iter().map(|r| r.num_databases).collect();
    dbs.sort();
    dbs.dedup();

    let html = r#"<!DOCTYPE html>
<html>
<head>
    <title>SQLite Async Benchmark Results</title>
    <script src="https://cdn.jsdelivr.net/npm/chart.js"></script>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; margin: 20px; background: #1a1a2e; color: #eee; }
        h1 { color: #00d4ff; }
        h2 { color: #00ff88; margin-top: 40px; }
        .chart-container { width: 100%; max-width: 900px; margin: 20px auto; background: #16213e; padding: 20px; border-radius: 10px; }
        .warning { background: #4a1515; border-left: 4px solid #ff6384; padding: 15px; margin: 20px 0; border-radius: 5px; }
        .insight { background: #153a4a; border-left: 4px solid #4bc0c0; padding: 15px; margin: 20px 0; border-radius: 5px; }
    </style>
</head>
<body>
    <h1>SQLite Async Benchmark Results</h1>
    
    <div class="warning">
        <strong>⚠️ Important:</strong> When interpreting results, look at the <strong>Write Completion Ratio</strong> first!
        A low ratio (e.g., 2%) means writer tasks are starving. High read throughput with low write completion
        is a symptom of broken concurrency, not better performance.
    </div>

    <p>Comparing three approaches to SQLite operations in async Rust:</p>
    <ul>
        <li><strong style="color: #ff6384">Direct Blocking</strong> - SQLite calls made directly (blocks workers)</li>
        <li><strong style="color: #36a2eb">spawn_blocking</strong> - Wrapped in tokio::task::spawn_blocking()</li>
        <li><strong style="color: #4bc0c0">tokio-rusqlite</strong> - Dedicated thread per database</li>
    </ul>
"#;

    write!(file, "{}", html).ok();

    // Write Completion Ratio charts (new!)
    for (idx, delay) in delays.iter().enumerate() {
        let delay_name = format_delay(*delay);
        let chart_id = format!("completion{}", idx);

        let mut direct_data = Vec::new();
        let mut spawn_data = Vec::new();
        let mut tokio_data = Vec::new();

        for num_db in &dbs {
            direct_data.push(
                results
                    .iter()
                    .find(|r| {
                        r.approach == "direct_blocking"
                            && r.num_databases == *num_db
                            && r.write_delay_ms == *delay
                    })
                    .map(|r| r.write_completion_ratio * 100.0)
                    .unwrap_or(0.0),
            );
            spawn_data.push(
                results
                    .iter()
                    .find(|r| {
                        r.approach == "spawn_blocking"
                            && r.num_databases == *num_db
                            && r.write_delay_ms == *delay
                    })
                    .map(|r| r.write_completion_ratio * 100.0)
                    .unwrap_or(0.0),
            );
            tokio_data.push(
                results
                    .iter()
                    .find(|r| {
                        r.approach == "tokio_rusqlite"
                            && r.num_databases == *num_db
                            && r.write_delay_ms == *delay
                    })
                    .map(|r| r.write_completion_ratio * 100.0)
                    .unwrap_or(0.0),
            );
        }

        writeln!(
            file,
            r#"
    <h2>Write Completion Ratio - {} Write Delay</h2>
    <div class="insight">Lower is worse! A ratio &lt;100% means writer tasks are starving for worker threads.</div>
    <div class="chart-container">
        <canvas id="{}"></canvas>
    </div>
    <script>
        new Chart(document.getElementById('{}'), {{
            type: 'bar',
            data: {{
                labels: {:?},
                datasets: [
                    {{ label: 'Direct Blocking', data: {:?}, backgroundColor: 'rgba(255, 99, 132, 0.8)', borderColor: 'rgb(255, 99, 132)', borderWidth: 1 }},
                    {{ label: 'spawn_blocking', data: {:?}, backgroundColor: 'rgba(54, 162, 235, 0.8)', borderColor: 'rgb(54, 162, 235)', borderWidth: 1 }},
                    {{ label: 'tokio-rusqlite', data: {:?}, backgroundColor: 'rgba(75, 192, 192, 0.8)', borderColor: 'rgb(75, 192, 192)', borderWidth: 1 }}
                ]
            }},
            options: {{
                responsive: true,
                plugins: {{
                    title: {{ display: true, text: 'Write Completion Ratio (%) - {} Write Delay', color: '#eee', font: {{ size: 16 }} }},
                    legend: {{ labels: {{ color: '#eee' }} }}
                }},
                scales: {{
                    x: {{ title: {{ display: true, text: 'Number of Databases', color: '#eee' }}, ticks: {{ color: '#aaa' }}, grid: {{ color: '#333' }} }},
                    y: {{ title: {{ display: true, text: 'Completion %', color: '#eee' }}, ticks: {{ color: '#aaa' }}, grid: {{ color: '#333' }}, max: 110 }}
                }}
            }}
        }});
    </script>
"#,
            delay_name,
            chart_id,
            chart_id,
            dbs,
            direct_data,
            spawn_data,
            tokio_data,
            delay_name
        )
        .ok();
    }

    // Read throughput charts
    for (idx, delay) in delays.iter().enumerate() {
        let delay_name = format_delay(*delay);
        let chart_id = format!("throughput{}", idx);

        let mut direct_data = Vec::new();
        let mut spawn_data = Vec::new();
        let mut tokio_data = Vec::new();

        for num_db in &dbs {
            direct_data.push(
                results
                    .iter()
                    .find(|r| {
                        r.approach == "direct_blocking"
                            && r.num_databases == *num_db
                            && r.write_delay_ms == *delay
                    })
                    .map(|r| r.read_throughput_per_sec)
                    .unwrap_or(0.0),
            );
            spawn_data.push(
                results
                    .iter()
                    .find(|r| {
                        r.approach == "spawn_blocking"
                            && r.num_databases == *num_db
                            && r.write_delay_ms == *delay
                    })
                    .map(|r| r.read_throughput_per_sec)
                    .unwrap_or(0.0),
            );
            tokio_data.push(
                results
                    .iter()
                    .find(|r| {
                        r.approach == "tokio_rusqlite"
                            && r.num_databases == *num_db
                            && r.write_delay_ms == *delay
                    })
                    .map(|r| r.read_throughput_per_sec)
                    .unwrap_or(0.0),
            );
        }

        writeln!(
            file,
            r#"
    <h2>Read Throughput - {} Write Delay</h2>
    <div class="chart-container">
        <canvas id="{}"></canvas>
    </div>
    <script>
        new Chart(document.getElementById('{}'), {{
            type: 'bar',
            data: {{
                labels: {:?},
                datasets: [
                    {{ label: 'Direct Blocking', data: {:?}, backgroundColor: 'rgba(255, 99, 132, 0.8)', borderColor: 'rgb(255, 99, 132)', borderWidth: 1 }},
                    {{ label: 'spawn_blocking', data: {:?}, backgroundColor: 'rgba(54, 162, 235, 0.8)', borderColor: 'rgb(54, 162, 235)', borderWidth: 1 }},
                    {{ label: 'tokio-rusqlite', data: {:?}, backgroundColor: 'rgba(75, 192, 192, 0.8)', borderColor: 'rgb(75, 192, 192)', borderWidth: 1 }}
                ]
            }},
            options: {{
                responsive: true,
                plugins: {{
                    title: {{ display: true, text: 'Read Throughput (requests/sec) - {} Write Delay', color: '#eee', font: {{ size: 16 }} }},
                    legend: {{ labels: {{ color: '#eee' }} }}
                }},
                scales: {{
                    x: {{ title: {{ display: true, text: 'Number of Databases', color: '#eee' }}, ticks: {{ color: '#aaa' }}, grid: {{ color: '#333' }} }},
                    y: {{ title: {{ display: true, text: 'Requests/sec', color: '#eee' }}, ticks: {{ color: '#aaa' }}, grid: {{ color: '#333' }} }}
                }}
            }}
        }});
    </script>
"#,
            delay_name,
            chart_id,
            chart_id,
            dbs,
            direct_data,
            spawn_data,
            tokio_data,
            delay_name
        )
        .ok();
    }

    // P99 Latency charts
    for (idx, delay) in delays.iter().enumerate() {
        let delay_name = format_delay(*delay);
        let chart_id = format!("latency{}", idx);

        let mut direct_data = Vec::new();
        let mut spawn_data = Vec::new();
        let mut tokio_data = Vec::new();

        for num_db in &dbs {
            direct_data.push(
                results
                    .iter()
                    .find(|r| {
                        r.approach == "direct_blocking"
                            && r.num_databases == *num_db
                            && r.write_delay_ms == *delay
                    })
                    .map(|r| r.p99_read_latency_ms)
                    .unwrap_or(0.0),
            );
            spawn_data.push(
                results
                    .iter()
                    .find(|r| {
                        r.approach == "spawn_blocking"
                            && r.num_databases == *num_db
                            && r.write_delay_ms == *delay
                    })
                    .map(|r| r.p99_read_latency_ms)
                    .unwrap_or(0.0),
            );
            tokio_data.push(
                results
                    .iter()
                    .find(|r| {
                        r.approach == "tokio_rusqlite"
                            && r.num_databases == *num_db
                            && r.write_delay_ms == *delay
                    })
                    .map(|r| r.p99_read_latency_ms)
                    .unwrap_or(0.0),
            );
        }

        writeln!(
            file,
            r#"
    <h2>P99 Read Latency - {} Write Delay</h2>
    <div class="chart-container">
        <canvas id="{}"></canvas>
    </div>
    <script>
        new Chart(document.getElementById('{}'), {{
            type: 'bar',
            data: {{
                labels: {:?},
                datasets: [
                    {{ label: 'Direct Blocking', data: {:?}, backgroundColor: 'rgba(255, 99, 132, 0.8)', borderColor: 'rgb(255, 99, 132)', borderWidth: 1 }},
                    {{ label: 'spawn_blocking', data: {:?}, backgroundColor: 'rgba(54, 162, 235, 0.8)', borderColor: 'rgb(54, 162, 235)', borderWidth: 1 }},
                    {{ label: 'tokio-rusqlite', data: {:?}, backgroundColor: 'rgba(75, 192, 192, 0.8)', borderColor: 'rgb(75, 192, 192)', borderWidth: 1 }}
                ]
            }},
            options: {{
                responsive: true,
                plugins: {{
                    title: {{ display: true, text: 'P99 Read Latency (ms) - {} Write Delay', color: '#eee', font: {{ size: 16 }} }},
                    legend: {{ labels: {{ color: '#eee' }} }}
                }},
                scales: {{
                    x: {{ title: {{ display: true, text: 'Number of Databases', color: '#eee' }}, ticks: {{ color: '#aaa' }}, grid: {{ color: '#333' }} }},
                    y: {{ title: {{ display: true, text: 'Latency (ms)', color: '#eee' }}, ticks: {{ color: '#aaa' }}, grid: {{ color: '#333' }} }}
                }}
            }}
        }});
    </script>
"#,
            delay_name,
            chart_id,
            chart_id,
            dbs,
            direct_data,
            spawn_data,
            tokio_data,
            delay_name
        )
        .ok();
    }

    writeln!(file, "</body></html>").ok();
}
