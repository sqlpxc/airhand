//! 信令流上的消息（长度前缀 + CBOR），和服务器读它们时走的是同一段代码（`airhand_link::wire::read_msg` 也是
//! `framing::payload_len` + `framing::decode`）
#![no_main]

use airhand_proto::MAX_SIGNAL_MSG;
use airhand_proto::framing::take_msg;
use airhand_proto::signal::ClientMsg;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut rest = data;
    while let Ok(Some(_)) = take_msg::<ClientMsg>(&mut rest, MAX_SIGNAL_MSG) {}
});
