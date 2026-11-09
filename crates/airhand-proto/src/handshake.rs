//! 端到端握手：控制通道上的头几条消息（明文 CBOR）。握手之后，会话每条通道上的每一帧、每个声音数据报都是密文。
//!
//! # 为什么要有这一层
//!
//! QUIC 的 TLS 在中转服务器终止：没有这一层，服务器（或者拿下了服务器的人）能看到画面、声音、键盘输入、剪贴板，
//! 还能往会话里插入键鼠操作。更要紧的是，控制码是服务器生成、用来路由的，**服务器知道所有控制码**，
//! 它自己就能以控制端的身份连上任何被控端。所以端到端这一层要同时做到：
//!
//! * 加密、防篡改（服务器只能转发或者丢掉）；
//! * 控制端确认对面真的是那台被控端（不是服务器冒充的）；
//! * 被控端确认这个控制端得到了授权，凭的东西服务器不知道（控制码、取回控制码的密钥、重连凭证都是服务器签发的，都不行）。
//!
//! # 身份
//!
//! 每台设备（被控端、控制端）有一对长期的 X25519 密钥，就是 Noise 的静态密钥，保存在本机（和控制码的密钥一样加密保存）。
//!
//! * 控制端记下被控端的公钥（"我的设备"，和 SSH 的 known_hosts 一样：第一次连上时记下），之后只认它。
//!   对不上就是硬错误，不会悄悄改成相信新的（可能是中间人，也可能是对方重装了，由用户决定）；
//! * 被控端记下配对过的控制端的公钥。没配对过的控制端，按被控端的设置：要求配对码（桌面版默认），
//!   请本机用户同意（手机默认），或者知道控制码就能连（等于信任中转服务器）。
//!
//! # 流程
//!
//! ```text
//! 控制端                                                  被控端
//! HandshakeInit { pattern, pake?, noise: 第 1 条 }  ──▶
//!                                ◀──  HandshakeReply { pake?, noise: 第 2 条, verdict? }
//! （First / Pairing）HandshakeFinish { noise: 第 3 条 } ──▶
//!                                ◀──  HandshakeReply { verdict }
//! ═══════════════ 之后：每条通道的每一帧都是密文 ═══════════════
//! ```
//!
//! 三种握手（[`Pattern`]）：
//!
//! * [`Pattern::Known`]：控制端记着被控端的公钥。`Noise_IK`，一个往返，和没加密时一样快（断线重连都走这个）；
//!   被控端的密钥不对，被控端解不开第 1 条，回答 [`HandshakeError::UnknownKey`]；
//! * [`Pattern::First`]：第一次连，还不知道被控端的公钥。`Noise_XX`，多半个往返，控制端记下对方的公钥；
//! * [`Pattern::Pairing`]：带着配对码。`Noise_XXpsk3`，PSK 由 SPAKE2 用配对码算出（见下面的"配对"）。
//!
//! Noise 的 prologue 是 [`prologue`]：把握手绑定到这个会话（会话 ID）和这条 [`HandshakeInit`]
//! （握手版本、模式、SPAKE2 消息），服务器改了其中任何一样，握手都会失败。
//!
//! 控制端的介绍（[`ControllerIntro`]：会话版本、能力位、名称）放在它最后一条 Noise 消息的负载里，
//! 是加密、认证过的。被控端的决定（[`HostVerdict`]：选定的会话版本、能力位，或者拒绝的原因）在被控端最后一条
//! [`HandshakeReply::verdict`] 里，已经是用会话密钥加密的第一帧（被控端 → 控制端的控制通道，计数器 0）。
//! 所以会话版本和能力位服务器既看不到、也改不了，没法让两端降级。
//!
//! # 配对
//!
//! 被控端在本机生成 [`crate::PAIRING_DIGITS`] 位数字的配对码，显示给本机用户，**从不发给服务器**
//! （服务器只知道前面的控制码，和 Magic Wormhole 的做法一样）。控制端的用户输入配对码之后：
//!
//! * 双方用 SPAKE2（Ed25519 群）把配对码变成一个 32 字节的强密钥，当作 Noise 的 PSK：
//!   配对码只有几位数字，**不能直接当 PSK**，否则中间人拿到一次握手的记录就能离线把它试出来；
//!   用 SPAKE2 时每次握手只能猜一次；
//! * SPAKE2 的密码是 [`pake_password`]（带上会话 ID 和控制码），身份是 [`PAKE_CONTROLLER`] / [`PAKE_HOST`]；
//! * 配对码不对，被控端解不开第 3 条，回答 [`HandshakeError::PairingFailed`]，记一次失败。连续失败几次之后
//!   被控端暂停配对（[`HandshakeError::PairingUnavailable`]），要本机用户换一个配对码才恢复：
//!   中间人一共只能猜那么几次；
//! * 成功之后被控端记下这个控制端的公钥，控制端记下被控端的公钥，以后不用再输配对码。
//!
//! # 安全码
//!
//! 双方都能从握手的哈希算出同一个 6 位数的安全码（[`safety_code`]），显示在界面上。第一次连接（没有配对码）时
//! 两边的人对一下：一样就说明中间没有人。
//!
//! # 之后的每一帧
//!
//! 密钥从 Noise 的结果派生（HKDF-SHA256，salt 是握手的哈希），每条通道、每个方向、数据报各用一把
//! （见 `airhand_link::secure`）。AEAD 是 ChaCha20-Poly1305：
//!
//! * 流：`u32 LE 长度 | 密文 | 16 字节标签`，nonce 是这条通道这个方向的帧计数（从 0 开始，不上线）。
//!   丢帧、重排、重放、从别的通道搬过来的帧都解不开；
//! * 数据报：`会话 ID | u64 LE 计数 | 密文 | 标签`（会话 ID 给服务器转发用），收的一方用滑动窗口挡掉重放。

