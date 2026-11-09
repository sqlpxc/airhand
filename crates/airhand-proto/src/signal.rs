//! 信令流（客户端 ⇄ 服务器）的消息。
//!
//! 服务器发给客户端的都是类型（例如 [`ConnectError`]、[`ServerHello::Incompatible`]），不是文字：
//! 客户端按自己的界面语言显示。
//!
//! # 版本握手
//!
//! 连接后客户端先发 [`ClientHello`]，服务器回 [`ServerHello`]。为了让**任何版本**的客户端和服务器都能互相告知
//! "版本不一致"，这两条消息的开头永远不变：它们是 v9 之前 postcard 编码的 `ClientMsg::Hello`（变体 0：
//! `version | role | name | os`）和 `ServerMsg::Welcome` / `ServerMsg::Incompatible`（变体 0 / 1：一个版本号）。
//!
//! 新的东西跟在这个开头后面，是 CBOR（[`HelloExt`] / [`WelcomeExt`] / [`IncompatibleExt`]，以后还能加字段）：
//! postcard 解码时不管后面多出来的字节，所以旧的服务器 / 客户端照样读得出开头、照样能报"版本不一致"；
//! 新的一方读完开头之后，后面还有字节就按 CBOR 读。
//!
//! 服务器不要求版本相等：双方各自能说一个区间的版本（[`crate::SIGNAL_MIN`]..=[`crate::SIGNAL_VERSION`]），
//! 取两边都能说的最高版本；没有交集才不兼容。[`ServerHello::Incompatible`] 带着服务器的区间，客户端据此判断该更新哪一边。
//! 握手里还交换信令层的能力位（[`crate::features::signal`]）。
//!
//! 握手之后的消息（[`ClientMsg`] / [`ServerMsg`]）是带编号的 CBOR，不认识的跳过（见 [`crate::framing`]）。
//! `framing` 和这里的测试锁定了握手的编码。
//!
//! # 重连凭证（resume tokens）
//!
//! 控制端连上被控端时，服务器在 [`SessionGrant::resume`] 里给一个重连凭证。凭证有两半：
//!
//! * 前一半是服务器用自己的密钥给**控制码**签的名：证明拿着它的控制端连上过这个控制码（服务器重启之后也认得，
//!   被控端在不在线都能验证）。拿着它的人本来就知道这个控制码，所以带凭证的请求不算猜码，也不受限速影响；
//! * 后一半由控制码和被控端的密钥算出（没有密钥算不出来，也不能反推出密钥）：只对被控端现在的密钥有效。
//!
//! 被控端掉线后，控制端带着凭证重连（[`ClientMsg::Connect::resume`]），服务器的回答：
//!
//! * 前一半不是本服务器为这个控制码签的（伪造的、旧版本的、别的服务器发的）：[`ConnectError::Revoked`]。
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
//! 不保存控制码的被控端（`keep = false`）和以前一样：掉线后控制码只保留一小段时间。

use std::net::IpAddr;

use minicbor::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::framing::FrameError;
use crate::{Features, Role};

// ───────────────────────────── 版本握手 ─────────────────────────────

/// 客户端连上之后的第一条消息（见模块说明的"版本握手"）。
///
/// `name` 为空表示不知道本机名称，由显示的一方按自己的语言显示"未知设备"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHello {
    /// 本端能说的最高信令版本（旧服务器拿它和自己的版本比）
    pub version: u16,
    pub role: Role,
    pub name: String,
    pub os: String,
    /// 本端能说的最老的版本（v9 之前的客户端没有，等于 `version`）
    pub min_version: u16,
    /// 信令层的能力位（[`crate::features::signal`]）
    pub features: Features,
}

/// [`ClientHello`] 跟在冻结的开头后面的部分
#[derive(Encode, Decode, Debug, Default)]
#[cbor(map)]
pub struct HelloExt {
    #[n(0)]
    pub min_version: Option<u16>,
    #[n(1)]
    pub features: Option<Features>,
}

