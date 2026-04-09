use oxidart::OxidArt;
use oxidart::error::TypeError;
use radixox_lib::shared_byte::SharedByte;
use radixox_lib::shared_frame::SharedFrame as Frame;

pub fn cmd_hset(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 3 || args.len().is_multiple_of(2) {
        return Frame::Error("ERR wrong number of arguments for 'HSET' command".into());
    }
    let field_values: Vec<(SharedByte, SharedByte)> = args[1..]
        .chunks_exact(2)
        .map(|chunk| (chunk[0].clone(), chunk[1].clone()))
        .collect();

    match art.cmd_hset(&args[0], &field_values, None) {
        Ok(added) => Frame::Integer(added as i64),
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(_) => Frame::Error("ERR internal error".into()),
    }
}

/// HMSET - legacy command (deprecated since Redis 4.0, use HSET instead)
pub fn cmd_hmset(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 3 || args.len().is_multiple_of(2) {
        return Frame::Error("ERR wrong number of arguments for 'HMSET' command".into());
    }
    let field_values: Vec<(SharedByte, SharedByte)> = args[1..]
        .chunks_exact(2)
        .map(|chunk| (chunk[0].clone(), chunk[1].clone()))
        .collect();

    match art.cmd_hset(&args[0], &field_values, None) {
        Ok(_) => Frame::SimpleString(SharedByte::from_slice(b"OK")),
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(_) => Frame::Error("ERR internal error".into()),
    }
}

pub fn cmd_hget(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'HGET' command".into());
    }
    match art.cmd_hget(&args[0], &args[1]) {
        Ok(Some(val)) => Frame::BulkString(val),
        Ok(None) => Frame::Null,
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hgetall(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'HGETALL' command".into());
    }
    match art.cmd_hgetall(&args[0]) {
        Ok(fields) => Frame::Array(fields.into_iter().map(Frame::BulkString).collect()),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hdel(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'HDEL' command".into());
    }
    match art.cmd_hdel(&args[0], &args[1..]) {
        Ok(count) => Frame::Integer(count as i64),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hexists(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'HEXISTS' command".into());
    }
    match art.cmd_hexists(&args[0], &args[1]) {
        Ok(exists) => Frame::Integer(if exists { 1 } else { 0 }),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hlen(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'HLEN' command".into());
    }
    match art.cmd_hlen(&args[0]) {
        Ok(len) => Frame::Integer(len as i64),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hkeys(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'HKEYS' command".into());
    }
    match art.cmd_hkeys(&args[0]) {
        Ok(keys) => Frame::Array(keys.into_iter().map(Frame::BulkString).collect()),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hvals(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'HVALS' command".into());
    }
    match art.cmd_hvals(&args[0]) {
        Ok(vals) => Frame::Array(vals.into_iter().map(Frame::BulkString).collect()),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hmget(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'HMGET' command".into());
    }
    match art.cmd_hmget(&args[0], &args[1..]) {
        Ok(values) => Frame::Array(
            values
                .into_iter()
                .map(|v| match v {
                    Some(b) => Frame::BulkString(b),
                    None => Frame::Null,
                })
                .collect(),
        ),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hincrby(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 3 {
        return Frame::Error("ERR wrong number of arguments for 'HINCRBY' command".into());
    }
    let increment: i64 = match std::str::from_utf8(&args[2])
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(n) => n,
        None => return Frame::Error("ERR value is not an integer or out of range".into()),
    };
    match art.cmd_hincrby(&args[0], args[1].clone(), increment) {
        Ok(new_val) => Frame::Integer(new_val),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}