use minicbor::{Decode, Encode};

use crate::Features;

/// 端到端握手的版本（握手本身的格式：[`HandshakeInit`] 和三种 Noise 握手）
pub const HANDSHAKE_VERSION: u16 = 1;
/// 本端还能说的最老的握手版本
pub const HANDSHAKE_MIN: u16 = 1;

/// AEAD 认证标签的长度
pub const TAG_LEN: usize = 16;
/// 公钥（X25519）
pub type PublicKey = [u8; 32];

/// 三种 Noise 握手的名字（握手版本 1）
pub const NOISE_KNOWN: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
pub const NOISE_FIRST: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
pub const NOISE_PAIRING: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";

/// SPAKE2 里的两个身份
pub const PAKE_CONTROLLER: &[u8] = b"airhand controller";
pub const PAKE_HOST: &[u8] = b"airhand host";

/// 握手的模式（见模块说明）
#[derive(Encode, Decode, Debug, Clone, Copy, PartialEq, Eq)]
#[cbor(index_only)]
pub enum Pattern {
    /// 控制端记着被控端的公钥：`Noise_IK`
    #[n(0)]
    Known,
    /// 第一次连：`Noise_XX`
    #[n(1)]
    First,
    /// 带着配对码：SPAKE2 + `Noise_XXpsk3`
    #[n(2)]
    Pairing,
}

impl Pattern {
    pub fn noise(self) -> &'static str {
        match self {
            Pattern::Known => NOISE_KNOWN,
            Pattern::First => NOISE_FIRST,
            Pattern::Pairing => NOISE_PAIRING,
        }
    }
}

/// 控制端 → 被控端：控制通道上的第一条消息
#[derive(Encode, Decode, Debug, Clone)]
pub struct HandshakeInit {
    #[n(0)]
    pub version: u16,
    /// 不认识的模式（对方版本新增的）是 `None`，被控端回答 [`HandshakeError::Unsupported`]
    #[cbor(n(1), with = "crate::cbor::opt", has_nil)]
    pub pattern: Option<Pattern>,
    /// SPAKE2 的第一条消息（配对时）
    #[cbor(n(2), with = "minicbor::bytes")]
    pub pake: Option<Vec<u8>>,
    /// Noise 的第 1 条消息
    #[cbor(n(3), with = "minicbor::bytes")]
    pub noise: Vec<u8>,
}

