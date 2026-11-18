//! 信令握手的两个方向都只解析一条 Hello。
#![no_main]

use airhand_proto::framing::decode_known;
use airhand_proto::signal::{ClientHello, ServerHello};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = decode_known::<ClientHello>(data);
    let _ = decode_known::<ServerHello>(data);
});
