use oxidart::OxidArt;
use oxidart::error::TypeError;
use oxidart::scommand::SPOPResult;
use radixox_lib::shared_byte::SharedByte;
use radixox_lib::shared_frame::SharedFrame as Frame;

pub fn cmd_sadd(art: &mut OxidArt, key: SharedByte, members: &[SharedByte]) -> Frame {
    match art.cmd_sadd(&key, members, None) {
        Ok(count) => Frame::Integer(count as i64),
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(_) => Frame::Error("ERR internal error".into()),
    }
}

pub fn cmd_srem(art: &mut OxidArt, key: SharedByte, members: &[SharedByte]) -> Frame {
    match art.cmd_srem(&key, members) {
        Ok(count) => Frame::Integer(count as i64),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected set, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_sismember(art: &mut OxidArt, key: SharedByte, member: SharedByte) -> Frame {
    match art.cmd_sismember(&key, member) {
        Ok(exists) => Frame::Integer(if exists { 1 } else { 0 }),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected set, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_scard(art: &mut OxidArt, key: SharedByte) -> Frame {
    match art.cmd_scard(&key) {
        Ok(count) => Frame::Integer(count as i64),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected set, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_smembers(art: &mut OxidArt, key: SharedByte) -> Frame {
    match art.cmd_smembers(&key) {
        Ok(members) => Frame::Array(members.into_iter().map(Frame::BulkString).collect()),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected set, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_spop(art: &mut OxidArt, key: SharedByte, count: Option<u64>) -> Frame {
    let count_str = count.map(|n| n.to_string());
    let count_bytes = count_str.as_deref().map(str::as_bytes);
    match art.cmd_spop(&key, count_bytes) {
        Ok(SPOPResult::Single(opt)) => match opt {
            Some(val) => Frame::BulkString(val),
            None => Frame::Null,
        },
        Ok(SPOPResult::Multiple(vec)) => {
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
