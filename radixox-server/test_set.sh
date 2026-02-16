#!/bin/bash
# Test Redis Set commands via redis-cli on localhost:6379

set -e

echo "=== Testing Set Commands ==="
echo

# SADD - add members to a set
echo "Testing SADD..."
redis-cli -p 6379 SADD myset "member1" "member2" "member3"
redis-cli -p 6379 SADD myset "member2"  # Already exists, should return 0

# SCARD - get cardinality
echo "Testing SCARD..."
redis-cli -p 6379 SCARD myset  # Should return 3

# SISMEMBER - check if member exists
echo "Testing SISMEMBER..."
redis-cli -p 6379 SISMEMBER myset "member1"  # Should return 1
redis-cli -p 6379 SISMEMBER myset "member99"  # Should return 0

# SMEMBERS - get all members
echo "Testing SMEMBERS..."
redis-cli -p 6379 SMEMBERS myset

# SREM - remove members
echo "Testing SREM..."
redis-cli -p 6379 SREM myset "member1" "member99"  # Should return 1 (only member1 removed)
redis-cli -p 6379 SCARD myset  # Should return 2 now

# SPOP - pop a random member
echo "Testing SPOP..."
redis-cli -p 6379 SPOP myset  # Should return one member
redis-cli -p 6379 SCARD myset  # Should return 1 now

# SPOP with count
echo "Testing SPOP with count..."
redis-cli -p 6379 SADD myset2 "a" "b" "c" "d" "e"
redis-cli -p 6379 SPOP myset2 3  # Should return 3 members
redis-cli -p 6379 SCARD myset2  # Should return 2 now

# Type check
echo "Testing TYPE on Set..."
redis-cli -p 6379 SADD typeset "value"
redis-cli -p 6379 TYPE typeset  # Should return "set"

# WRONGTYPE error
echo "Testing WRONGTYPE error..."
redis-cli -p 6379 SET stringkey "value"
redis-cli -p 6379 SADD stringkey "member" || echo "Expected WRONGTYPE error"

# Empty set cleanup
echo "Testing empty set cleanup..."
redis-cli -p 6379 SADD cleanupset "a" "b"
redis-cli -p 6379 SREM cleanupset "a" "b"  # Remove all members
redis-cli -p 6379 EXISTS cleanupset  # Should return 0 (set auto-deleted when empty)

echo
echo "=== All Set tests completed ==="