/// v9 之前的 `ClientMsg`：只用它的变体 0（编码永远不变）
#[derive(Serialize, Deserialize)]
enum FrozenClient {
    Hello {
        version: u16,
        role: Role,
        name: String,
        os: String,
    },
}

/// v9 之前的 `ServerMsg`：只用它的变体 0 和 1（编码永远不变）
#[derive(Serialize, Deserialize)]
enum FrozenServer {
    Welcome { version: u16 },
    Incompatible { server_version: u16 },
}

impl ClientHello {
    /// 本端的握手
    pub fn new(role: Role, name: String, os: String) -> Self {
        Self {
            version: crate::SIGNAL_VERSION,
            role,
            name,
            os,
            min_version: crate::SIGNAL_MIN,
            features: crate::features::signal::SUPPORTED,
        }
    }

    /// 负载（不带长度前缀）：冻结的开头 + CBOR 的 [`HelloExt`]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = postcard::to_stdvec(&FrozenClient::Hello {
            version: self.version,
            role: self.role,
            name: self.name.clone(),
            os: self.os.clone(),
        })
        .expect("Hello always encodes");
        minicbor::encode(
            HelloExt {
                min_version: Some(self.min_version),
                features: Some(self.features),
            },
            &mut out,
        )
        .expect("writing to a Vec can't fail");
        out
    }

    pub fn decode(payload: &[u8]) -> Result<Self, FrameError> {
        let (
            FrozenClient::Hello {
                version,
                role,
                name,
                os,
            },
            rest,
        ) = postcard::take_from_bytes(payload)?;
        let ext = if rest.is_empty() {
            HelloExt::default()
        } else {
            minicbor::decode(rest)?
        };
        Ok(Self {
            version,
            role,
            name,
            os,
            min_version: ext.min_version.unwrap_or(version).min(version),
            features: ext.features.unwrap_or_default(),
        })
    }

    /// 服务器怎么回答：双方都能说的最高版本，没有交集就是不兼容
    pub fn negotiate(&self, min: u16, max: u16, features: Features) -> ServerHello {
        let version = self.version.min(max);
        if version < self.min_version.max(min) {
            ServerHello::Incompatible { min, max }
        } else {
            ServerHello::Welcome { version, features }
        }
    }
}

/// 服务器对 [`ClientHello`] 的回答（见模块说明的"版本握手"）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerHello {
    /// 握手成功。之后按 `version` 说话；`features` 是服务器的信令能力位
    Welcome { version: u16, features: Features },
    /// 没有双方都能说的版本：服务器能说 `min..=max`。客户端应停止重试，并据此提示用户更新客户端还是服务器
    Incompatible { min: u16, max: u16 },
}

#[derive(Encode, Decode, Debug, Default)]
#[cbor(map)]
pub struct WelcomeExt {
    #[n(0)]
    pub features: Option<Features>,
}

#[derive(Encode, Decode, Debug, Default)]
#[cbor(map)]
pub struct IncompatibleExt {
    #[n(0)]
    pub min: Option<u16>,
}

impl ServerHello {
    /// 负载（不带长度前缀）：冻结的开头 + CBOR 的扩展
    pub fn encode(&self) -> Vec<u8> {
        let (frozen, ext) = match *self {
            ServerHello::Welcome { version, features } => (
                FrozenServer::Welcome { version },
                minicbor::to_vec(WelcomeExt {
                    features: Some(features),
                }),
            ),
            // 旧客户端只看这个号：比它自己的大就提示更新客户端
            ServerHello::Incompatible { min, max } => (
                FrozenServer::Incompatible {
                    server_version: max,
                },
                minicbor::to_vec(IncompatibleExt { min: Some(min) }),
            ),
        };
        let mut out = postcard::to_stdvec(&frozen).expect("ServerHello always encodes");
        out.extend(ext.expect("writing to a Vec can't fail"));
        out
    }

