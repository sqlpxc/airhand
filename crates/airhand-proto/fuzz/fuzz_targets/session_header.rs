//! 通道流开头的 16 字节（服务器按它找到会话的另一端）
#![no_main]

use airhand_proto::SessionHeader;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(b) = <&[u8; SessionHeader::LEN]>::try_from(data)
        && let Some(h) = SessionHeader::decode(b)
    {
        // 解得出来的都能编回去（后两个保留字节编成 0）
        assert_eq!(h.encode()[..14], b[..14]);
    }
});
