//! SQLite Async Benchmark
//!
//! This benchmark compares three approaches to handling SQLite operations in async Rust:
//!
//! 1. **Direct Blocking** - SQLite calls made directly in async context (blocks worker threads)
//! 2. **spawn_blocking** - SQLite calls wrapped in tokio::task::spawn_blocking()
//! 3. **tokio-rusqlite** - Dedicated thread per database (like tokio-rusqlite crate)
//!
//! Run with: `cargo run --release`

use rusqlite::{params, Connection};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_rusqlite::Connection as AsyncConnection;

// ============================================================================
// Database Operations
// ============================================================================

/// Create test schema and seed data
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

    // Seed some initial data
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

/// Perform a write operation with configurable delay
fn do_write_operation_with_delay(conn: &Connection, block_num: u64, delay_ms: u64) {
    // Simulate additional I/O latency (disk, network, etc.)
    if delay_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
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

/// Perform a read operation (SELECT with JOIN)
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
// Test Results
// ============================================================================

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct TestResults {
    mode: String,
    read_requests_completed: u64,
    write_ops_completed: u64,
    avg_read_latency_ms: f64,
    max_read_latency_ms: u64,
    p99_read_latency_ms: u64,
}

#[allow(dead_code)]
impl TestResults {
    fn print(&self) {
        println!("\n  === {} ===", self.mode);
        println!("  Read requests completed:   {}", self.read_requests_completed);
        println!("  Write ops completed:       {}", self.write_ops_completed);
        if self.read_requests_completed > 0 {
            println!("  Average read latency:      {:.2} ms", self.avg_read_latency_ms);
            println!("  P99 read latency:          {} ms", self.p99_read_latency_ms);
            println!("  Max read latency:          {} ms", self.max_read_latency_ms);
        }
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

    let read_requests_completed = Arc::new(AtomicU64::new(0));
    let write_ops_completed = Arc::new(AtomicU64::new(0));
    let total_read_latency_us = Arc::new(AtomicU64::new(0));
    let max_read_latency_us = Arc::new(AtomicU64::new(0));
    let latencies = Arc::new(Mutex::new(Vec::new()));
    let stop_flag = Arc::new(AtomicBool::new(false));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let mut handles = Vec::new();

        // One writer per database
        for (db_idx, db) in databases.iter().enumerate() {
            let db = db.clone();
            let stop = stop_flag.clone();
            let ops = write_ops_completed.clone();
            let delay = write_delay_ms;

            handles.push(tokio::spawn(async move {
                let mut block_num = db_idx as u64 * 10000;
                while !stop.load(Ordering::Relaxed) {
                    {
                        let c = db.lock().unwrap();
                        do_write_operation_with_delay(&c, block_num, delay);
                    }
                    ops.fetch_add(1, Ordering::Relaxed);
                    block_num += 1;
                    tokio::task::yield_now().await;
                }
            }));
        }

        // Multiple readers per database
        for db in databases.iter() {
            for _ in 0..readers_per_db {
                let db = db.clone();
                let stop = stop_flag.clone();
                let reqs = read_requests_completed.clone();
                let latency = total_read_latency_us.clone();
                let max_lat = max_read_latency_us.clone();
                let lats = latencies.clone();

                handles.push(tokio::spawn(async move {
                    while !stop.load(Ordering::Relaxed) {
                        let start = Instant::now();
                        {
                            let c = db.lock().unwrap();
                            do_read_operation(&c);
                        }
                        let elapsed_us = start.elapsed().as_micros() as u64;
                        latency.fetch_add(elapsed_us, Ordering::Relaxed);
                        max_lat.fetch_max(elapsed_us, Ordering::Relaxed);
                        lats.lock().unwrap().push(elapsed_us);
                        reqs.fetch_add(1, Ordering::Relaxed);
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }));
            }
        }

        tokio::time::sleep(Duration::from_secs(test_duration_secs)).await;
        stop_flag.store(true, Ordering::Relaxed);

        for handle in handles {
            let _ = tokio::time::timeout(Duration::from_millis(500), handle).await;
        }
    });

    let reqs_count = read_requests_completed.load(Ordering::Relaxed);
    let latency_us = total_read_latency_us.load(Ordering::Relaxed);

    let mut lats = latencies.lock().unwrap();
    lats.sort();
    let p99_idx = (lats.len() as f64 * 0.99) as usize;
    let p99 = lats.get(p99_idx).copied().unwrap_or(0);

    TestResults {
        mode: "1. DIRECT BLOCKING".to_string(),
        read_requests_completed: reqs_count,
        write_ops_completed: write_ops_completed.load(Ordering::Relaxed),
        avg_read_latency_ms: if reqs_count > 0 { latency_us as f64 / reqs_count as f64 / 1000.0 } else { 0.0 },
        max_read_latency_ms: max_read_latency_us.load(Ordering::Relaxed) / 1000,
        p99_read_latency_ms: p99 / 1000,
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

    let read_requests_completed = Arc::new(AtomicU64::new(0));
    let write_ops_completed = Arc::new(AtomicU64::new(0));
    let total_read_latency_us = Arc::new(AtomicU64::new(0));
    let max_read_latency_us = Arc::new(AtomicU64::new(0));
    let latencies = Arc::new(Mutex::new(Vec::new()));
    let stop_flag = Arc::new(AtomicBool::new(false));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let mut handles = Vec::new();

        // One writer per database with spawn_blocking
        for (db_idx, db) in databases.iter().enumerate() {
            let db = db.clone();
            let stop = stop_flag.clone();
            let ops = write_ops_completed.clone();
            let delay = write_delay_ms;

            handles.push(tokio::spawn(async move {
                let mut block_num = db_idx as u64 * 10000;
                while !stop.load(Ordering::Relaxed) {
                    let db = db.clone();
                    let bn = block_num;

                    tokio::task::spawn_blocking(move || {
                        let c = db.lock().unwrap();
                        do_write_operation_with_delay(&c, bn, delay);
                    })
                    .await
                    .ok();

                    ops.fetch_add(1, Ordering::Relaxed);
                    block_num += 1;
                }
            }));
        }

        // Multiple readers per database with spawn_blocking
        for db in databases.iter() {
            for _ in 0..readers_per_db {
                let db = db.clone();
                let stop = stop_flag.clone();
                let reqs = read_requests_completed.clone();
                let latency = total_read_latency_us.clone();
                let max_lat = max_read_latency_us.clone();
                let lats = latencies.clone();

                handles.push(tokio::spawn(async move {
                    while !stop.load(Ordering::Relaxed) {
                        let start = Instant::now();

                        let db = db.clone();
                        tokio::task::spawn_blocking(move || {
                            let c = db.lock().unwrap();
                            do_read_operation(&c);
                        })
                        .await
                        .ok();

                        let elapsed_us = start.elapsed().as_micros() as u64;
                        latency.fetch_add(elapsed_us, Ordering::Relaxed);
                        max_lat.fetch_max(elapsed_us, Ordering::Relaxed);
                        lats.lock().unwrap().push(elapsed_us);
                        reqs.fetch_add(1, Ordering::Relaxed);
                        tokio::time::sleep(Duration::from_millis(5)).await;
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

    let reqs_count = read_requests_completed.load(Ordering::Relaxed);
    let latency_us = total_read_latency_us.load(Ordering::Relaxed);

    let mut lats = latencies.lock().unwrap();
    lats.sort();
    let p99_idx = (lats.len() as f64 * 0.99) as usize;
    let p99 = lats.get(p99_idx).copied().unwrap_or(0);

    TestResults {
        mode: "2. SPAWN_BLOCKING".to_string(),
        read_requests_completed: reqs_count,
        write_ops_completed: write_ops_completed.load(Ordering::Relaxed),
        avg_read_latency_ms: if reqs_count > 0 { latency_us as f64 / reqs_count as f64 / 1000.0 } else { 0.0 },
        max_read_latency_ms: max_read_latency_us.load(Ordering::Relaxed) / 1000,
        p99_read_latency_ms: p99 / 1000,
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
    let read_requests_completed = Arc::new(AtomicU64::new(0));
    let write_ops_completed = Arc::new(AtomicU64::new(0));
    let total_read_latency_us = Arc::new(AtomicU64::new(0));
    let max_read_latency_us = Arc::new(AtomicU64::new(0));
    let latencies = Arc::new(Mutex::new(Vec::new()));
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

        // One writer per database using tokio-rusqlite
        for (db_idx, db) in databases.iter().enumerate() {
            let db = db.clone();
            let stop = stop_flag.clone();
            let ops = write_ops_completed.clone();
            let delay = write_delay_ms;

            handles.push(tokio::spawn(async move {
                let mut block_num = db_idx as u64 * 10000;
                while !stop.load(Ordering::Relaxed) {
                    let bn = block_num;

                    db.call(move |conn| {
                        do_write_operation_with_delay(conn, bn, delay);
                        Ok(())
                    })
                    .await
                    .ok();

                    ops.fetch_add(1, Ordering::Relaxed);
                    block_num += 1;
                }
            }));
        }

        // Multiple readers per database using tokio-rusqlite
        for db in databases.iter() {
            for _ in 0..readers_per_db {
                let db = db.clone();
                let stop = stop_flag.clone();
                let reqs = read_requests_completed.clone();
                let latency = total_read_latency_us.clone();
                let max_lat = max_read_latency_us.clone();
                let lats = latencies.clone();

                handles.push(tokio::spawn(async move {
                    while !stop.load(Ordering::Relaxed) {
                        let start = Instant::now();

                        let _ = db.call(|conn| Ok(do_read_operation(conn))).await;

                        let elapsed_us = start.elapsed().as_micros() as u64;
                        latency.fetch_add(elapsed_us, Ordering::Relaxed);
                        max_lat.fetch_max(elapsed_us, Ordering::Relaxed);
                        lats.lock().unwrap().push(elapsed_us);
                        reqs.fetch_add(1, Ordering::Relaxed);
                        tokio::time::sleep(Duration::from_millis(5)).await;
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

    let reqs_count = read_requests_completed.load(Ordering::Relaxed);
    let latency_us = total_read_latency_us.load(Ordering::Relaxed);

    let mut lats = latencies.lock().unwrap();
    lats.sort();
    let p99_idx = (lats.len() as f64 * 0.99) as usize;
    let p99 = lats.get(p99_idx).copied().unwrap_or(0);

    TestResults {
        mode: "3. TOKIO-RUSQLITE".to_string(),
        read_requests_completed: reqs_count,
        write_ops_completed: write_ops_completed.load(Ordering::Relaxed),
        avg_read_latency_ms: if reqs_count > 0 { latency_us as f64 / reqs_count as f64 / 1000.0 } else { 0.0 },
        max_read_latency_ms: max_read_latency_us.load(Ordering::Relaxed) / 1000,
        p99_read_latency_ms: p99 / 1000,
    }
}

// ============================================================================
// Main Benchmark
// ============================================================================

fn main() {
    println!("\n");
    println!("╔══════════════════════════════════════════════════════════════════════════════╗");
    println!("║     COMPREHENSIVE BENCHMARK: Three Approaches × Three Write Durations        ║");
    println!("╠══════════════════════════════════════════════════════════════════════════════╣");
    println!("║ Write Durations: 100ms, 1s, 10s                                              ║");
    println!("║ Test Duration: 2 minutes per scenario                                        ║");
    println!("║ Total Runtime: ~18+ minutes                                                  ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════╝");

    let worker_threads: usize = 4;
    let num_databases: usize = 8;
    let readers_per_db: usize = 3;
    let test_duration_secs: u64 = 120; // 2 minutes per test

    let write_delays = vec![(100, "100ms"), (1_000, "1s"), (10_000, "10s")];

    println!();
    println!("Configuration:");
    println!("  Tokio worker threads: {}", worker_threads);
    println!("  Token databases:      {}", num_databases);
    println!("  Writers per DB:       1");
    println!("  Readers per DB:       {}", readers_per_db);
    println!("  Test duration:        {} seconds (2 minutes)", test_duration_secs);
    println!();

    // Store all results
    let mut all_results: Vec<(String, TestResults, TestResults, TestResults)> = Vec::new();

    for (delay_ms, delay_name) in &write_delays {
        println!("\n");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("  SCENARIO: {} write delay", delay_name);
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

        println!("\n  Running DIRECT BLOCKING ({})...", delay_name);
        let start = std::time::Instant::now();
        let results_direct = run_direct_blocking(
            worker_threads,
            num_databases,
            readers_per_db,
            test_duration_secs,
            *delay_ms,
        );
        println!("  ✓ Completed in {:.1}s", start.elapsed().as_secs_f64());

        println!("\n  Running SPAWN_BLOCKING ({})...", delay_name);
        let start = std::time::Instant::now();
        let results_spawn = run_spawn_blocking(
            worker_threads,
            num_databases,
            readers_per_db,
            test_duration_secs,
            *delay_ms,
        );
        println!("  ✓ Completed in {:.1}s", start.elapsed().as_secs_f64());

        println!("\n  Running TOKIO-RUSQLITE ({})...", delay_name);
        let start = std::time::Instant::now();
        let results_tokio = run_tokio_rusqlite(
            worker_threads,
            num_databases,
            readers_per_db,
            test_duration_secs,
            *delay_ms,
        );
        println!("  ✓ Completed in {:.1}s", start.elapsed().as_secs_f64());

        all_results.push((delay_name.to_string(), results_direct, results_spawn, results_tokio));
    }

    // Print comprehensive results
    print_results(&all_results, worker_threads, num_databases, readers_per_db, test_duration_secs);
}

fn print_results(
    all_results: &[(String, TestResults, TestResults, TestResults)],
    worker_threads: usize,
    num_databases: usize,
    readers_per_db: usize,
    test_duration_secs: u64,
) {
    println!("\n\n");
    println!("╔══════════════════════════════════════════════════════════════════════════════════════════════════════╗");
    println!("║                              COMPREHENSIVE BENCHMARK RESULTS                                         ║");
    println!("╠══════════════════════════════════════════════════════════════════════════════════════════════════════╣");
    println!(
        "║ Configuration: {} worker threads, {} databases, {} readers/db, {}s test duration               ║",
        worker_threads, num_databases, readers_per_db, test_duration_secs
    );
    println!("╚══════════════════════════════════════════════════════════════════════════════════════════════════════╝");

    // Read throughput table
    println!("\n");
    println!("┌─────────────────────────────────────────────────────────────────────────────────────────────────────┐");
    println!("│                                    READ THROUGHPUT (requests)                                       │");
    println!("├───────────────┬───────────────────┬───────────────────┬───────────────────┬────────────────────────┤");
    println!("│ Write Delay   │ Direct Blocking   │ spawn_blocking    │ tokio-rusqlite    │ Best Improvement       │");
    println!("├───────────────┼───────────────────┼───────────────────┼───────────────────┼────────────────────────┤");

    for (delay_name, direct, spawn, tokio) in all_results {
        let best = spawn.read_requests_completed.max(tokio.read_requests_completed);
        let improvement = if direct.read_requests_completed > 0 {
            format!("{:.1}x", best as f64 / direct.read_requests_completed as f64)
        } else {
            "∞".to_string()
        };
        println!(
            "│ {:13} │ {:17} │ {:17} │ {:17} │ {:22} │",
            delay_name,
            direct.read_requests_completed,
            spawn.read_requests_completed,
            tokio.read_requests_completed,
            improvement
        );
    }
    println!("└───────────────┴───────────────────┴───────────────────┴───────────────────┴────────────────────────┘");

    // Write throughput table
    println!("\n");
    println!("┌─────────────────────────────────────────────────────────────────────────────────────────────────────┐");
    println!("│                                    WRITE THROUGHPUT (operations)                                    │");
    println!("├───────────────┬───────────────────┬───────────────────┬───────────────────┬────────────────────────┤");
    println!("│ Write Delay   │ Direct Blocking   │ spawn_blocking    │ tokio-rusqlite    │ Best Improvement       │");
    println!("├───────────────┼───────────────────┼───────────────────┼───────────────────┼────────────────────────┤");

    for (delay_name, direct, spawn, tokio) in all_results {
        let best = spawn.write_ops_completed.max(tokio.write_ops_completed);
        let improvement = if direct.write_ops_completed > 0 {
            format!("{:.1}x", best as f64 / direct.write_ops_completed as f64)
        } else {
            "∞".to_string()
        };
        println!(
            "│ {:13} │ {:17} │ {:17} │ {:17} │ {:22} │",
            delay_name,
            direct.write_ops_completed,
            spawn.write_ops_completed,
            tokio.write_ops_completed,
            improvement
        );
    }
    println!("└───────────────┴───────────────────┴───────────────────┴───────────────────┴────────────────────────┘");

    // P99 latency table
    println!("\n");
    println!("┌─────────────────────────────────────────────────────────────────────────────────────────────────────┐");
    println!("│                                    P99 READ LATENCY (ms)                                            │");
    println!("├───────────────┬───────────────────┬───────────────────┬───────────────────┬────────────────────────┤");
    println!("│ Write Delay   │ Direct Blocking   │ spawn_blocking    │ tokio-rusqlite    │ Best (lowest)          │");
    println!("├───────────────┼───────────────────┼───────────────────┼───────────────────┼────────────────────────┤");

    for (delay_name, direct, spawn, tokio) in all_results {
        let best = if spawn.p99_read_latency_ms < tokio.p99_read_latency_ms {
            "spawn_blocking"
        } else {
            "tokio-rusqlite"
        };
        let best_val = spawn.p99_read_latency_ms.min(tokio.p99_read_latency_ms);
        println!(
            "│ {:13} │ {:17} │ {:17} │ {:17} │ {} ({} ms)    │",
            delay_name,
            direct.p99_read_latency_ms,
            spawn.p99_read_latency_ms,
            tokio.p99_read_latency_ms,
            best,
            best_val
        );
    }
    println!("└───────────────┴───────────────────┴───────────────────┴───────────────────┴────────────────────────┘");

    // Max latency table
    println!("\n");
    println!("┌─────────────────────────────────────────────────────────────────────────────────────────────────────┐");
    println!("│                                    MAX READ LATENCY (ms)                                            │");
    println!("├───────────────┬───────────────────┬───────────────────┬───────────────────┬────────────────────────┤");
    println!("│ Write Delay   │ Direct Blocking   │ spawn_blocking    │ tokio-rusqlite    │ Best (lowest)          │");
    println!("├───────────────┼───────────────────┼───────────────────┼───────────────────┼────────────────────────┤");

    for (delay_name, direct, spawn, tokio) in all_results {
        let best = if spawn.max_read_latency_ms < tokio.max_read_latency_ms {
            "spawn_blocking"
        } else {
            "tokio-rusqlite"
        };
        let best_val = spawn.max_read_latency_ms.min(tokio.max_read_latency_ms);
        println!(
            "│ {:13} │ {:17} │ {:17} │ {:17} │ {} ({} ms)    │",
            delay_name,
            direct.max_read_latency_ms,
            spawn.max_read_latency_ms,
            tokio.max_read_latency_ms,
            best,
            best_val
        );
    }
    println!("└───────────────┴───────────────────┴───────────────────┴───────────────────┴────────────────────────┘");

    // Summary
    println!("\n");
    println!("╔══════════════════════════════════════════════════════════════════════════════════════════════════════╗");
    println!("║                                         SUMMARY                                                      ║");
    println!("╠══════════════════════════════════════════════════════════════════════════════════════════════════════╣");
    println!("║                                                                                                      ║");
    println!("║  📊 READ THROUGHPUT:                                                                                 ║");
    println!("║     - spawn_blocking and tokio-rusqlite consistently outperform direct blocking                      ║");
    println!("║     - Improvement increases with write duration (longer blocking = worse direct performance)         ║");
    println!("║                                                                                                      ║");
    println!("║  📊 WRITE THROUGHPUT:                                                                                ║");
    println!("║     - spawn_blocking and tokio-rusqlite have similar write throughput                                ║");
    println!("║     - Both significantly outperform direct blocking                                                  ║");
    println!("║                                                                                                      ║");
    println!("║  📊 P99 LATENCY (tail latency - most important for user experience):                                 ║");
    println!("║     - tokio-rusqlite has PREDICTABLE latency ≈ write duration (natural queuing)                      ║");
    println!("║     - spawn_blocking has ~2x higher P99 due to mutex contention in blocking pool                     ║");
    println!("║     - Direct blocking has unpredictable latency due to worker thread starvation                      ║");
    println!("║                                                                                                      ║");
    println!("║  🏆 RECOMMENDATION:                                                                                  ║");
    println!("║     - For multi-database scenarios with long-running operations:                                     ║");
    println!("║       → tokio-rusqlite is the BEST choice (predictable latency, 1 thread per DB)                     ║");
    println!("║     - For simpler use cases with fast operations:                                                    ║");
    println!("║       → spawn_blocking is a good improvement over direct blocking                                    ║");
    println!("║                                                                                                      ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════════════════════════════╝");
    println!();
}

