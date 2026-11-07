//! Commandes bitset (BITSET_DESIGN.md §10.4) : type à part, réponses de Redis.

#[allow(unused_imports)]
use crate::prelude::*;

use oxidart::OxidArt;
use oxidart::bcommand::BitUnit;
use oxidart::value::RedisType;
use radixox_lib::shared_frame::SharedFrame as Frame;

fn wrongtype(_: RedisType) -> Frame {
    Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
}

#[inline]
fn unit(bits: bool) -> BitUnit {
    if bits { BitUnit::Bit } else { BitUnit::Byte }
}

pub fn cmd_setbit(art: &mut OxidArt, key: &[u8], off: u32, on: bool) -> Frame {
    match art.cmd_setbit(key, off, on) {
        Ok(old) => Frame::Integer(old as i64),
        Err(t) => wrongtype(t),
    }
}

pub fn cmd_getbit(art: &mut OxidArt, key: &[u8], off: u32) -> Frame {
    match art.cmd_getbit(key, off) {
        Ok(b) => Frame::Integer(b as i64),
        Err(t) => wrongtype(t),
    }
}

pub fn cmd_bitcount(art: &mut OxidArt, key: &[u8], range: Option<(i64, i64, bool)>) -> Frame {
    match art.cmd_bitcount(key, range.map(|(s, e, b)| (s, e, unit(b)))) {
        Ok(n) => Frame::Integer(n as i64),
        Err(t) => wrongtype(t),
    }
}

pub fn cmd_bitpos(art: &mut OxidArt, key: &[u8], bit: bool, start: Option<i64>, end: Option<i64>, bits: bool) -> Frame {
    match art.cmd_bitpos(key, bit, start, end, unit(bits)) {
        Ok(p) => Frame::Integer(p),
        Err(t) => wrongtype(t),
    }
}
