//! 画面的纠错数据报（中转原样转发，控制端解析之后按里面的块数、长度分配内存，见 `airhand_proto::fec`）
#![no_main]

use airhand_proto::fec::{Fragment, MAX_FRAME, MAX_SHARD, max_repair};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Some(f) = Fragment::parse(data) {
        // 解得出来的都在上限里面，编回去就是原来的字节
        let (k, r) = (f.k as usize, f.r as usize);
        assert!(f.shard.len() <= MAX_SHARD && (f.len as usize) <= MAX_FRAME);
        assert!(r <= max_repair(k) && (f.index as usize) < k + r);
        let session = u64::from_le_bytes(data[..8].try_into().unwrap());
        assert_eq!(f.encode(session), data);
    }
});
