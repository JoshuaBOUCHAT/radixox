#!/bin/bash
# Redis ultra-optimized for benchmarking (no persistence)

echo "🚀 Lancement Redis optimisé pour benchmark..."

# Stop existing redis-bench container if any
docker stop redis-bench 2>/dev/null || true
docker rm redis-bench 2>/dev/null || true

# Launch Redis 7.4 with optimal settings
docker run -d \
  --name redis-bench \
  -p 6380:6379 \
  --network host \
  redis:7.4-alpine \
  redis-server \
  --save "" \
  --appendonly no \
  --maxmemory-policy noeviction \
  --tcp-backlog 511 \
  --timeout 0 \
  --tcp-keepalive 300 \
  --daemonize no \
  --loglevel warning

echo "✅ Redis lancé sur port 6380"
echo ""
echo "Test connexion:"
sleep 1
redis-cli -p 6380 PING

echo ""
echo "📊 Pour bencher Redis:"
echo "  YCSB load: bin/ycsb.sh load redis -p redis.port=6380 ..."
echo "  YCSB run:  bin/ycsb.sh run redis -p redis.port=6380 ..."
echo ""
echo "🛑 Pour arrêter: docker stop redis-bench"
