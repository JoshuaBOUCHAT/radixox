#!/bin/bash
# Test script for radixox-resp server
# Requires: redis-cli, server running on port 6379

set -e
PORT=6379
CLI="redis-cli -p $PORT"

echo "=== RadixOx RESP Test Suite ==="
echo

# Clean state
$CLI FLUSHDB > /dev/null

echo "--- Meta Commands ---"
echo -n "PING: "
$CLI PING

echo -n "ECHO hello: "
$CLI ECHO hello

echo -n "SELECT 0: "
$CLI SELECT 0

echo
echo "--- Basic GET/SET ---"
echo -n "SET foo bar: "
$CLI SET foo bar

echo -n "GET foo: "
$CLI GET foo

echo -n "GET nonexistent: "
$CLI GET nonexistent

echo
echo "--- SET with TTL ---"
echo -n "SET temp value EX 10: "
$CLI SET temp value EX 10

echo -n "TTL temp: "
$CLI TTL temp

echo -n "SET temp2 value PX 5000: "
$CLI SET temp2 value PX 5000

echo -n "PTTL temp2: "
$CLI PTTL temp2

echo
echo "--- SET conditions (NX/XX) ---"
echo -n "SET newkey val NX (should OK): "
$CLI SET newkey val NX

echo -n "SET newkey val2 NX (should nil): "
$CLI SET newkey val2 NX

echo -n "SET newkey val3 XX (should OK): "
$CLI SET newkey val3 XX

echo -n "SET ghost val XX (should nil): "
$CLI SET ghost val XX

echo
echo "--- SETNX / SETEX ---"
echo -n "SETNX brand_new hello (should 1): "
$CLI SETNX brand_new hello

echo -n "SETNX brand_new world (should 0): "
$CLI SETNX brand_new world

echo -n "SETEX timed 60 myvalue: "
$CLI SETEX timed 60 myvalue

echo -n "TTL timed: "
$CLI TTL timed

echo
echo "--- MGET / MSET ---"
echo -n "MSET a 1 b 2 c 3: "
$CLI MSET a 1 b 2 c 3

echo -n "MGET a b c nonexistent: "
$CLI MGET a b c nonexistent

echo
echo "--- EXISTS / DEL ---"
echo -n "EXISTS a b c ghost: "
$CLI EXISTS a b c ghost

echo -n "DEL a b: "
$CLI DEL a b

echo -n "EXISTS a b c: "
$CLI EXISTS a b c

echo
echo "--- KEYS (prefix search) ---"
$CLI SET user:1 alice > /dev/null
$CLI SET user:2 bob > /dev/null
$CLI SET user:3 charlie > /dev/null
$CLI SET other:x data > /dev/null

echo -n "KEYS user:*: "
$CLI KEYS "user:*"

echo
echo "--- EXPIRE / PERSIST ---"
echo -n "EXPIRE c 120: "
$CLI EXPIRE c 120

echo -n "TTL c: "
$CLI TTL c

echo -n "PERSIST c: "
$CLI PERSIST c

echo -n "TTL c (should -1): "
$CLI TTL c

echo
echo "--- PEXPIRE ---"
echo -n "PEXPIRE c 30000: "
$CLI PEXPIRE c 30000

echo -n "PTTL c: "
$CLI PTTL c

echo
echo "--- TYPE ---"
echo -n "TYPE c: "
$CLI TYPE c

echo -n "TYPE nonexistent: "
$CLI TYPE nonexistent

echo
echo "--- DBSIZE / FLUSHDB ---"
echo -n "DBSIZE: "
$CLI DBSIZE

echo -n "FLUSHDB: "
$CLI FLUSHDB

echo -n "DBSIZE (should 0): "
$CLI DBSIZE

echo
echo "=== All tests passed! ==="
