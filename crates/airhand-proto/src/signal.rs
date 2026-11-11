//! 信令流（客户端 ⇄ 服务器）的消息。
//!
//! 服务器发给客户端的都是类型（例如 [`ConnectError`]），不是文字：客户端按自己的界面语言显示。
//!
//! # 版本握手
//!
//! 连接后客户端先发 [`ClientHello`]，服务器回 [`ServerHello`]。两条都是 CBOR map，**0、1 号键永远是版本区间**
//! （[`Versions`]），这是信令协议里唯一冻结的部分：服务器先只读这两个键（map 里别的键不管）、算出双方都能说的最高版本，
//! 没有交集就回答 `version: None` 和自己的区间，客户端据此提示该更新哪一边。所以任何两个版本之间都能互相告知"不兼容"，
//! 而握手里的其它字段照样可以按 [`crate::framing`] 的规则增减。
//!
//! 握手之后的消息（[`ClientMsg`] / [`ServerMsg`]）是带编号的 CBOR，不认识的跳过（见 [`crate::framing`]）。
//!
//! # 发起会话
//!
//! 控制端的 [`ClientMsg::Connect`] 带着端到端握手的第一条消息（[`crate::handshake::HandshakeInit`]，对服务器是不透明的字节），
//! 服务器批准之后在 [`ServerMsg::Incoming`] 里原样交给被控端：被控端收到通知时就能回答握手，不用等控制端先拿到批准、
//! 再开流、再发第一条消息（省一个往返）。
//!
//! # 重连凭证（resume tokens）
//!
//! 控制端连上被控端时，服务器在 [`SessionGrant::resume`] 里给一个重连凭证。凭证有两半：
//!
//! * 前一半是服务器用自己的密钥给**控制码**签的名：证明拿着它的控制端连上过这个控制码（服务器重启之后也认得，
//!   被控端在不在线都能验证）。拿着它的人本来就知道这个控制码，所以带凭证的请求不算猜码，也不受猜码限速的影响
//!   （向同一台被控端发起会话的限速照样算：凭证只说明它不是在猜码）；
//! * 后一半由控制码和被控端的密钥算出（没有密钥算不出来，也不能反推出密钥）：只对被控端现在的密钥有效。
//!
//! 被控端掉线后，控制端带着凭证重连（[`ClientMsg::Connect::resume`]），服务器的回答：
//!
//! * 前一半不是本服务器为这个控制码签的（伪造的、别的服务器发的）：[`ConnectError::Revoked`]。
//!   这个回答只取决于凭证本身，和控制码在不在、有没有人用都无关，所以凭证不能用来猜码；
//! * 被控端在线、凭证有效：照常建立会话；
//! * 被控端在线、后一半对不上（被控端换过密钥、控制码换了主人）：[`ConnectError::Revoked`]；
//! * 被控端不在线：[`ConnectError::Offline`]。服务器记下这个请求，被控端用这个控制码上线时发
//!   [`ServerMsg::HostOnline`]，控制端收到后马上重连。等待期间控制端不需要反复尝试。
//!
//! 收到 `Revoked` 的控制端不应该自动重连（被控端的用户断开了所有控制端）；用户自己要连的（例如从"我的设备"打开）
//! 就丢掉凭证、用控制码直接连。被控端不在线时控制端只需要等，不必为了排除"凭证作废了"再用控制码试一次
//! （那样会算一次猜错）。
//!
//! 被控端取回原来的控制码（包括服务器重启之后）时凭证仍然有效；换了控制码、或者被控端的用户断开了所有控制端
//! （被控端随即发 [`ClientMsg::Rekey`] 换一个密钥）时，之前发出的凭证全部作废。被控端发了 `Rekey` 却没等到
//! [`ServerMsg::Rekeyed`] 就断线了的话，下次注册时带上 [`ClientMsg::Register::rekey`]：服务器在还给它控制码的
//! 同时换密钥。否则服务器可能根本没收到 `Rekey`，或者收到之后还没记下来就重启了，手里的旧密钥又成了有效的，
//! 旧凭证也跟着恢复。
//!
//! 重连凭证只决定**服务器**让不让这个请求通过（不算猜码）。被控端让不让这个控制端操作，由端到端握手决定
//! （见 [`crate::handshake`]）：服务器知道所有控制码，不能只凭控制码放行。
//!
//! # 密钥
//!
//! 密钥由服务器生成并签名（服务器自己的密钥做 HMAC），服务器重启之后也认得出是自己发的。被控端取回控制码时：
//! 控制码在线 / 保留中 / 绑定着，密钥必须对得上；控制码空闲（例如服务器重启过），密钥必须是服务器为**这个控制码**签发的。
//! 对不上就重新分配一个，所以不能用注册来探测某个控制码有没有人在用。
//!
//! # 固定控制码
//!
//! 被控端把控制码和密钥保存下来（桌面客户端保存在 `config.toml` 里，Android 保存在应用的私有目录里）时，注册时带上
//! [`ClientMsg::Register::keep`]：服务器把这个控制码**绑定**到这台被控端（只记密钥的哈希，存在服务器的 SQLite 数据库里），
//! 被控端离线多久、服务器重启多少次，都不会把它分给别人，被控端上线时总能取回。很久（见服务器的
//! `BINDING_TTL`）没有上线的绑定才会被清理。被控端更换控制码时先发 [`ClientMsg::Release`] 解除绑定。
//! 不保存控制码的被控端（`keep = false`）掉线后控制码只保留一小段时间。
//!
//! # 公网地址
//!
//! 服务器在会话开始时告诉每一端**它自己**的公网地址（服务器看到的这条连接的来源地址，[`ServerMsg::Incoming::observed`]、
//! [`SessionGrant::observed`]）：两端打洞直连要用（见 crate 说明的"直连"）。服务器不把一端的地址告诉另一端：
//! 两端在会话开始之后（被控端的用户同意了、端到端握手确认了对方）才在加密的控制通道上交换地址。

