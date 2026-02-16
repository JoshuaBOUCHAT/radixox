#!/bin/bash
# Compare RadixOx vs Redis with YCSB

YCSB_DIR="$HOME/ycsb-0.17.0"
WORKLOAD="workloads/workloada"
RECORDS=100000
OPS=100000

echo "═══════════════════════════════════════════"
echo "  YCSB Benchmark: RadixOx vs Redis"
echo "═══════════════════════════════════════════"
echo "Records: $RECORDS | Operations: $OPS"
echo "Workload A: 50% read, 50% update"
echo ""

# ============================================
# RadixOx Benchmark
# ============================================
echo "🦀 [1/4] Benchmarking RadixOx (port 6379)..."
echo "-------------------------------------------"

cd "$YCSB_DIR"

# Load
echo "Loading data..."
bin/ycsb.sh load redis -s -P "$WORKLOAD" \
  -p redis.host=127.0.0.1 -p redis.port=6379 \
  -p fieldcount=1 -p fieldnamekey=false \
  -p recordcount=$RECORDS \
  > /tmp/radixox_load.txt 2>&1

RADIXOX_LOAD_OPS=$(grep "Throughput(ops/sec)" /tmp/radixox_load.txt | awk '{print $3}')
RADIXOX_LOAD_P99=$(grep "\[INSERT\], 99thPercentileLatency" /tmp/radixox_load.txt | awk '{print $3}')

echo "  ✓ Load: $RADIXOX_LOAD_OPS ops/sec | P99: $RADIXOX_LOAD_P99 µs"

# Run
echo "Running benchmark..."
bin/ycsb.sh run redis -s -P "$WORKLOAD" \
  -p redis.host=127.0.0.1 -p redis.port=6379 \
  -p fieldcount=1 -p fieldnamekey=false \
  -p operationcount=$OPS \
  > /tmp/radixox_run.txt 2>&1

RADIXOX_RUN_OPS=$(grep "Throughput(ops/sec)" /tmp/radixox_run.txt | awk '{print $3}')
RADIXOX_READ_AVG=$(grep "\[READ\], AverageLatency" /tmp/radixox_run.txt | awk '{print $3}')
RADIXOX_READ_P99=$(grep "\[READ\], 99thPercentileLatency" /tmp/radixox_run.txt | awk '{print $3}')
RADIXOX_UPDATE_AVG=$(grep "\[UPDATE\], AverageLatency" /tmp/radixox_update.txt | awk '{print $3}')
RADIXOX_UPDATE_P99=$(grep "\[UPDATE\], 99thPercentileLatency" /tmp/radixox_run.txt | awk '{print $3}')

echo "  ✓ Run:  $RADIXOX_RUN_OPS ops/sec"
echo "  ✓ READ:  Avg ${RADIXOX_READ_AVG} µs | P99 ${RADIXOX_READ_P99} µs"
echo "  ✓ UPDATE: P99 ${RADIXOX_UPDATE_P99} µs"
echo ""

# Flush RadixOx
redis-cli -p 6379 FLUSHDB > /dev/null 2>&1

# ============================================
# Redis Benchmark
# ============================================
echo "🔴 [2/4] Benchmarking Redis (port 6380)..."
echo "-------------------------------------------"

# Load
echo "Loading data..."
bin/ycsb.sh load redis -s -P "$WORKLOAD" \
  -p redis.host=127.0.0.1 -p redis.port=6380 \
  -p fieldcount=1 -p fieldnamekey=false \
  -p recordcount=$RECORDS \
  > /tmp/redis_load.txt 2>&1

REDIS_LOAD_OPS=$(grep "Throughput(ops/sec)" /tmp/redis_load.txt | awk '{print $3}')
REDIS_LOAD_P99=$(grep "\[INSERT\], 99thPercentileLatency" /tmp/redis_load.txt | awk '{print $3}')

echo "  ✓ Load: $REDIS_LOAD_OPS ops/sec | P99: $REDIS_LOAD_P99 µs"

# Run
echo "Running benchmark..."
bin/ycsb.sh run redis -s -P "$WORKLOAD" \
  -p redis.host=127.0.0.1 -p redis.port=6380 \
  -p fieldcount=1 -p fieldnamekey=false \
  -p operationcount=$OPS \
  > /tmp/redis_run.txt 2>&1

REDIS_RUN_OPS=$(grep "Throughput(ops/sec)" /tmp/redis_run.txt | awk '{print $3}')
REDIS_READ_AVG=$(grep "\[READ\], AverageLatency" /tmp/redis_run.txt | awk '{print $3}')
REDIS_READ_P99=$(grep "\[READ\], 99thPercentileLatency" /tmp/redis_run.txt | awk '{print $3}')
REDIS_UPDATE_P99=$(grep "\[UPDATE\], 99thPercentileLatency" /tmp/redis_run.txt | awk '{print $3}')

echo "  ✓ Run:  $REDIS_RUN_OPS ops/sec"
echo "  ✓ READ:  Avg ${REDIS_READ_AVG} µs | P99 ${REDIS_READ_P99} µs"
echo "  ✓ UPDATE: P99 ${REDIS_UPDATE_P99} µs"
echo ""

# ============================================
# Comparison
# ============================================
echo "═══════════════════════════════════════════"
echo "  📊 RESULTS COMPARISON"
echo "═══════════════════════════════════════════"
echo ""
printf "%-20s %15s %15s %15s\n" "Metric" "RadixOx" "Redis" "Winner"
echo "-----------------------------------------------------------"
printf "%-20s %15s %15s %15s\n" "Load Throughput" "$RADIXOX_LOAD_OPS" "$REDIS_LOAD_OPS" "$(echo "$RADIXOX_LOAD_OPS > $REDIS_LOAD_OPS" | bc -l | grep -q 1 && echo "🦀 RadixOx" || echo "🔴 Redis")"
printf "%-20s %15s %15s %15s\n" "Run Throughput" "$RADIXOX_RUN_OPS" "$REDIS_RUN_OPS" "$(echo "$RADIXOX_RUN_OPS > $REDIS_RUN_OPS" | bc -l | grep -q 1 && echo "🦀 RadixOx" || echo "🔴 Redis")"
printf "%-20s %15s %15s %15s\n" "READ P99 (µs)" "$RADIXOX_READ_P99" "$REDIS_READ_P99" "$(echo "$RADIXOX_READ_P99 < $REDIS_READ_P99" | bc -l | grep -q 1 && echo "🦀 RadixOx" || echo "🔴 Redis")"
printf "%-20s %15s %15s %15s\n" "UPDATE P99 (µs)" "$RADIXOX_UPDATE_P99" "$REDIS_UPDATE_P99" "$(echo "$RADIXOX_UPDATE_P99 < $REDIS_UPDATE_P99" | bc -l | grep -q 1 && echo "🦀 RadixOx" || echo "🔴 Redis")"
echo ""
echo "Full results: /tmp/radixox_*.txt and /tmp/redis_*.txt"
