# Gnuplot script for SQLite Async Benchmark
# 
# Usage:
#   gnuplot -e "datafile='path/to/data.csv'" plot.gp
#
# Or edit the datafile variable below and run:
#   gnuplot plot.gp

# Default data file (override with -e "datafile='...'" on command line)
if (!exists("datafile")) datafile = "benchmark.csv"
if (!exists("outdir")) outdir = "."

# Output settings
set terminal pngcairo size 1400,1000 enhanced font 'Helvetica,12'
set output outdir."/benchmark_charts.png"

set datafile separator ','

# Colors
direct_color = "#ff6384"
spawn_color = "#36a2eb"
tokio_color = "#4bc0c0"

# Common settings
set style data histogram
set style histogram cluster gap 1
set style fill solid 0.8 border -1
set boxwidth 0.9
set key outside right top
set grid ytics

# Multi-plot layout
set multiplot layout 2,2 title "SQLite Async Benchmark Results\n".datafile font ',14'

# ============================================================================
# Chart 1: Read Throughput - Bar Chart
# ============================================================================
set title 'Read Throughput by Configuration'
set xlabel 'Configuration (DBs @ Delay)'
set ylabel 'Requests/sec'
set xtics rotate by -45

# We need to pre-process the CSV since gnuplot histograms work differently
# For now, create a simple line plot version

# ============================================================================
# Chart 2: Read Throughput vs Database Count (Line)
# ============================================================================
set title 'Read Throughput vs Number of Databases'
set xlabel 'Number of Databases'
set ylabel 'Requests/sec'
set xtics autofreq rotate by 0

# Parse CSV: approach,num_databases,write_delay_ms,...,read_throughput_per_sec
# Column 12 is read_throughput_per_sec

plot datafile using ($1 eq 'direct_blocking' ? $2 : 1/0):12 \
         every ::1 with linespoints title 'Direct Blocking' lc rgb direct_color pt 7 ps 1.5, \
     datafile using ($1 eq 'spawn_blocking' ? $2 : 1/0):12 \
         every ::1 with linespoints title 'spawn\_blocking' lc rgb spawn_color pt 5 ps 1.5, \
     datafile using ($1 eq 'tokio_rusqlite' ? $2 : 1/0):12 \
         every ::1 with linespoints title 'tokio-rusqlite' lc rgb tokio_color pt 9 ps 1.5

# ============================================================================
# Chart 3: P99 Latency
# ============================================================================
set title 'P99 Read Latency'
set xlabel 'Number of Databases'
set ylabel 'Latency (ms)'

# Column 10 is p99_read_latency_ms
plot datafile using ($1 eq 'direct_blocking' ? $2 : 1/0):10 \
         every ::1 with linespoints title 'Direct Blocking' lc rgb direct_color pt 7 ps 1.5, \
     datafile using ($1 eq 'spawn_blocking' ? $2 : 1/0):10 \
         every ::1 with linespoints title 'spawn\_blocking' lc rgb spawn_color pt 5 ps 1.5, \
     datafile using ($1 eq 'tokio_rusqlite' ? $2 : 1/0):10 \
         every ::1 with linespoints title 'tokio-rusqlite' lc rgb tokio_color pt 9 ps 1.5

# ============================================================================
# Chart 4: Write Throughput
# ============================================================================
set title 'Write Throughput'
set xlabel 'Number of Databases'
set ylabel 'Writes/sec'

# Column 13 is write_throughput_per_sec
plot datafile using ($1 eq 'direct_blocking' ? $2 : 1/0):13 \
         every ::1 with linespoints title 'Direct Blocking' lc rgb direct_color pt 7 ps 1.5, \
     datafile using ($1 eq 'spawn_blocking' ? $2 : 1/0):13 \
         every ::1 with linespoints title 'spawn\_blocking' lc rgb spawn_color pt 5 ps 1.5, \
     datafile using ($1 eq 'tokio_rusqlite' ? $2 : 1/0):13 \
         every ::1 with linespoints title 'tokio-rusqlite' lc rgb tokio_color pt 9 ps 1.5

unset multiplot

print "Generated: ".outdir."/benchmark_charts.png"

