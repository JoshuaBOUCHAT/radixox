use oxidart::OxidArt;
use oxidart::error::TypeError;
use radixox_lib::shared_byte::SharedByte;
use radixox_lib::shared_frame::SharedFrame as Frame;

pub fn cmd_zadd(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 3 || args.len().is_multiple_of(2) {
        return Frame::Error("ERR wrong number of arguments for 'ZADD' command".into());
    }
    let mut score_members = Vec::new();
    for chunk in args[1..].chunks_exact(2) {
        let score = match parse_f64(&chunk[0]) {
            Some(s) => s,
            None => return Frame::Error("ERR value is not a valid float".into()),
        };
        score_members.push((score, chunk[1].clone()));
    }
    match art.cmd_zadd(args[0].clone(), &score_members, None) {
        Ok(added) => Frame::Integer(added as i64),
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(_) => Frame::Error("ERR internal error".into()),
    }
}

pub fn cmd_zcard(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() != 1 {
        return Frame::Error("ERR wrong number of arguments for 'ZCARD' command".into());
    }
    match art.cmd_zcard(&args[0]) {
        Ok(count) => Frame::Integer(count as i64),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected zset, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_zrange(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 3 {
        return Frame::Error("ERR wrong number of arguments for 'ZRANGE' command".into());
    }
    let start = match parse_i64(&args[1]) {
        Some(n) => n,
        None => return Frame::Error("ERR value is not an integer or out of range".into()),
    };
    let stop = match parse_i64(&args[2]) {
        Some(n) => n,
        None => return Frame::Error("ERR value is not an integer or out of range".into()),
    };
    let with_scores = args
        .get(3)
        .is_some_and(|opt| opt.eq_ignore_ascii_case(b"WITHSCORES"));

    match art.cmd_zrange(&args[0], start, stop, with_scores) {
        Ok(result) => Frame::Array(result.into_iter().map(Frame::BulkString).collect()),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected zset, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_zscore(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() != 2 {
        return Frame::Error("ERR wrong number of arguments for 'ZSCORE' command".into());
    }
    match art.cmd_zscore(&args[0], args[1].clone()) {
        Ok(Some(score)) => Frame::BulkString(SharedByte::from_slice(score.to_string().as_bytes())),
        Ok(None) => Frame::Null,
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected zset, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_zrem(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'ZREM' command".into());
    }
    match art.cmd_zrem(&args[0], &args[1..]) {
        Ok(removed) => Frame::Integer(removed as i64),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected zset, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_zincrby(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() != 3 {
        return Frame::Error("ERR wrong number of arguments for 'ZINCRBY' command".into());
    }
    let increment = match parse_f64(&args[1]) {
        Some(n) => n,
        None => return Frame::Error("ERR value is not a valid float".into()),
    };
    match art.cmd_zincrby(args[0].clone(), increment, args[2].clone()) {
        Ok(new_score) => {
            Frame::BulkString(SharedByte::from_slice(new_score.to_string().as_bytes()))
        }
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(_) => Frame::Error("ERR internal error".into()),
    }
}

fn parse_f64(data: &[u8]) -> Option<f64> {
    std::str::from_utf8(data).ok()?.parse::<f64>().ok()
}

fn parse_i64(data: &[u8]) -> Option<i64> {
    std::str::from_utf8(data).ok()?.parse::<i64>().ok()
}
