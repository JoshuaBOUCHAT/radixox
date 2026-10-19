use oxidart::OxidArt;
use oxidart::error::TypeError;
use radixox_lib::shared_byte::SharedByte;
use radixox_lib::shared_frame::SharedFrame as Frame;

pub fn cmd_zadd(art: &mut OxidArt, key: &[u8], score_members: &[(f64, SharedByte)]) -> Frame {
    // cmd_zadd peut créer le nœud zset : oxidart exige une clé possédée ici.
    match art.cmd_zadd(SharedByte::from_slice(key), score_members, None) {
        Ok(added) => Frame::Integer(added as i64),
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(_) => Frame::Error("ERR internal error".into()),
    }
}

pub fn cmd_zcard(art: &mut OxidArt, key: &[u8]) -> Frame {
    match art.cmd_zcard(key) {
        Ok(count) => Frame::Integer(count as i64),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected zset, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_zrange(
    art: &mut OxidArt,
    key: &[u8],
    start: i64,
    stop: i64,
    with_scores: bool,
) -> Frame {
    match art.cmd_zrange(key, start, stop, with_scores) {
        Ok(result) => Frame::Array(result.into_iter().map(Frame::BulkString).collect()),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected zset, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_zscore(art: &mut OxidArt, key: &[u8], member: SharedByte) -> Frame {
    match art.cmd_zscore(key, member) {
        Ok(Some(score)) => Frame::BulkString(SharedByte::from_slice(score.to_string().as_bytes())),
        Ok(None) => Frame::Null,
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected zset, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_zrem(art: &mut OxidArt, key: &[u8], members: &[SharedByte]) -> Frame {
    match art.cmd_zrem(key, members) {
        Ok(removed) => Frame::Integer(removed as i64),
        Err(redis_type) => Frame::Error(format!(
            "WRONGTYPE Operation against a key holding the wrong kind of value (expected zset, got {})",
            redis_type.as_str()
        )),
    }
}

pub fn cmd_zincrby(art: &mut OxidArt, key: &[u8], delta: f64, member: SharedByte) -> Frame {
    // cmd_zincrby peut créer le nœud zset : oxidart exige une clé possédée ici.
    match art.cmd_zincrby(SharedByte::from_slice(key), delta, member) {
        Ok(new_score) => {
            Frame::BulkString(SharedByte::from_slice(new_score.to_string().as_bytes()))
        }
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(_) => Frame::Error("ERR internal error".into()),
    }
}
