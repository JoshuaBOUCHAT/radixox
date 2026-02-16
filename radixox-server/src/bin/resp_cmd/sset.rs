use bytes::Bytes;
use oxidart::OxidArt;
use oxidart::error::TypeError;
use redis_protocol::resp2::types::BytesFrame as Frame;

pub fn cmd_sadd(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'SADD' command".into());
    }
    let key = &args[0];
    let members = &args[1..];

    match art.cmd_sadd(key, members, None) {
        Ok(count) => Frame::Integer(count as i64),
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(_) => Frame::Error("ERR internal error".into()),
    }
}

pub fn cmd_srem(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'SREM' command".into());
    }
    let key = &args[0];
    let members = &args[1..];

    match art.cmd_srem(key, members) {
        Ok(count) => Frame::Integer(count as i64),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected set, got {})",
            redis_type.as_str()
        ).into()),
    }
}

pub fn cmd_sismember(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'SISMEMBER' command".into());
    }
    let key = &args[0];
    let member = &args[1];

    match art.cmd_sismember(key, member) {
        Ok(exists) => Frame::Integer(if exists { 1 } else { 0 }),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected set, got {})",
            redis_type.as_str()
        ).into()),
    }
}

pub fn cmd_scard(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'SCARD' command".into());
    }
    let key = &args[0];

    match art.cmd_scard(key) {
        Ok(count) => Frame::Integer(count as i64),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected set, got {})",
            redis_type.as_str()
        ).into()),
    }
}

pub fn cmd_smembers(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'SMEMBERS' command".into());
    }
    let key = &args[0];

    match art.cmd_smembers(key) {
        Ok(members) => {
            let frames: Vec<Frame> = members
                .into_iter()
                .map(|b| Frame::BulkString(b))
                .collect();
            Frame::Array(frames)
        }
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected set, got {})",
            redis_type.as_str()
        ).into()),
    }
}

pub fn cmd_spop(args: &[Bytes], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'SPOP' command".into());
    }

    let key = &args[0];
    let count = args.get(1).map(|b| b.as_ref());

    match art.cmd_spop(key, count) {
        Ok(oxidart::scommand::SPOPResult::Single(opt)) => match opt {
            Some(val) => Frame::BulkString(val),
            None => Frame::Null,
        },
        Ok(oxidart::scommand::SPOPResult::Multiple(vec)) => {
            let frames: Vec<Frame> = vec.into_iter().map(|b| Frame::BulkString(b)).collect();
            Frame::Array(frames)
        }
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(TypeError::NotAInt) => {
            Frame::Error("ERR value is not an integer or out of range".into())
        }
    }
}
