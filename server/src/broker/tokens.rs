//! 凭证：服务器签发的东西，以及怎么认它们。
//!
//! * 控制码的密钥（[`Broker::issue_secret`]）：被控端取回控制码要出示它。密钥由服务器签名，服务器重启之后、
//!   控制码空闲时也认得是不是本服务器为**这个控制码**签发的，所以注册不能用来探测控制码有没有人用；
//! * 重连凭证（[`Broker::resume_token`]）：控制端连上过这个控制码的证明，被控端换了密钥就作废。
//!
//! 比较一律用常数时间（[`ct_eq`]）：不能靠测量回答的快慢逐字节猜出来。

use airhand_proto::signal::ResumeToken;
use sha2::{Digest, Sha256};

use super::Broker;
use crate::codes::Owner;

/// 被控端取回控制码用的密钥：现在的，以及换密钥之后被控端可能还没保存、所以暂时也认的旧密钥
#[derive(Clone, Copy)]
pub(super) struct Secrets {
    pub(super) cur: [u8; 16],
    pub(super) prev: Option<[u8; 16]>,
}

impl Secrets {
    pub(super) fn new(cur: [u8; 16]) -> Self {
        Self { cur, prev: None }
    }

    pub(super) fn owner(&self, secret: &[u8; 16]) -> Owner {
        if ct_eq(&self.cur, secret) {
            Owner::Current
        } else if self.prev.is_some_and(|p| ct_eq(&p, secret)) {
            Owner::Previous
        } else {
            Owner::Other
        }
    }
}

/// 重连凭证的前一半：服务器为控制码 `code` 签的名（见 [`Broker::resume_token`]）
fn token_proof(key: &[u8; 32], code: u32) -> [u8; 8] {
    let mac = hmac_sha256(key, &[b"airhand resume proof v1\0", &code.to_le_bytes()]);
    mac[..8].try_into().unwrap()
}

/// 重连凭证的后一半：只对密钥 `secret` 有效（见 [`Broker::resume_token`]）
fn token_auth(code: u32, secret: &[u8; 16]) -> [u8; 8] {
    let mut h = Sha256::new();
    h.update(b"airhand resume token v2\0");
    h.update(code.to_le_bytes());
    h.update(secret);
    h.finalize()[..8].try_into().unwrap()
}

/// 比较凭证、密钥，用时与内容无关（不能靠测量回答的快慢逐字节猜出来）
pub(super) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// HMAC-SHA256（RFC 2104）。密钥 32 字节，短于 SHA-256 的块长，直接补零
pub(super) fn hmac_sha256(key: &[u8; 32], parts: &[&[u8]]) -> [u8; 32] {
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for (i, k) in key.iter().enumerate() {
        ipad[i] ^= k;
        opad[i] ^= k;
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    for p in parts {
        inner.update(p);
    }
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// 控制码密钥的签名：`密钥 = 随机数（8 字节）| HMAC(服务器密钥, 控制码 | 随机数) 的前 8 字节`
fn secret_mac(key: &[u8; 32], code: u32, nonce: &[u8]) -> [u8; 8] {
    let mac = hmac_sha256(
        key,
        &[b"airhand code secret v1\0", &code.to_le_bytes(), nonce],
    );
    mac[..8].try_into().unwrap()
}

impl Broker {
    /// 控制码 `code`、密钥 `secret` 对应的重连凭证：
    ///
    /// * 前一半是服务器用自己的密钥给控制码签的名（[`token_proof`]）：证明拿着它的控制端连上过这个控制码，
    ///   服务器重启之后也认得，被控端不在线也能验证（[`Broker::vouches`]）；
    /// * 后一半由控制码和被控端的密钥算出（[`token_auth`]）：密钥只有服务器和被控端知道，所以只能从服务器那里得到，
    ///   也不能从它反推出密钥（取回控制码要用）。被控端取回控制码时密钥不变，凭证仍然有效（包括服务器重启之后）；
    ///   换了密钥（[`ClientMsg::Rekey`]）就作废了。
    pub(super) fn resume_token(&self, code: u32, secret: &[u8; 16]) -> ResumeToken {
        let mut t = [0u8; 16];
        t[..8].copy_from_slice(&token_proof(&self.key, code));
        t[8..].copy_from_slice(&token_auth(code, secret));
        t
    }

    /// `token` 是不是本服务器给控制码 `code` 的控制端发的（不管现在还有没有效）
    pub(super) fn vouches(&self, code: u32, token: &ResumeToken) -> bool {
        ct_eq(&token[..8], &token_proof(&self.key, code))
    }

    /// 为控制码 `code` 签发一个新的密钥（见 [`secret_mac`]）
    pub(super) fn issue_secret(&self, code: u32) -> [u8; 16] {
        let nonce: [u8; 8] = rand::random();
        let mut s = [0u8; 16];
        s[..8].copy_from_slice(&nonce);
        s[8..].copy_from_slice(&secret_mac(&self.key, code, &nonce));
        s
    }

    /// `secret` 是不是本服务器为 `code` 签发的（服务器重启之后也认得）
    pub(super) fn issued_here(&self, code: u32, secret: &[u8; 16]) -> bool {
        ct_eq(&secret[8..], &secret_mac(&self.key, code, &secret[..8]))
    }
}