use std::net::{IpAddr, SocketAddr};

use minicbor::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::cbor::open_enum;
use crate::{Features, Role};

// ───────────────────────────── 版本握手 ─────────────────────────────

/// 版本握手里冻结的部分：[`ClientHello`] / [`ServerHello`] 的 0、1 号键（见模块说明的"版本握手"）
#[derive(Encode, Decode, Debug, Clone, Copy, PartialEq, Eq)]
#[cbor(map)]
pub struct Versions {
    /// 能说的最老的版本
    #[n(0)]
    pub min: u16,
    /// 能说的最新的版本
    #[n(1)]
    pub max: u16,
}

impl Versions {
    /// 本端能说的信令版本
    pub const SIGNAL: Versions = Versions {
        min: crate::SIGNAL_MIN,
        max: crate::SIGNAL_VERSION,
    };

    /// 双方都能说的最高版本；没有交集时为 `None`
    pub fn common(self, other: Versions) -> Option<u16> {
        let v = self.max.min(other.max);
        (v >= self.min.max(other.min)).then_some(v)
    }
}

/// 客户端连上之后的第一条消息（见模块说明的"版本握手"）。
///
/// `name` 为空表示不知道本机名称，由显示的一方按自己的语言显示"未知设备"。
#[derive(Encode, Decode, Debug, Clone, PartialEq, Eq)]
#[cbor(map)]
pub struct ClientHello {
    #[n(0)]
    pub min: u16,
    #[n(1)]
    pub max: u16,
    #[n(2)]
    pub role: Role,
    #[n(3)]
    pub name: String,
    #[n(4)]
    pub os: String,
    /// 信令层的能力位（[`crate::features::signal`]）
    #[cbor(n(5), default)]
    pub features: Features,
}

impl ClientHello {
    /// 本端的握手
    pub fn new(role: Role, name: String, os: String) -> Self {
        Self {
            min: Versions::SIGNAL.min,
            max: Versions::SIGNAL.max,
            role,
            name,
            os,
            features: crate::features::signal::SUPPORTED,
        }
    }

    pub fn versions(&self) -> Versions {
        Versions {
            min: self.min,
            max: self.max,
        }
    }
}

