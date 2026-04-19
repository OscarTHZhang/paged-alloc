#!/usr/bin/env bash
#
# Run paged-alloc benchmarks and print a formatted report with scaling
# factors, per-thread efficiency, and source / prewarm comparisons.
#
# Usage:
#   scripts/bench.sh                          # run all benches, print report
#   scripts/bench.sh --skip-run               # just re-parse existing results
#   scripts/bench.sh --quick                  # shorter measurement time
#   scripts/bench.sh --filter concurrent      # pass a filter to cargo bench
#   MEASUREMENT_TIME=5 scripts/bench.sh       # custom measurement time
#
# Reads criterion's target/criterion/**/new/estimates.json after the run
# and pretty-prints the mean point estimates, grouped by bench area.
#
# Requires: cargo, python3.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# ----------------------------------------------------------------- args
SKIP_RUN=0
WARMUP_TIME="${WARMUP_TIME:-1}"
MEASUREMENT_TIME="${MEASUREMENT_TIME:-3}"
FILTER=""

while [ $# -gt 0 ]; do
    case "$1" in
        --skip-run)     SKIP_RUN=1; shift ;;
        --quick)        MEASUREMENT_TIME=1; WARMUP_TIME=1; shift ;;
        --filter)       FILTER="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,17p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)              echo "unknown arg: $1" >&2; exit 1 ;;
    esac
done

command -v cargo   >/dev/null || { echo "error: cargo not found" >&2; exit 1; }
command -v python3 >/dev/null || { echo "error: python3 not found" >&2; exit 1; }

# ----------------------------------------------------------------- colors
if [ -t 1 ]; then
    BOLD=$'\e[1m'; DIM=$'\e[2m'
    GREEN=$'\e[32m'; YELLOW=$'\e[33m'; CYAN=$'\e[36m'; MAGENTA=$'\e[35m'
    RESET=$'\e[0m'
else
    BOLD=''; DIM=''; GREEN=''; YELLOW=''; CYAN=''; MAGENTA=''; RESET=''
fi

hr() { printf '%s────────────────────────────────────────────────────────────%s\n' "$DIM" "$RESET"; }
title() { printf '\n%s %s %s\n' "$BOLD$CYAN" "$1" "$RESET"; hr; }

# ----------------------------------------------------------------- sys info
print_sys() {
    title "System"
    printf '  %-10s %s\n' "Date" "$(date '+%Y-%m-%d %H:%M:%S %Z')"
    printf '  %-10s %s\n' "Host" "$(hostname -s 2>/dev/null || echo ?)"
    printf '  %-10s %s\n' "Kernel" "$(uname -sr)"

    local kind="$(uname -s)"
    if [ "$kind" = "Darwin" ]; then
        local brand model cores p_cores e_cores
        brand=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo ?)
        model=$(sysctl -n hw.model 2>/dev/null || echo ?)
        cores=$(sysctl -n hw.physicalcpu 2>/dev/null || echo ?)
        p_cores=$(sysctl -n hw.perflevel0.physicalcpu 2>/dev/null || echo "")
        e_cores=$(sysctl -n hw.perflevel1.physicalcpu 2>/dev/null || echo "")
        printf '  %-10s %s  (%s)\n' "CPU" "$brand" "$model"
        if [ -n "$p_cores" ] && [ -n "$e_cores" ]; then
            printf '  %-10s %s%s P + %s E%s = %s total %s(heterogeneous — caps 8-thread scaling)%s\n' \
                "Cores" "$YELLOW" "$p_cores" "$e_cores" "$RESET" "$cores" "$DIM" "$RESET"
        else
            printf '  %-10s %s physical\n' "Cores" "$cores"
        fi
    elif [ "$kind" = "Linux" ]; then
        local cpu cores sockets
        cpu=$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | sed 's/.*: //' || echo ?)
        cores=$(nproc 2>/dev/null || echo ?)
        sockets=$(grep 'physical id' /proc/cpuinfo 2>/dev/null | sort -u | wc -l | tr -d ' ')
        printf '  %-10s %s\n' "CPU" "$cpu"
        printf '  %-10s %s logical, %s socket(s)\n' "Cores" "$cores" "$sockets"
    fi

    printf '  %-10s %s\n' "Rust" "$(rustc --version)"
    printf '  %-10s lto=thin codegen-units=1 %s(release)%s\n' "Profile" "$DIM" "$RESET"
}

