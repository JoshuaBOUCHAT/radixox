#!/bin/bash
# Test Redis Hash commands via redis-cli on localhost:6379

set -e

echo "=== Testing Hash Commands ==="
echo

# HSET - set fields
echo "Testing HSET..."
redis-cli -p 6379 HSET user:1 name "Alice" age "30" city "Paris"  # Should return 3 (3 new fields)
redis-cli -p 6379 HSET user:1 name "Bob"  # Should return 0 (update existing)

# HGET - get a field
echo "Testing HGET..."
redis-cli -p 6379 HGET user:1 name  # Should return "Bob"
redis-cli -p 6379 HGET user:1 nonexistent  # Should return nil

# HGETALL - get all fields
echo "Testing HGETALL..."
redis-cli -p 6379 HGETALL user:1  # Should return [age, 30, city, Paris, name, Bob]

# HEXISTS - check if field exists
echo "Testing HEXISTS..."
redis-cli -p 6379 HEXISTS user:1 name  # Should return 1
redis-cli -p 6379 HEXISTS user:1 email  # Should return 0

# HLEN - get number of fields
echo "Testing HLEN..."
redis-cli -p 6379 HLEN user:1  # Should return 3

# HKEYS - get all field names
echo "Testing HKEYS..."
redis-cli -p 6379 HKEYS user:1  # Should return [age, city, name]

# HVALS - get all values
echo "Testing HVALS..."
redis-cli -p 6379 HVALS user:1  # Should return [30, Paris, Bob]

# HMGET - get multiple fields
echo "Testing HMGET..."
redis-cli -p 6379 HMGET user:1 name age email  # Should return [Bob, 30, nil]

# HINCRBY - increment a field
echo "Testing HINCRBY..."
redis-cli -p 6379 HINCRBY user:1 age 5  # Should return 35
redis-cli -p 6379 HGET user:1 age  # Should return "35"
redis-cli -p 6379 HINCRBY stats:page views 1  # Create new field, should return 1
redis-cli -p 6379 HINCRBY stats:page views 10  # Should return 11

# HDEL - delete fields
echo "Testing HDEL..."
redis-cli -p 6379 HDEL user:1 city nonexistent  # Should return 1 (only city deleted)
redis-cli -p 6379 HLEN user:1  # Should return 2 now

# Type check
echo "Testing TYPE on Hash..."
redis-cli -p 6379 TYPE user:1  # Should return "hash"

# WRONGTYPE error
echo "Testing WRONGTYPE error..."
redis-cli -p 6379 SET stringkey "value"
redis-cli -p 6379 HGET stringkey field || echo "Expected WRONGTYPE error"

# Empty hash cleanup
echo "Testing empty hash cleanup..."
redis-cli -p 6379 HSET cleanhash f1 "v1" f2 "v2"
redis-cli -p 6379 HDEL cleanhash f1 f2  # Remove all fields
redis-cli -p 6379 EXISTS cleanhash  # Should return 0 (hash auto-deleted when empty)

# Multiple HSET
echo "Testing multiple HSET..."
redis-cli -p 6379 HSET product:1 title "Laptop" price "999" stock "42" brand "TechCo"
redis-cli -p 6379 HLEN product:1  # Should return 4

echo
echo "=== All Hash tests completed ==="