/// 服务器对 [`ClientHello`] 的回答（见模块说明的"版本握手"）
#[derive(Encode, Decode, Debug, Clone, PartialEq, Eq)]
#[cbor(map)]
pub struct ServerHello {
    /// 服务器能说的信令版本
    #[n(0)]
    pub min: u16,
    #[n(1)]
    pub max: u16,
    /// 之后按这个版本说话；`None`：没有双方都能说的版本，客户端应停止重试，按 `min..=max` 提示用户更新哪一边
    #[n(2)]
    pub version: Option<u16>,
    /// 服务器的信令能力位
    #[cbor(n(3), default)]
    pub features: Features,
}

impl ServerHello {
    /// 服务器怎么回答能说 `client` 的客户端：双方都能说的最高版本
    pub fn answer(client: Versions, features: Features) -> Self {
        let ours = Versions::SIGNAL;
        Self {
            min: ours.min,
            max: ours.max,
            version: ours.common(client),
            features,
        }
    }
}

// ───────────────────────────── 之后的消息 ─────────────────────────────

/// 客户端 → 服务器（版本握手之后）。变体的编号是线上的类型，不能改、不能复用
#[derive(Encode, Decode, Debug, Clone)]
pub enum ClientMsg {
    /// 被控端申请控制码。断线重连时携带上次的控制码与密钥，以便取回同一个控制码。
    ///
    /// `keep`：被控端会保存控制码（重启后也带着它来取回），服务器应该一直为它保留（见模块说明的"固定控制码"）。
    ///
    /// `rekey`：上次发的 [`ClientMsg::Rekey`] 没有等到回答：取回控制码时换一个密钥，`reclaim` 里的密钥
    /// 和用它算出的重连凭证随之作废（见模块说明的"重连凭证"）。
    #[n(1)]
    Register {
        #[n(0)]
        reclaim: Option<Reclaim>,
        #[n(1)]
        keep: bool,
        #[n(2)]
        rekey: bool,
    },
    /// 控制端请求连接某个控制码对应的被控端。
    ///
    /// `resume`：之前连上这个控制码时得到的重连凭证（见模块说明的"重连凭证"），第一次连接时为 `None`。
    /// `init`：端到端握手的第一条消息（见模块说明的"发起会话"），最多 [`crate::handshake::MAX_MSG`] 字节
    #[n(2)]
    Connect {
        #[n(0)]
        req_id: u32,
        #[n(1)]
        code: u32,
        #[cbor(n(2), with = "minicbor::bytes")]
        resume: Option<ResumeToken>,
        #[cbor(n(3), with = "minicbor::bytes")]
        init: Vec<u8>,
    },
    /// 被控端放弃当前的控制码（托盘"更换控制码"）：解除绑定，之后断开连接、不带 `reclaim` 重新注册。
    #[n(3)]
    Release,
    /// 被控端的用户断开了所有控制端：换一个密钥，之前发出的重连凭证全部作废（被断开的控制端不能带着凭证马上连回来）。
    /// 服务器回 [`ServerMsg::Rekeyed`]，被控端保存新的密钥，下次取回控制码时用它。
    /// `secret` 是本机已保存的密钥：同一把密钥的重复请求返回同一个结果；
    /// 收到并保存回答后才用新密钥发下一次请求。
    #[n(4)]
    Rekey {
        #[cbor(n(0), with = "minicbor::bytes")]
        secret: [u8; 16],
    },
}

/// 重连凭证（见模块说明）。对客户端来说是不透明的，原样带回即可
pub type ResumeToken = [u8; 16];

#[derive(Encode, Decode, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reclaim {
    #[n(0)]
    pub code: u32,
    #[cbor(n(1), with = "minicbor::bytes")]
    pub secret: [u8; 16],
}