/// 被控端 → 控制端（握手期间的每一条）
#[derive(Encode, Decode, Debug, Clone, Default)]
#[cbor(map)]
pub struct HandshakeReply {
    /// 握手失败的原因（明文：这时还没有共同的密钥，服务器也能伪造它，所以只用来提示，不改变任何信任）
    #[cbor(n(0), with = "crate::cbor::opt", has_nil)]
    pub error: Option<HandshakeError>,
    /// SPAKE2 的第二条消息（配对时）
    #[cbor(n(1), with = "minicbor::bytes")]
    pub pake: Option<Vec<u8>>,
    /// Noise 的第 2 条消息
    #[cbor(n(2), with = "minicbor::bytes")]
    pub noise: Option<Vec<u8>>,
    /// 加密的 [`HostVerdict`]：控制通道上被控端 → 控制端的第一帧密文（不带长度前缀）。握手到此结束
    #[cbor(n(3), with = "minicbor::bytes")]
    pub verdict: Option<Vec<u8>>,
}

/// 控制端 → 被控端：Noise 的第 3 条消息（[`Pattern::First`] / [`Pattern::Pairing`]）
#[derive(Encode, Decode, Debug, Clone)]
pub struct HandshakeFinish {
    #[cbor(n(0), with = "minicbor::bytes")]
    pub noise: Vec<u8>,
}

/// 握手失败（明文，见 [`HandshakeReply::error`]）
#[derive(Encode, Decode, Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeError {
    /// 被控端不支持这个握手版本 / 模式：它支持 `min..=max`
    #[n(0)]
    Unsupported {
        #[n(0)]
        min: u16,
        #[n(1)]
        max: u16,
    },
    /// 被控端解不开：控制端记着的被控端公钥不对（对方重装过、换了身份，或者有人在中间冒充）
    #[n(1)]
    UnknownKey,
    /// 配对码不对
    #[n(2)]
    PairingFailed,
    /// 被控端现在不接受配对（没有配对码，或者输错太多次、暂停了）
    #[n(3)]
    PairingUnavailable,
    /// 消息格式不对
    #[n(4)]
    Malformed,
}

/// 控制端的介绍，在控制端最后一条 Noise 消息的负载里（加密、认证过）
#[derive(Encode, Decode, Debug, Clone, PartialEq, Eq)]
#[cbor(map)]
pub struct ControllerIntro {
    /// 控制端能说的会话版本
    #[n(0)]
    pub min: u16,
    #[n(1)]
    pub max: u16,
    /// 会话层的能力位（[`crate::features::session`]）
    #[n(2)]
    pub features: Features,
    /// 控制端的名称（空字符串表示不知道）和系统
    #[n(3)]
    pub name: String,
    #[n(4)]
    pub os: String,
}

/// 被控端的决定（加密，见 [`HandshakeReply::verdict`]）
#[derive(Encode, Decode, Debug, Clone, PartialEq, Eq)]
#[cbor(map)]
pub struct HostVerdict {
    /// 选定的会话版本；`None` 表示拒绝（原因在 `refused`）
    #[n(0)]
    pub version: Option<u16>,
    /// 被控端的会话能力位
    #[n(1)]
    pub features: Features,
    /// 拒绝的原因；不认识的原因是 `None`（这时 `version` 也是 `None`）
    #[cbor(n(2), with = "crate::cbor::opt", has_nil)]
    pub refused: Option<Refusal>,
    /// 被控端能说的会话版本（不兼容时告诉用户该更新哪一边）
    #[n(3)]
    pub min: u16,
    #[n(4)]
    pub max: u16,
}

/// 被控端为什么拒绝
#[derive(Encode, Decode, Debug, Clone, Copy, PartialEq, Eq)]
#[cbor(index_only)]
pub enum Refusal {
    /// 没有双方都能说的会话版本
    #[n(0)]
    Incompatible,
    /// 这个控制端没配对过，被控端要求配对码
    #[n(1)]
    PairingRequired,
}

