//! 会话流开头的 16 字节（服务器按它配对两端的流）
#![no_main]

use airhand_proto::SessionHeader;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(b) = <&[u8; SessionHeader::LEN]>::try_from(data)
        && let Some(h) = SessionHeader::decode(b)
    {
        // 解得出来的都能原样编回去
        assert_eq!(&h.encode(), b);
    }
});