/// 服务器 → 客户端（版本握手之后）。变体的编号是线上的类型，不能改、不能复用
#[derive(Encode, Decode, Debug, Clone)]
pub enum ServerMsg {
    /// 被控端注册成功。
    #[n(1)]
    Registered {
        #[n(0)]
        code: u32,
        #[cbor(n(1), with = "minicbor::bytes")]
        secret: [u8; 16],
    },
    /// 对 [`ClientMsg::Connect`] 的应答：成功时有 `grant`，失败时有 `error`（服务器的版本更新时可能是
    /// [`ConnectError::Unknown`]）。用 [`ServerMsg::connect_result`] 构造、[`connect_outcome`] 读
    #[n(2)]
    ConnectResult {
        #[n(0)]
        req_id: u32,
        #[n(1)]
        grant: Option<SessionGrant>,
        #[n(2)]
        error: Option<ConnectError>,
    },
    /// 通知被控端：有控制端发起会话。`init` 是控制端的端到端握手的第一条消息（[`ClientMsg::Connect::init`]），
    /// 被控端在会话的控制通道上回答它；`observed`：服务器看到的被控端自己的地址（见模块说明的"公网地址"）
    #[n(3)]
    Incoming {
        #[n(0)]
        session_id: u64,
        #[n(1)]
        peer: Peer,
        #[n(2)]
        observed: SocketAddr,
        #[cbor(n(3), with = "minicbor::bytes")]
        init: Vec<u8>,
    },
    /// 通知控制端：它在等的被控端（收到过 [`ConnectError::Offline`]）重新上线了，可以重连
    ///（凭证仍然有效的话就连上了，否则回答 [`ConnectError::Revoked`]）。
    /// 每次 `Offline` 回答最多换来一次通知；控制端与服务器的连接断了要重新请求。
    #[n(4)]
    HostOnline {
        #[n(0)]
        code: u32,
    },
    /// 对 [`ClientMsg::Rekey`] 的回答：控制码不变，取回它的密钥换成了这个（已经保存到服务器的数据库里）
    #[n(5)]
    Rekeyed {
        #[cbor(n(0), with = "minicbor::bytes")]
        secret: [u8; 16],
    },
}

impl ServerMsg {
    pub fn connect_result(req_id: u32, result: Result<SessionGrant, ConnectError>) -> Self {
        let (grant, error) = match result {
            Ok(g) => (Some(g), None),
            Err(e) => (None, Some(e)),
        };
        ServerMsg::ConnectResult {
            req_id,
            grant,
            error,
        }
    }
}

/// [`ServerMsg::ConnectResult`] 的结果。两个都没有（不该发生）当作 [`ConnectError::Unknown`]
pub fn connect_outcome(
    grant: Option<SessionGrant>,
    error: Option<ConnectError>,
) -> Result<SessionGrant, ConnectError> {
    match (grant, error) {
        (Some(g), _) => Ok(g),
        (None, e) => Err(e.unwrap_or(ConnectError::Unknown(u32::MAX))),
    }
}

/// 发起会话的控制端（服务器看到的：名称是控制端自己报的，IP 是服务器看到的）
#[derive(Encode, Decode, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// 计算机名，为空表示对方不知道自己的名称
    #[n(0)]
    pub name: String,
    #[n(1)]
    pub ip: IpAddr,
}

#[derive(Encode, Decode, Debug, Clone)]
pub struct SessionGrant {
    #[n(0)]
    pub session_id: u64,
    #[n(1)]
    pub host_name: String,
    #[n(2)]
    pub host_os: String,
    /// 重连凭证，被控端掉线后重连时带上
    #[cbor(n(3), with = "minicbor::bytes")]
    pub resume: ResumeToken,
    /// 服务器看到的控制端自己的地址（见模块说明的"公网地址"）
    #[n(4)]
    pub observed: SocketAddr,
}

