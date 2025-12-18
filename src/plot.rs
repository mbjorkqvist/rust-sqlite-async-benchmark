//! CSV Parser and Plot Generator
//!
//! This tool parses benchmark CSV results and generates visualizations.
//!
//! Usage:
//!   cargo run --release --bin plot -- --csv results/benchmark_YYYYMMDD_HHMMSS.csv

use clap::Parser;
use csv::Reader;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(author, version, about = "Parse benchmark CSV and generate plots")]
struct Args {
    /// Path to the CSV file
    #[arg(short, long)]
    csv: PathBuf,

    /// Output directory for generated files
    #[arg(short, long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // Fields are required for CSV deserialization even if not directly accessed
struct BenchmarkResult {
    approach: String,
    num_databases: usize,
    write_delay_ms: u64,
    test_duration_secs: u64,
    worker_threads: usize,
    readers_per_db: usize,
    read_requests_completed: u64,
    write_ops_completed: u64,
    avg_read_latency_ms: f64,
    p99_read_latency_ms: u64,
    max_read_latency_ms: u64,
    read_throughput_per_sec: f64,
    write_throughput_per_sec: f64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Read CSV
    let mut reader = Reader::from_path(&args.csv)?;
    let results: Vec<BenchmarkResult> = reader.deserialize().filter_map(|r| r.ok()).collect();

    if results.is_empty() {
        eprintln!("No results found in CSV file");
        return Ok(());
    }

    // Determine output directory
    let output_dir = args.output.unwrap_or_else(|| {
        args.csv
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    });
    std::fs::create_dir_all(&output_dir)?;

    println!("Loaded {} results from {}", results.len(), args.csv.display());
    println!("Output directory: {}", output_dir.display());

    // Extract unique values
    let delays: Vec<u64> = results
        .iter()
        .map(|r| r.write_delay_ms)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let dbs: Vec<usize> = {
        let mut v: Vec<_> = results
            .iter()
            .map(|r| r.num_databases)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        v.sort();
        v
    };

    // Print summary table
    print_summary_tables(&results, &delays, &dbs);

    // Generate HTML charts
    let html_path = output_dir.join("charts_regenerated.html");
    generate_html_chart(&html_path, &results)?;

    // Generate ASCII bar charts  
    let ascii_path = output_dir.join("ascii_regenerated.txt");
    generate_ascii_charts(&ascii_path, &results, &delays, &dbs)?;

    println!("\n✅ Generated files:");
    println!("   - {}", html_path.display());
    println!("   - {}", ascii_path.display());
    println!();
    println!("   For gnuplot, use the template with your CSV:");
    println!("   gnuplot -e \"datafile='{}'; outdir='{}'\" templates/plot.gp", 
             args.csv.display(), output_dir.display());