/// Noise 的 prologue：把握手绑定到这个会话和这条 [`HandshakeInit`]（除了 Noise 消息本身）
pub fn prologue(session_id: u64, init: &HandshakeInit) -> Vec<u8> {
    let mut p = b"airhand e2e".to_vec();
    p.extend_from_slice(&init.version.to_le_bytes());
    p.extend_from_slice(&session_id.to_le_bytes());
    p.push(match init.pattern {
        Some(Pattern::Known) => 0,
        Some(Pattern::First) => 1,
        Some(Pattern::Pairing) => 2,
        None => 0xff,
    });
    if let Some(pake) = &init.pake {
        p.extend_from_slice(&(pake.len() as u32).to_le_bytes());
        p.extend_from_slice(pake);
    }
    p
}

/// SPAKE2 的密码：配对码，加上会话 ID 和控制码（同一个配对码在别的会话、别的控制码上算出来的不一样）
pub fn pake_password(session_id: u64, code: u32, pairing: &str) -> Vec<u8> {
    let mut p = b"airhand pairing v1".to_vec();
    p.extend_from_slice(&session_id.to_le_bytes());
    p.extend_from_slice(&code.to_le_bytes());
    p.extend_from_slice(pairing.as_bytes());
    p
}

/// 安全码：从两端一样的一段随机字节（`airhand_link` 从握手的哈希派生）取 6 位数，写成 `123-456`
pub fn safety_code(seed: &[u8; 32]) -> String {
    let n = u64::from_le_bytes(seed[..8].try_into().unwrap()) % 1_000_000;
    format!("{:03}-{:03}", n / 1000, n % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prologue_binds_everything_but_the_noise_message() {
        let init = HandshakeInit {
            version: 1,
            pattern: Some(Pattern::Pairing),
            pake: Some(vec![1, 2, 3]),
            noise: vec![9; 32],
        };
        let p = prologue(7, &init);
        let other_noise = HandshakeInit {
            noise: vec![8; 32],
            ..init.clone()
        };
        assert_eq!(prologue(7, &other_noise), p);
        assert_ne!(prologue(8, &init), p);
        let other_pake = HandshakeInit {
            pake: Some(vec![1, 2, 4]),
            ..init.clone()
        };
        assert_ne!(prologue(7, &other_pake), p);
        let other_pattern = HandshakeInit {
            pattern: Some(Pattern::First),
            pake: None,
            ..init
        };
        assert_ne!(prologue(7, &other_pattern), p);
    }

    #[test]
    fn safety_codes_look_right() {
        let c = safety_code(&[0xab; 32]);
        assert_eq!(c.len(), 7);
        assert_eq!(&c[3..4], "-");
        assert_ne!(safety_code(&[1; 32]), safety_code(&[2; 32]));
        assert_eq!(safety_code(&[0; 32]), "000-000");
    }

    #[test]
    fn unknown_patterns_and_refusals_are_none() {
        let mut e = minicbor::Encoder::new(Vec::new());
        e.array(4).unwrap();
        e.u16(1)
            .unwrap()
            .u32(9)
            .unwrap()
            .null()
            .unwrap()
            .bytes(&[1])
            .unwrap();
        let init: HandshakeInit = minicbor::decode(&e.into_writer()).unwrap();
        assert_eq!(init.pattern, None);

        let mut e = minicbor::Encoder::new(Vec::new());
        e.map(5).unwrap();
        e.u32(0).unwrap().null().unwrap();
        e.u32(1).unwrap().u64(0).unwrap();
        e.u32(2).unwrap().u32(50).unwrap();
        e.u32(3).unwrap().u16(1).unwrap();
        e.u32(4).unwrap().u16(3).unwrap();
        let v: HostVerdict = minicbor::decode(&e.into_writer()).unwrap();
        assert_eq!((v.version, v.refused, v.max), (None, None, 3));
    }
}
