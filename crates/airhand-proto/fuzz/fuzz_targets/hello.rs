//! 版本握手：客户端连上之后的第一条消息（服务器对谁都要读）。和服务器一样先只读版本区间，有交集才读整条
#![no_main]

use airhand_proto::features;
use airhand_proto::framing::{decode_known, to_vec};
use airhand_proto::signal::{ClientHello, ServerHello, Versions};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(v) = decode_known::<Versions>(data) {
        // 服务器接着就要回答它
        let answer = ServerHello::answer(v, features::signal::SUPPORTED);
        let _ = to_vec(&answer);
        if answer.version.is_some() {
            let _ = decode_known::<ClientHello>(data);
        }
    }
});
