use oxidart::OxidArt;
use oxidart::error::TypeError;
use radixox_lib::shared_byte::SharedByte;
use radixox_lib::shared_frame::SharedFrame as Frame;

pub fn cmd_hset(art: &mut OxidArt, key: SharedByte, fields: &[(SharedByte, SharedByte)]) -> Frame {
    match art.cmd_hset(&key, fields, None) {
        Ok(added) => Frame::Integer(added as i64),
        Err(TypeError::ValueNotSet) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
        Err(_) => Frame::Error("ERR internal error".into()),
    }
}

pub fn cmd_hget(art: &mut OxidArt, key: SharedByte, field: SharedByte) -> Frame {
    match art.cmd_hget(&key, &field) {
        Ok(Some(val)) => Frame::BulkString(val),
        Ok(None) => Frame::Null,
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hgetall(art: &mut OxidArt, key: SharedByte) -> Frame {
    match art.cmd_hgetall(&key) {
        Ok(fields) => Frame::Array(fields.into_iter().map(Frame::BulkString).collect()),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hdel(art: &mut OxidArt, key: SharedByte, fields: &[SharedByte]) -> Frame {
    match art.cmd_hdel(&key, fields) {
        Ok(count) => Frame::Integer(count as i64),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hexists(art: &mut OxidArt, key: SharedByte, field: SharedByte) -> Frame {
    match art.cmd_hexists(&key, &field) {
        Ok(exists) => Frame::Integer(if exists { 1 } else { 0 }),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hlen(art: &mut OxidArt, key: SharedByte) -> Frame {
    match art.cmd_hlen(&key) {
        Ok(len) => Frame::Integer(len as i64),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hkeys(art: &mut OxidArt, key: SharedByte) -> Frame {
    match art.cmd_hkeys(&key) {
        Ok(keys) => Frame::Array(keys.into_iter().map(Frame::BulkString).collect()),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hvals(art: &mut OxidArt, key: SharedByte) -> Frame {
    match art.cmd_hvals(&key) {
        Ok(vals) => Frame::Array(vals.into_iter().map(Frame::BulkString).collect()),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}

pub fn cmd_hmget(art: &mut OxidArt, key: SharedByte, fields: &[SharedByte]) -> Frame {
    match art.cmd_hmget(&key, fields) {
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

pub fn cmd_hincrby(art: &mut OxidArt, key: SharedByte, field: SharedByte, delta: i64) -> Frame {
    match art.cmd_hincrby(&key, field, delta) {
        Ok(new_val) => Frame::Integer(new_val),
        Err(_) => {
            Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
        }
    }
}