# ----------------------------------------------------------------- run benches
run_benches() {
    title "Running benchmarks"
    printf '  %-10s %ss warmup, %ss measurement\n' "Settings" "$WARMUP_TIME" "$MEASUREMENT_TIME"
    if [ -n "$FILTER" ]; then
        printf '  %-10s %s\n' "Filter" "$FILTER"
    fi
    printf '\n'

    local args=(--bench alloc -- \
        --warm-up-time "$WARMUP_TIME" \
        --measurement-time "$MEASUREMENT_TIME")
    [ -n "$FILTER" ] && args+=("$FILTER")

    # Run the benchmark and stream compact progress. Full stdout is
    # available via `cargo bench` if anything goes wrong.
    cargo bench "${args[@]}" 2>&1 | \
        awk -v dim="$DIM" -v reset="$RESET" '
            /^Benchmarking .*: Analyzing/ {
                name = $0
                sub(/^Benchmarking /, "", name)
                sub(/: Analyzing$/, "", name)
                printf "    %s▸%s %-60s\r", dim, reset, name
                fflush()
                next
            }
            /^error/ { print }
        '
    printf '\n'
}

# ----------------------------------------------------------------- report
print_report() {
ROOT="$ROOT" BOLD="$BOLD" DIM="$DIM" GREEN="$GREEN" YELLOW="$YELLOW" \
CYAN="$CYAN" MAGENTA="$MAGENTA" RESET="$RESET" \
python3 - <<'PY'
import json
import os
import sys
from pathlib import Path

ROOT = Path(os.environ["ROOT"]).resolve()
CRIT = ROOT / "target" / "criterion"

BOLD   = os.environ.get("BOLD", "")
DIM    = os.environ.get("DIM", "")
GREEN  = os.environ.get("GREEN", "")
YELLOW = os.environ.get("YELLOW", "")
CYAN   = os.environ.get("CYAN", "")
MAG    = os.environ.get("MAGENTA", "")
RESET  = os.environ.get("RESET", "")

def load(*parts):
    """Load mean point estimate (ns) for a bench directory."""
    p = CRIT.joinpath(*parts, "new", "estimates.json")
    if not p.exists():
        return None
    with p.open() as f:
        return json.load(f)["mean"]["point_estimate"]

def fmt_ns(ns):
    if ns is None:
        return f"{DIM}   —   {RESET}"
    if ns < 1_000:
        return f"{ns:8.2f} ns"
    if ns < 1_000_000:
        return f"{ns/1e3:8.2f} µs"
    if ns < 1_000_000_000:
        return f"{ns/1e6:8.2f} ms"
    return f"{ns/1e9:8.2f}  s"

def fmt_ops(ns_per_op):
    if ns_per_op is None:
        return f"{DIM}    —    {RESET}"
    ops_per_sec = 1e9 / ns_per_op
    if ops_per_sec >= 1e6:
        return f"{ops_per_sec/1e6:7.2f} Melem/s"
    if ops_per_sec >= 1e3:
        return f"{ops_per_sec/1e3:7.2f} Kelem/s"
    return f"{ops_per_sec:9.0f} op/s"

def fmt_bytes(ns_per_op, bytes_per_op):
    if ns_per_op is None:
        return f"{DIM}    —    {RESET}"
    bps = bytes_per_op * 1e9 / ns_per_op
    for name, d in [("TiB/s", 1 << 40), ("GiB/s", 1 << 30), ("MiB/s", 1 << 20), ("KiB/s", 1 << 10)]:
        if bps >= d:
            return f"{bps/d:7.2f} {name}"
    return f"{bps:7.2f} B/s"

def title(text):
    print(f"\n{BOLD}{CYAN} {text} {RESET}")
    print(f"{DIM}────────────────────────────────────────────────────────────{RESET}")

def efficiency_color(eff):
    if eff >= 0.90:
        return GREEN
    if eff >= 0.75:
        return YELLOW
    return MAG

def colored(text, width, color):
    """Right-align `text` into `width` cells, then wrap in color. The
    width is computed on the raw (uncolored) string so the column
    lines up regardless of ANSI codes."""
    return f"{color}{text.rjust(width)}{RESET}"

# =========================================================================
# Single-threaded steady state
# =========================================================================
title("Single-threaded steady state")
print(f"  {DIM}alloc → seal → drop on a warm free list{RESET}\n")
print(f"  {'Page size':>12}  {'Time':>12}  {'Throughput':>14}")
print(f"  {DIM}{'-'*12}  {'-'*12}  {'-'*14}{RESET}")
any_ss = False
for ps in (4096, 16384, 65536):
    ns = load("steady_state_alloc_seal_drop", str(ps))
    if ns is None:
        continue
    any_ss = True
    print(f"  {ps:>10} B  {fmt_ns(ns):>12}  {fmt_bytes(ns, ps):>14}")
if not any_ss:
    print(f"  {DIM}(no data — run benches first){RESET}")

# =========================================================================
# Concurrent per-worker scaling — the headline metric
# =========================================================================
title("Concurrent scaling  (thread-per-core, one PagePool per worker)")
print(f"  {DIM}4 KiB pages, aggregate throughput across all worker threads{RESET}\n")
baseline = load("concurrent_per_worker", "1")
if baseline is None:
    print(f"  {DIM}(no data){RESET}")
else:
    print(
        f"  {'Threads':>8}  {'ns/op':>11}  {'Throughput':>15}  "
        f"{'Scaling':>10}  {'Efficiency':>12}"
    )
    print(
        f"  {DIM}{'-'*8}  {'-'*11}  {'-'*15}  {'-'*10}  {'-'*12}{RESET}"
    )
    for t in (1, 2, 3, 4, 8):
        ns = load("concurrent_per_worker", str(t))
        if ns is None:
            continue
        scaling = baseline / ns
        eff = scaling / t
        c = efficiency_color(eff)
        scl_str = colored(f"{scaling:.2f}×", 10, c)
        eff_str = colored(f"{eff*100:.1f} %", 12, c)
        print(
            f"  {t:>8}  {fmt_ns(ns):>11}  {fmt_ops(ns):>15}  "
            f"{scl_str}  {eff_str}"
        )

# =========================================================================
# Heap vs mmap — cold path
# =========================================================================
title("Source cold path  (fresh pool, one alloc — syscall / memset bound)")
print(f"  {DIM}mmap and heap do NOT share a bottleneck here{RESET}\n")
print(
    f"  {'Page size':>12}  {'Heap':>13}  {'Mmap':>13}  "
    f"{'Mmap/Heap':>11}"
)
print(
    f"  {DIM}{'-'*12}  {'-'*13}  {'-'*13}  {'-'*11}{RESET}"
)
for ps in (16384, 65536):
    h = load("source_cold_path", "heap", str(ps))
    m = load("source_cold_path", "mmap", str(ps))
    if h is not None and m is not None:
        r = m / h
        color = GREEN if r < 1 else YELLOW
        ratio = colored(f"{r:.2f}×", 11, color)
    else:
        ratio = DIM + "   —   ".rjust(11) + RESET
    print(
        f"  {ps:>10} B  {fmt_ns(h):>13}  {fmt_ns(m):>13}  {ratio}"
    )

# =========================================================================
# Source steady state — confirms the trait is free
# =========================================================================
title("Source abstraction cost  (warm free list)")
print(f"  {DIM}same workload, different PageSource — hot path never touches the source{RESET}\n")
print(f"  {'Source':<10}  {'Time':>12}  {'Throughput':>14}")
print(f"  {DIM}{'-'*10}  {'-'*12}  {'-'*14}{RESET}")
for name in ("heap", "mmap"):
    ns = load("source_steady_state", name)
    if ns is None:
        continue
    print(f"  {name:<10}  {fmt_ns(ns):>12}  {fmt_bytes(ns, 16384):>14}")

# =========================================================================
# Prewarm impact on startup
# =========================================================================
title("Prewarm impact on startup latency")
print(f"  {DIM}time to serve N sealed pages from a cold-started worker{RESET}\n")
print(
    f"  {'N pages':>9}  {'Cold':>12}  {'Prewarmed':>12}  "
    f"{'Speedup':>10}  {'Savings/page':>15}"
)
print(
    f"  {DIM}{'-'*9}  {'-'*12}  {'-'*12}  {'-'*10}  {'-'*15}{RESET}"
)
for n in (64, 256, 1024):
    c = load("startup_ready", "cold_first_allocs", str(n))
    p = load("startup_ready", "prewarm_then_allocs", str(n))
    if c is None or p is None:
        continue
    speedup = c / p
    savings_per = (c - p) / n
    color = efficiency_color(1.0 if speedup >= 1.5 else 0.8)
    spd = colored(f"{speedup:.2f}×", 10, color)
    savings_str = colored(fmt_ns(savings_per).strip(), 15, GREEN)
    print(
        f"  {n:>9}  {fmt_ns(c):>12}  {fmt_ns(p):>12}  {spd}  {savings_str}"
    )

# =========================================================================
# Chunk allocation: three size classes
# =========================================================================
title("Chunk allocation  (variable-size bytes on top of fixed-size pages)")
print(f"  {DIM}16 KiB underlying pages; threshold = 8 KiB{RESET}\n")
print(f"  {'Path':<11}  {'Size':>10}  {'Time':>12}  {'Throughput':>14}")
print(f"  {DIM}{'-'*11}  {'-'*10}  {'-'*12}  {'-'*14}{RESET}")

def emit_chunk_row(path, label, size):
    ns = load("chunk_alloc", path, str(size))
    if ns is None:
        return
    print(f"  {label:<11}  {size:>8} B  {fmt_ns(ns):>12}  {fmt_bytes(ns, size):>14}")

for size in (64, 256, 1024, 4096):
    emit_chunk_row("packed", "packed", size)
for size in (10_240, 12_288):
    emit_chunk_row("dedicated", "dedicated", size)
for size in (65_536, 262_144):
    emit_chunk_row("oversized", "oversized", size)

# =========================================================================
# Chunk vs page overhead
# =========================================================================
title("Chunk abstraction overhead vs raw Page  (16 KiB full-page alloc)")
print()
print(f"  {'Layer':<18}  {'Time':>12}  {'Throughput':>14}")
print(f"  {DIM}{'-'*18}  {'-'*12}  {'-'*14}{RESET}")
page_ns = load("chunk_vs_page_overhead", "page_full_alloc")
chunk_ns = load("chunk_vs_page_overhead", "chunk_full_alloc")
if page_ns is not None:
    print(f"  {'page_full_alloc':<18}  {fmt_ns(page_ns):>12}  {fmt_bytes(page_ns, 16384):>14}")
if chunk_ns is not None:
    print(f"  {'chunk_full_alloc':<18}  {fmt_ns(chunk_ns):>12}  {fmt_bytes(chunk_ns, 16384):>14}")
if page_ns is not None and chunk_ns is not None:
    overhead = chunk_ns - page_ns
    pct = (overhead / page_ns) * 100
    color = GREEN if pct < 50 else YELLOW
    print(f"\n  {DIM}Chunk overhead:{RESET} {color}{fmt_ns(overhead).strip()} (+{pct:.1f}%){RESET}")

# =========================================================================
# Cross-thread drop
# =========================================================================
title("Cross-thread drop  (page allocated on owner, dropped on N sibling threads)")
print()
print(f"  {'Droppers':>9}  {'Time':>12}  {'Throughput':>14}")
print(f"  {DIM}{'-'*9}  {'-'*12}  {'-'*14}{RESET}")
for n in (1, 2, 4, 8):
    ns = load("cross_thread_drop", str(n))
    if ns is None:
        continue
    print(f"  {n:>9}  {fmt_ns(ns):>12}  {fmt_ops(ns):>14}")

# =========================================================================
# Cloning & append
# =========================================================================
title("Misc")
print()
print(f"  {DIM}page clone (Arc bump):{RESET}")
for n in (1, 4, 16, 64):
    ns = load("page_clone", str(n))
    if ns is None:
        continue
    per_clone = ns / n
    print(f"    {n:>3} clones → {fmt_ns(ns):<12}  ({fmt_ns(per_clone)} / clone)")
print(f"\n  {DIM}append-fill 16 KiB page:{RESET}")
for chunk in (16, 64, 256, 1024):
    ns = load("append_fill_page", "chunk_bytes", str(chunk))
    if ns is None:
        continue
    print(f"    {chunk:>4} B chunks → {fmt_ns(ns):<12}  ({fmt_bytes(ns, 16384)})")

print()
PY
}

# ----------------------------------------------------------------- main
print_sys
if [ "$SKIP_RUN" -eq 0 ]; then
    run_benches
else
    title "Skipping run — reading existing target/criterion data"
fi
print_report

title "Done"
printf '  Full criterion HTML reports: %sfile://%s/target/criterion/report/index.html%s\n\n' \
    "$DIM" "$ROOT" "$RESET"
