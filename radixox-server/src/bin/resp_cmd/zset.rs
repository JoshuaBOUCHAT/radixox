use bytes::Bytes;
use oxidart::error::TypeError;
use oxidart::OxidArt;
use redis_protocol::resp2::types::BytesFrame as Frame;

pub fn cmd_zadd(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 3 || args.len() % 2 == 0 {
        return Frame::Error("ERR wrong number of arguments for 'ZADD' command".into());
    }
    let key = &args[0];

    // Parse score-member pairs
    let mut score_members = Vec::new();
    for chunk in args[1..].chunks_exact(2) {
        let score = match parse_f64(&chunk[0]) {
            Some(s) => s,
            None => return Frame::Error("ERR value is not a valid float".into()),
        };
        score_members.push((score, chunk[1].clone()));
    }

    match art.cmd_zadd(key, &score_members, None) {
        Ok(added) => Frame::Integer(added as i64),
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(_) => Frame::Error("ERR internal error".into()),
    }
}

pub fn cmd_zcard(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() != 1 {
        return Frame::Error("ERR wrong number of arguments for 'ZCARD' command".into());
    }
    let key = &args[0];

    match art.cmd_zcard(key) {
        Ok(count) => Frame::Integer(count as i64),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected zset, got {})",
            redis_type.as_str()
        ).into()),
    }
}

pub fn cmd_zrange(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 3 {
        return Frame::Error("ERR wrong number of arguments for 'ZRANGE' command".into());
    }
    let key = &args[0];
    let start = match parse_i64(&args[1]) {
        Some(n) => n,
        None => return Frame::Error("ERR value is not an integer or out of range".into()),
    };
    let stop = match parse_i64(&args[2]) {
        Some(n) => n,
        None => return Frame::Error("ERR value is not an integer or out of range".into()),
    };

    // Check for WITHSCORES option
    let with_scores = args.get(3).map_or(false, |opt| opt.eq_ignore_ascii_case(b"WITHSCORES"));

    match art.cmd_zrange(key, start, stop, with_scores) {
        Ok(result) => {
            let frames: Vec<Frame> = result.into_iter().map(|b| Frame::BulkString(b)).collect();
            Frame::Array(frames)
        }
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected zset, got {})",
            redis_type.as_str()
        ).into()),
    }
}

pub fn cmd_zscore(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() != 2 {
        return Frame::Error("ERR wrong number of arguments for 'ZSCORE' command".into());
    }
    let key = &args[0];
    let member = &args[1];

    match art.cmd_zscore(key, member) {
        Ok(Some(score)) => Frame::BulkString(Bytes::from(score.to_string())),
        Ok(None) => Frame::Null,
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected zset, got {})",
            redis_type.as_str()
        ).into()),
    }
}

pub fn cmd_zrem(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'ZREM' command".into());
    }
    let key = &args[0];
    let members = &args[1..];

    match art.cmd_zrem(key, members) {
        Ok(removed) => Frame::Integer(removed as i64),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected zset, got {})",
            redis_type.as_str()
        ).into()),
    }
}

// Helper to parse f64 from Bytes
fn parse_f64(data: &[u8]) -> Option<f64> {
    let s = std::str::from_utf8(data).ok()?;
    s.parse::<f64>().ok()
}

pub fn cmd_zincrby(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() != 3 {
        return Frame::Error("ERR wrong number of arguments for 'ZINCRBY' command".into());
    }
    let key = &args[0];
    let increment = match parse_f64(&args[1]) {
        Some(n) => n,
        None => return Frame::Error("ERR value is not a valid float".into()),
    };
    let member = &args[2];

    match art.cmd_zincrby(key, increment, member) {
        Ok(new_score) => Frame::BulkString(Bytes::from(new_score.to_string())),
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(_) => Frame::Error("ERR internal error".into()),
    }
}

// Helper to parse i64 from Bytes
fn parse_i64(data: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(data).ok()?;
    s.parse::<i64>().ok()
}
