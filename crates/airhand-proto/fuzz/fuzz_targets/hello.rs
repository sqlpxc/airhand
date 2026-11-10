//! 版本握手：客户端连上之后的第一条消息（服务器对谁都要读）
#![no_main]

use airhand_proto::signal::ClientHello;
use airhand_proto::{SIGNAL_MIN, SIGNAL_VERSION, features};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(hello) = ClientHello::decode(data) {
        // 服务器接着就要回答它
        let answer = hello.negotiate(SIGNAL_MIN, SIGNAL_VERSION, features::signal::SUPPORTED);
        let _ = answer.encode();
    }
});