    pub fn decode(payload: &[u8]) -> Result<Self, FrameError> {
        let (frozen, rest): (FrozenServer, _) = postcard::take_from_bytes(payload)?;
        Ok(match frozen {
            FrozenServer::Welcome { version } => {
                let ext: WelcomeExt = if rest.is_empty() {
                    WelcomeExt::default()
                } else {
                    minicbor::decode(rest)?
                };
                ServerHello::Welcome {
                    version,
                    features: ext.features.unwrap_or_default(),
                }
            }
            FrozenServer::Incompatible { server_version } => {
                let ext: IncompatibleExt = if rest.is_empty() {
                    IncompatibleExt::default()
                } else {
                    minicbor::decode(rest)?
                };
                ServerHello::Incompatible {
                    min: ext.min.unwrap_or(server_version).min(server_version),
                    max: server_version,
                }
            }
        })
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
    #[n(2)]
    Connect {
        #[n(0)]
        req_id: u32,
        #[n(1)]
        code: u32,
        #[cbor(n(2), with = "minicbor::bytes")]
        resume: Option<ResumeToken>,
    },
    /// 被控端放弃当前的控制码（托盘"更换控制码"）：解除绑定，之后断开连接、不带 `reclaim` 重新注册。
    #[n(3)]
    Release,
    /// 被控端的用户断开了所有控制端：换一个密钥，之前发出的重连凭证全部作废（被断开的控制端不能带着凭证马上连回来）。
    /// 服务器回 [`ServerMsg::Rekeyed`]，被控端保存新的密钥，下次取回控制码时用它
    #[n(4)]
    Rekey,
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
    /// 对 [`ClientMsg::Connect`] 的应答：成功时有 `grant`，失败时有 `error`
    /// （两个都没有：服务器的版本更新、拒绝的原因这边不认识）。用 [`ServerMsg::connect_result`] 构造、[`connect_outcome`] 读
    #[n(2)]
    ConnectResult {
        #[n(0)]
        req_id: u32,
        #[n(1)]
        grant: Option<SessionGrant>,
        #[cbor(n(2), with = "crate::cbor::opt", has_nil)]
        error: Option<ConnectError>,
    },
    /// 通知被控端：有控制端发起会话，请打开会话流。
    #[n(3)]
    Incoming {
        #[n(0)]
        session_id: u64,
        #[n(1)]
        peer: Peer,
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

/// [`ServerMsg::ConnectResult`] 的结果；`Err(None)` 表示服务器拒绝了、原因这个版本不认识
pub fn connect_outcome(
    grant: Option<SessionGrant>,
    error: Option<ConnectError>,
) -> Result<SessionGrant, Option<ConnectError>> {
    match (grant, error) {
        (Some(g), _) => Ok(g),
        (None, e) => Err(e),
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
}

#[derive(Encode, Decode, Debug, Clone, Copy, PartialEq, Eq)]
#[cbor(index_only)]
pub enum ConnectError {
    /// 控制码不存在或对方不在线
    #[n(0)]
    NotFound,
    /// 尝试过于频繁
    #[n(1)]
    RateLimited,
    /// 对方会话数已满
    #[n(2)]
    Busy,
    /// 对方暂时不在线，上线时会收到 [`ServerMsg::HostOnline`]（只回答带凭证的请求，见模块说明）
    #[n(3)]
    Offline,
    /// 重连凭证作废了（被控端的用户断开过所有控制端、控制码换了主人），或者不是这台服务器为这个控制码发的
    ///（只回答带凭证的请求，见模块说明）
    #[n(4)]
    Revoked,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Role;

    /// 版本握手的开头永远不能变（见模块说明）。这个测试锁定它们的编码：
    /// 如果它失败了，说明改动会让其它版本的客户端 / 服务器连"版本不一致"都无法得知。
    #[test]
    fn version_handshake_prefix_is_frozen() {
        let hello = ClientHello {
            version: 7,
            role: Role::Controller,
            name: "A".into(),
            os: "B".into(),
            min_version: 7,
            features: Features::NONE,
        };
        // 变体 0 | version=7 | Role::Controller=1 | "A" | "B"，后面是扩展
        let wire = hello.encode();
        assert_eq!(wire[..7], [0, 7, 1, 1, b'A', 1, b'B']);
        let host = ClientHello {
            role: Role::Host,
            name: String::new(),
            os: String::new(),
            ..hello.clone()
        };
        assert_eq!(host.encode()[..5], [0, 7, 0, 0, 0]);
        let welcome = ServerHello::Welcome {
            version: 7,
            features: Features::NONE,
        };
        assert_eq!(welcome.encode()[..2], [0, 7]);
        assert_eq!(
            ServerHello::Incompatible { min: 5, max: 7 }.encode()[..2],
            [1, 7]
        );
        assert_eq!(crate::ALPN, b"airhand/1");
    }

    /// 旧版本（只认开头）读得懂新的握手；新版本读得懂旧的（没有扩展）
    #[test]
    fn old_and_new_handshakes_understand_each_other() {
        // v8 的服务器 / 客户端解码时用的就是冻结的枚举，后面多出来的字节不管
        let new = ClientHello {
            version: 10,
            role: Role::Host,
            name: "PC".into(),
            os: "Windows".into(),
            min_version: 9,
            features: Features::from_bits(0b101),
        };
        let wire = new.encode();
        let old: FrozenClient = postcard::from_bytes(&wire).unwrap();
        assert!(matches!(
            old,
            FrozenClient::Hello {
                version: 10,
                role: Role::Host,
                ..
            }
        ));
        assert_eq!(ClientHello::decode(&wire).unwrap(), new);

        // v8 的客户端发来的：没有扩展
        let old = postcard::to_stdvec(&FrozenClient::Hello {
            version: 8,
            role: Role::Controller,
            name: "x".into(),
            os: "y".into(),
        })
        .unwrap();
        let h = ClientHello::decode(&old).unwrap();
        assert_eq!(
            (h.version, h.min_version, h.features),
            (8, 8, Features::NONE)
        );

        for s in [
            ServerHello::Welcome {
                version: 9,
                features: Features::from_bits(1),
            },
            ServerHello::Incompatible { min: 9, max: 11 },
        ] {
            let wire = s.encode();
            assert_eq!(ServerHello::decode(&wire).unwrap(), s);
            // v8 的客户端照样认得出
            let old: FrozenServer = postcard::from_bytes(&wire).unwrap();
            match (old, s) {
                (FrozenServer::Welcome { version }, ServerHello::Welcome { .. }) => {
                    assert_eq!(version, 9)
                }
                (
                    FrozenServer::Incompatible { server_version },
                    ServerHello::Incompatible { .. },
                ) => {
                    assert_eq!(server_version, 11)
                }
                _ => panic!(),
            }
        }
        // v8 的服务器回的：没有扩展
        let old = postcard::to_stdvec(&FrozenServer::Incompatible { server_version: 8 }).unwrap();
        assert_eq!(
            ServerHello::decode(&old).unwrap(),
            ServerHello::Incompatible { min: 8, max: 8 }
        );
    }

    #[test]
    fn negotiation_picks_the_highest_common_version() {
        let hello = |min, max| ClientHello {
            version: max,
            min_version: min,
            ..ClientHello::new(Role::Host, String::new(), String::new())
        };
        let f = Features::from_bits(3);
        assert_eq!(
            hello(9, 11).negotiate(9, 10, f),
            ServerHello::Welcome {
                version: 10,
                features: f
            }
        );
        assert_eq!(
            hello(9, 9).negotiate(9, 12, f),
            ServerHello::Welcome {
                version: 9,
                features: f
            }
        );
        // 客户端太旧 / 太新
        assert_eq!(
            hello(8, 8).negotiate(9, 10, f),
            ServerHello::Incompatible { min: 9, max: 10 }
        );
        assert_eq!(
            hello(11, 12).negotiate(9, 10, f),
            ServerHello::Incompatible { min: 9, max: 10 }
        );
    }

    #[test]
    fn unknown_connect_errors_are_none() {
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
        assert!(matches!(connect_outcome(grant, error), Err(None)));
    }
}