open_enum! {
    /// 服务器为什么不让连。不认识的原因（服务器的版本更新）是 `Unknown`
    pub enum ConnectError {
        /// 控制码不存在或对方不在线
        NotFound = 0 => "notFound",
        /// 尝试过于频繁：猜错控制码太多次，或者短时间内向同一台被控端发起了太多次会话（带凭证的也算）
        RateLimited = 1 => "rateLimited",
        /// 对方会话数已满
        Busy = 2 => "busy",
        /// 对方暂时不在线，上线时会收到 [`ServerMsg::HostOnline`]（只回答带凭证的请求，见模块说明）
        Offline = 3 => "offline",
        /// 重连凭证作废了（被控端的用户断开过所有控制端、控制码换了主人），或者不是这台服务器为这个控制码发的
        ///（只回答带凭证的请求，见模块说明）
        Revoked = 4 => "revoked",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 版本握手冻结的部分：两条消息都是 map，0、1 号键是版本区间。这个测试守着它：
    /// 如果它失败了，说明改动会让其它版本的客户端 / 服务器连"版本不一致"都无法得知
    #[test]
    fn version_handshake_is_frozen() {
        let hello = ClientHello {
            min: 9,
            max: 11,
            role: Role::Controller,
            name: "A".into(),
            os: "B".into(),
            features: Features::NONE,
        };
        let wire = minicbor::to_vec(&hello).unwrap();
        // map(6) | 0: 9 | 1: 11 | ...
        assert_eq!(wire[..5], [0xa6, 0x00, 0x09, 0x01, 0x0b]);
        assert_eq!(
            minicbor::decode::<Versions>(&wire).unwrap(),
            Versions { min: 9, max: 11 }
        );
        let answer = ServerHello::answer(hello.versions(), Features::NONE);
        let wire = minicbor::to_vec(&answer).unwrap();
        assert_eq!(wire[..5], [0xa4, 0x00, 0x09, 0x01, 0x09]);
        assert_eq!(answer.version, Some(9));
        assert_eq!(crate::ALPN, b"airhand/1");
    }

    /// 以后的版本在握手里加了字段、换了别的东西：照样读得出版本区间，照样能回答"不兼容"
    #[test]
    fn any_version_can_be_told_it_is_incompatible() {
        #[derive(Encode)]
        #[cbor(map)]
        struct FutureHello {
            #[n(0)]
            min: u16,
            #[n(1)]
            max: u16,
            #[n(2)]
            role: String,
            #[n(9)]
            hologram: Vec<u8>,
        }
        let wire = minicbor::to_vec(FutureHello {
            min: 12,
            max: 14,
            role: "quantum".into(),
            hologram: vec![1, 2],
        })
        .unwrap();
        let v: Versions = minicbor::decode(&wire).unwrap();
        assert!(minicbor::decode::<ClientHello>(&wire).is_err());
        let answer = ServerHello::answer(v, Features::NONE);
        assert_eq!(answer.version, None);
        assert_eq!(
            (answer.min, answer.max),
            (Versions::SIGNAL.min, Versions::SIGNAL.max)
        );
        // 将来的服务器少发了能力位：照样读得懂
        #[derive(Encode)]
        #[cbor(map)]
        struct OldServer {
            #[n(0)]
            min: u16,
            #[n(1)]
            max: u16,
            #[n(2)]
            version: Option<u16>,
        }
        let wire = minicbor::to_vec(OldServer {
            min: 9,
            max: 9,
            version: Some(9),
        })
        .unwrap();
        let s: ServerHello = minicbor::decode(&wire).unwrap();
        assert_eq!((s.version, s.features), (Some(9), Features::NONE));
    }

    #[test]
    fn negotiation_picks_the_highest_common_version() {
        let v = |min, max| Versions { min, max };
        assert_eq!(v(9, 11).common(v(9, 10)), Some(10));
        assert_eq!(v(9, 9).common(v(9, 12)), Some(9));
        // 一边太旧 / 太新
        assert_eq!(v(8, 8).common(v(9, 10)), None);
        assert_eq!(v(11, 12).common(v(9, 10)), None);
    }

    #[test]
    fn unknown_connect_errors_keep_their_value() {
        #[derive(minicbor::Encode)]
        enum NewServerMsg {
            #[n(2)]
            ConnectResult {
                #[n(0)]
                req_id: u32,
                #[n(1)]
                grant: Option<u8>,
                #[n(2)]
                error: Option<NewError>,
            },
        }
        #[derive(minicbor::Encode)]
        #[cbor(index_only)]
        enum NewError {
            #[n(40)]
            Maintenance,
        }
        let wire = minicbor::to_vec(NewServerMsg::ConnectResult {
            req_id: 3,
            grant: None,
            error: Some(NewError::Maintenance),
        })
        .unwrap();
        let m: ServerMsg = minicbor::decode(&wire).unwrap();
        let ServerMsg::ConnectResult {
            req_id: 3,
            grant,
            error,
        } = m
        else {
            panic!("{m:?}")
        };
        assert_eq!(
            connect_outcome(grant, error).unwrap_err(),
            ConnectError::Unknown(40)
        );
    }
}