    Ok(())
}

fn format_delay(delay_ms: u64) -> String {
    if delay_ms >= 1000 {
        format!("{}s", delay_ms / 1000)
    } else {
        format!("{}ms", delay_ms)
    }
}

fn print_summary_tables(results: &[BenchmarkResult], delays: &[u64], dbs: &[usize]) {
    println!("\n╔══════════════════════════════════════════════════════════════════════════════╗");
    println!("║                         BENCHMARK SUMMARY                                    ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════╝");

    for delay in delays {
        println!("\n━━━ Read Throughput (req/sec) - {} write delay ━━━\n", format_delay(*delay));
        println!(
            "{:>10} {:>15} {:>15} {:>15} {:>12}",
            "DBs", "Direct", "spawn_block", "tokio-rsql", "Best Gain"
        );
        println!("{}", "─".repeat(70));

        for num_db in dbs {
            let direct = get_value(results, "direct_blocking", *num_db, *delay, |r| {
                r.read_throughput_per_sec
            });
            let spawn = get_value(results, "spawn_blocking", *num_db, *delay, |r| {
                r.read_throughput_per_sec
            });
            let tokio = get_value(results, "tokio_rusqlite", *num_db, *delay, |r| {
                r.read_throughput_per_sec
            });

            let best = spawn.max(tokio);
            let gain = if direct > 0.0 {
                format!("{:.1}x", best / direct)
            } else {
                "∞".to_string()
            };

            println!(
                "{:>10} {:>15.1} {:>15.1} {:>15.1} {:>12}",
                num_db, direct, spawn, tokio, gain
            );
        }
    }

    for delay in delays {
        println!(
            "\n━━━ P99 Read Latency (ms) - {} write delay ━━━\n",
            format_delay(*delay)
        );
        println!(
            "{:>10} {:>15} {:>15} {:>15} {:>12}",
            "DBs", "Direct", "spawn_block", "tokio-rsql", "Best"
        );
        println!("{}", "─".repeat(70));

        for num_db in dbs {
            let direct = get_value_u64(results, "direct_blocking", *num_db, *delay, |r| {
                r.p99_read_latency_ms
            });
            let spawn = get_value_u64(results, "spawn_blocking", *num_db, *delay, |r| {
                r.p99_read_latency_ms
            });
            let tokio = get_value_u64(results, "tokio_rusqlite", *num_db, *delay, |r| {
                r.p99_read_latency_ms
            });

            let best_val = direct.min(spawn).min(tokio);
            let best = if best_val == tokio {
                "tokio-rsql"
            } else if best_val == spawn {
                "spawn_block"
            } else {
                "direct"
            };

            println!(
                "{:>10} {:>15} {:>15} {:>15} {:>12}",
                num_db, direct, spawn, tokio, best
            );
        }
    }
}

fn get_value<F>(
    results: &[BenchmarkResult],
    approach: &str,
    num_db: usize,
    delay: u64,
    f: F,
) -> f64
where
    F: Fn(&BenchmarkResult) -> f64,
{
    results
        .iter()
        .find(|r| r.approach == approach && r.num_databases == num_db && r.write_delay_ms == delay)
        .map(&f)
        .unwrap_or(0.0)
}

fn get_value_u64<F>(
    results: &[BenchmarkResult],
    approach: &str,
    num_db: usize,
    delay: u64,
    f: F,
) -> u64
where
    F: Fn(&BenchmarkResult) -> u64,
{
    results
        .iter()
        .find(|r| r.approach == approach && r.num_databases == num_db && r.write_delay_ms == delay)
        .map(&f)
        .unwrap_or(0)
}

fn generate_html_chart(
    path: &PathBuf,
    results: &[BenchmarkResult],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = File::create(path)?;

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
        canvas { background: #16213e; }
    </style>
</head>
<body>
    <h1>SQLite Async Benchmark Results (Regenerated)</h1>
    
    <p>Comparing three approaches to SQLite operations in async Rust:</p>
    <ul>
        <li><strong style="color: #ff6384">Direct Blocking</strong> - SQLite calls made directly (blocks workers)</li>
        <li><strong style="color: #36a2eb">spawn_blocking</strong> - Wrapped in tokio::task::spawn_blocking()</li>
        <li><strong style="color: #4bc0c0">tokio-rusqlite</strong> - Dedicated thread per database</li>
    </ul>
"#;

    write!(file, "{}", html)?;

    for (idx, delay) in delays.iter().enumerate() {
        let delay_name = format_delay(*delay);

        let mut direct_data = Vec::new();
        let mut spawn_data = Vec::new();
        let mut tokio_data = Vec::new();

        for num_db in &dbs {
            direct_data.push(get_value(results, "direct_blocking", *num_db, *delay, |r| {
                r.read_throughput_per_sec
            }));
            spawn_data.push(get_value(results, "spawn_blocking", *num_db, *delay, |r| {
                r.read_throughput_per_sec
            }));
            tokio_data.push(get_value(results, "tokio_rusqlite", *num_db, *delay, |r| {
                r.read_throughput_per_sec
            }));
        }

        writeln!(
            file,
            r#"
    <h2>Read Throughput - {} Write Delay</h2>
    <div class="chart-container">
        <canvas id="chart{}"></canvas>
    </div>
    <script>
        new Chart(document.getElementById('chart{}'), {{
            type: 'bar',
            data: {{
                labels: {:?},
                datasets: [
                    {{
                        label: 'Direct Blocking',
                        data: {:?},
                        backgroundColor: 'rgba(255, 99, 132, 0.8)',
                        borderColor: 'rgb(255, 99, 132)',
                        borderWidth: 1
                    }},
                    {{
                        label: 'spawn_blocking',
                        data: {:?},
                        backgroundColor: 'rgba(54, 162, 235, 0.8)',
                        borderColor: 'rgb(54, 162, 235)',
                        borderWidth: 1
                    }},
                    {{
                        label: 'tokio-rusqlite',
                        data: {:?},
                        backgroundColor: 'rgba(75, 192, 192, 0.8)',
                        borderColor: 'rgb(75, 192, 192)',
                        borderWidth: 1
                    }}
                ]
            }},
            options: {{
                responsive: true,
                plugins: {{
                    title: {{
                        display: true,
                        text: 'Read Throughput (requests/sec) - {} Write Delay',
                        color: '#eee',
                        font: {{ size: 16 }}
                    }},
                    legend: {{
                        labels: {{ color: '#eee' }}
                    }}
                }},
                scales: {{
                    x: {{
                        title: {{ display: true, text: 'Number of Databases', color: '#eee' }},
                        ticks: {{ color: '#aaa' }},
                        grid: {{ color: '#333' }}
                    }},
                    y: {{
                        title: {{ display: true, text: 'Requests/sec', color: '#eee' }},
                        ticks: {{ color: '#aaa' }},
                        grid: {{ color: '#333' }}
                    }}
                }}
            }}
        }});
    </script>
"#,
            delay_name, idx, idx, dbs, direct_data, spawn_data, tokio_data, delay_name
        )?;
    }

    // P99 latency charts
    for (idx, delay) in delays.iter().enumerate() {
        let delay_name = format_delay(*delay);
        let chart_idx = delays.len() + idx;

        let mut direct_data = Vec::new();
        let mut spawn_data = Vec::new();
        let mut tokio_data = Vec::new();

        for num_db in &dbs {
            direct_data.push(get_value_u64(
                results,
                "direct_blocking",
                *num_db,
                *delay,
                |r| r.p99_read_latency_ms,
            ));
            spawn_data.push(get_value_u64(
                results,
                "spawn_blocking",
                *num_db,
                *delay,
                |r| r.p99_read_latency_ms,
            ));
            tokio_data.push(get_value_u64(
                results,
                "tokio_rusqlite",
                *num_db,
                *delay,
                |r| r.p99_read_latency_ms,
            ));
        }

        writeln!(
            file,
            r#"
    <h2>P99 Read Latency - {} Write Delay</h2>
    <div class="chart-container">
        <canvas id="chart{}"></canvas>
    </div>
    <script>
        new Chart(document.getElementById('chart{}'), {{
            type: 'bar',
            data: {{
                labels: {:?},
                datasets: [
                    {{
                        label: 'Direct Blocking',
                        data: {:?},
                        backgroundColor: 'rgba(255, 99, 132, 0.8)',
                        borderColor: 'rgb(255, 99, 132)',
                        borderWidth: 1
                    }},
                    {{
                        label: 'spawn_blocking',
                        data: {:?},
                        backgroundColor: 'rgba(54, 162, 235, 0.8)',
                        borderColor: 'rgb(54, 162, 235)',
                        borderWidth: 1
                    }},
                    {{
                        label: 'tokio-rusqlite',
                        data: {:?},
                        backgroundColor: 'rgba(75, 192, 192, 0.8)',
                        borderColor: 'rgb(75, 192, 192)',
                        borderWidth: 1
                    }}
                ]
            }},
            options: {{
                responsive: true,
                plugins: {{
                    title: {{
                        display: true,
                        text: 'P99 Read Latency (ms) - {} Write Delay',
                        color: '#eee',
                        font: {{ size: 16 }}
                    }},
                    legend: {{
                        labels: {{ color: '#eee' }}
                    }}
                }},
                scales: {{
                    x: {{
                        title: {{ display: true, text: 'Number of Databases', color: '#eee' }},
                        ticks: {{ color: '#aaa' }},
                        grid: {{ color: '#333' }}
                    }},
                    y: {{
                        title: {{ display: true, text: 'Latency (ms)', color: '#eee' }},
                        ticks: {{ color: '#aaa' }},
                        grid: {{ color: '#333' }}
                    }}
                }}
            }}
        }});
    </script>
"#,
            delay_name, chart_idx, chart_idx, dbs, direct_data, spawn_data, tokio_data, delay_name
        )?;
    }

    writeln!(file, "</body></html>")?;

    Ok(())
}

fn generate_ascii_charts(
    path: &PathBuf,
    results: &[BenchmarkResult],
    delays: &[u64],
    dbs: &[usize],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = File::create(path)?;

    writeln!(file, "SQLite Async Benchmark - ASCII Charts")?;
    writeln!(file, "======================================")?;

    for delay in delays {
        writeln!(file, "\n\n=== Read Throughput (req/sec) - {} ===\n", format_delay(*delay))?;
        
        // Find max value for scaling
        let max_val: f64 = dbs
            .iter()
            .flat_map(|num_db| {
                vec![
                    get_value(results, "direct_blocking", *num_db, *delay, |r| r.read_throughput_per_sec),
                    get_value(results, "spawn_blocking", *num_db, *delay, |r| r.read_throughput_per_sec),
                    get_value(results, "tokio_rusqlite", *num_db, *delay, |r| r.read_throughput_per_sec),
                ]
            })
            .fold(0.0_f64, |a, b| a.max(b));

        let bar_width = 50;

        for num_db in dbs {
            let direct = get_value(results, "direct_blocking", *num_db, *delay, |r| r.read_throughput_per_sec);
            let spawn = get_value(results, "spawn_blocking", *num_db, *delay, |r| r.read_throughput_per_sec);
            let tokio = get_value(results, "tokio_rusqlite", *num_db, *delay, |r| r.read_throughput_per_sec);

            writeln!(file, "{}  databases:", num_db)?;
            
            let direct_bar = (direct / max_val * bar_width as f64) as usize;
            let spawn_bar = (spawn / max_val * bar_width as f64) as usize;
            let tokio_bar = (tokio / max_val * bar_width as f64) as usize;

            writeln!(file, "  Direct:  |{}| {:.1}", "█".repeat(direct_bar), direct)?;
            writeln!(file, "  Spawn:   |{}| {:.1}", "▓".repeat(spawn_bar), spawn)?;
            writeln!(file, "  Tokio:   |{}| {:.1}", "░".repeat(tokio_bar), tokio)?;
            writeln!(file)?;
        }
    }

    Ok(())
}
