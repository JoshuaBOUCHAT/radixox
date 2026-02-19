use bytes::Bytes;
use oxidart::OxidArt;
use oxidart::error::TypeError;
use redis_protocol::resp2::types::BytesFrame as Frame;

// Static OK response for HMSET
static OK: Bytes = Bytes::from_static(b"OK");

pub fn cmd_hset(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 3 || args.len().is_multiple_of(2) {
        return Frame::Error("ERR wrong number of arguments for 'HSET' command".into());
    }
    let key = &args[0];
    let field_values: Vec<(Bytes, Bytes)> = args[1..]
        .chunks_exact(2)
        .map(|chunk| (chunk[0].clone(), chunk[1].clone()))
        .collect();

    match art.cmd_hset(key, &field_values, None) {
        Ok(added) => Frame::Integer(added as i64),
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(_) => Frame::Error("ERR internal error".into()),
    }
}

/// HMSET - legacy command (deprecated since Redis 4.0, use HSET instead)
/// Sets multiple fields in a hash. Returns "OK" for compatibility with old clients.
/// Identical to HSET but returns SimpleString "OK" instead of Integer count.
pub fn cmd_hmset(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 3 || args.len().is_multiple_of(2) {
        return Frame::Error("ERR wrong number of arguments for 'HMSET' command".into());
    }
    let key = &args[0];
    let field_values: Vec<(Bytes, Bytes)> = args[1..]
        .chunks_exact(2)
        .map(|chunk| (chunk[0].clone(), chunk[1].clone()))
        .collect();

    match art.cmd_hset(key, &field_values, None) {
        Ok(_) => Frame::SimpleString(OK.clone()), // Legacy: always return OK
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(_) => Frame::Error("ERR internal error".into()),
    }
}

pub fn cmd_hget(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() != 2 {
        return Frame::Error("ERR wrong number of arguments for 'HGET' command".into());
    }
    let key = &args[0];
    let field = &args[1];

    match art.cmd_hget(key, field) {
        Ok(Some(value)) => Frame::BulkString(value),
        Ok(None) => Frame::Null,
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected hash, got {})",
            redis_type.as_str()
        ).into()),
    }
}

pub fn cmd_hgetall(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() != 1 {
        return Frame::Error("ERR wrong number of arguments for 'HGETALL' command".into());
    }
    let key = &args[0];

    match art.cmd_hgetall(key) {
        Ok(flat_pairs) => {
            let frames: Vec<Frame> = flat_pairs
                .into_iter()
                .map(Frame::BulkString)
                .collect();
            Frame::Array(frames)
        }
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected hash, got {})",
            redis_type.as_str()
        ).into()),
    }
}

pub fn cmd_hdel(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'HDEL' command".into());
    }
    let key = &args[0];
    let fields = &args[1..];

    match art.cmd_hdel(key, fields) {
        Ok(deleted) => Frame::Integer(deleted as i64),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected hash, got {})",
            redis_type.as_str()
        ).into()),
    }
}

pub fn cmd_hexists(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() != 2 {
        return Frame::Error("ERR wrong number of arguments for 'HEXISTS' command".into());
    }
    let key = &args[0];
    let field = &args[1];

    match art.cmd_hexists(key, field) {
        Ok(exists) => Frame::Integer(if exists { 1 } else { 0 }),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected hash, got {})",
            redis_type.as_str()
        ).into()),
    }
}

pub fn cmd_hlen(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() != 1 {
        return Frame::Error("ERR wrong number of arguments for 'HLEN' command".into());
    }
    let key = &args[0];

    match art.cmd_hlen(key) {
        Ok(len) => Frame::Integer(len as i64),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected hash, got {})",
            redis_type.as_str()
        ).into()),
    }
}

pub fn cmd_hkeys(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() != 1 {
        return Frame::Error("ERR wrong number of arguments for 'HKEYS' command".into());
    }
    let key = &args[0];

    match art.cmd_hkeys(key) {
        Ok(keys) => {
            let frames: Vec<Frame> = keys.into_iter().map(Frame::BulkString).collect();
            Frame::Array(frames)
        }
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected hash, got {})",
            redis_type.as_str()
        ).into()),
    }
}

pub fn cmd_hvals(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() != 1 {
        return Frame::Error("ERR wrong number of arguments for 'HVALS' command".into());
    }
    let key = &args[0];

    match art.cmd_hvals(key) {
        Ok(vals) => {
            let frames: Vec<Frame> = vals.into_iter().map(Frame::BulkString).collect();
            Frame::Array(frames)
        }
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected hash, got {})",
            redis_type.as_str()
        ).into()),
    }
}

pub fn cmd_hmget(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'HMGET' command".into());
    }
    let key = &args[0];
    let fields = &args[1..];

    match art.cmd_hmget(key, fields) {
        Ok(values) => {
            let frames: Vec<Frame> = values
                .into_iter()
                .map(|opt| match opt {
                    Some(v) => Frame::BulkString(v),
                    None => Frame::Null,
                })
                .collect();
            Frame::Array(frames)
        }
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected hash, got {})",
            redis_type.as_str()
        ).into()),
    }
}

pub fn cmd_hincrby(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() != 3 {
        return Frame::Error("ERR wrong number of arguments for 'HINCRBY' command".into());
    }
    let key = &args[0];
    let field = &args[1];
    let increment = match parse_i64(&args[2]) {
        Some(n) => n,
        None => return Frame::Error("ERR value is not an integer or out of range".into()),
    };

    match art.cmd_hincrby(key, field, increment) {
        Ok(new_val) => Frame::Integer(new_val),
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(TypeError::NotAInt) => {
            Frame::Error("ERR hash value is not an integer or out of range".into())
        }
    }
}

// Helper to parse i64 from Bytes
fn parse_i64(data: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(data).ok()?;
    s.parse::<i64>().ok()
}
