use oxidart::OxidArt;
use oxidart::error::TypeError;
use radixox_lib::shared_byte::SharedByte;
use radixox_lib::shared_frame::SharedFrame as Frame;

pub fn cmd_sadd(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'SADD' command".into());
    }
    match art.cmd_sadd(&args[0], &args[1..], None) {
        Ok(count) => Frame::Integer(count as i64),
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(_) => Frame::Error("ERR internal error".into()),
    }
}

pub fn cmd_srem(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'SREM' command".into());
    }
    match art.cmd_srem(&args[0], &args[1..]) {
        Ok(count) => Frame::Integer(count as i64),
        Err(redis_type) => Frame::Error(
            format!(
                "WRONGTYPE Operation against a key holding the wrong kind of value (expected set, got {})",
                redis_type.as_str()
            ),
        ),
    }
}

pub fn cmd_sismember(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.len() < 2 {
        return Frame::Error("ERR wrong number of arguments for 'SISMEMBER' command".into());
    }
    match art.cmd_sismember(&args[0], args[1].clone()) {
        Ok(exists) => Frame::Integer(if exists { 1 } else { 0 }),
        Err(redis_type) => Frame::Error(
            format!(
                "WRONGTYPE Operation against a key holding the wrong kind of value (expected set, got {})",
                redis_type.as_str()
            ),
        ),
    }
}

pub fn cmd_scard(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'SCARD' command".into());
    }
    match art.cmd_scard(&args[0]) {
        Ok(count) => Frame::Integer(count as i64),
        Err(redis_type) => Frame::Error(
            format!(
                "WRONGTYPE Operation against a key holding the wrong kind of value (expected set, got {})",
                redis_type.as_str()
            ),
        ),
    }
}

pub fn cmd_smembers(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'SMEMBERS' command".into());
    }
    match art.cmd_smembers(&args[0]) {
        Ok(members) => Frame::Array(members.into_iter().map(Frame::BulkString).collect()),
        Err(redis_type) => Frame::Error(
            format!(
                "WRONGTYPE Operation against a key holding the wrong kind of value (expected set, got {})",
                redis_type.as_str()
            ),
        ),
    }
}

pub fn cmd_spop(args: &[SharedByte], art: &mut OxidArt) -> Frame {
    if args.is_empty() {
        return Frame::Error("ERR wrong number of arguments for 'SPOP' command".into());
    }
    let count = args.get(1).map(|b| b.as_ref());
    match art.cmd_spop(&args[0], count) {
        Ok(oxidart::scommand::SPOPResult::Single(opt)) => match opt {
            Some(val) => Frame::BulkString(val),
            None => Frame::Null,
        },
        Ok(oxidart::scommand::SPOPResult::Multiple(vec)) => {
            Frame::Array(vec.into_iter().map(Frame::BulkString).collect())
        }
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(TypeError::NotAInt) => {
            Frame::Error("ERR value is not an integer or out of range".into())
        }
    }
}
