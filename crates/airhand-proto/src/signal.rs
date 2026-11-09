//! 控制流（客户端 ⇄ 服务器）信令消息。
//!
//! 服务器发给客户端的都是类型（例如 [`ConnectError`]、[`ServerMsg::Incompatible`]），不是文字：
//! 客户端按自己的界面语言显示。
//!
//! # 版本握手
//!
//! 连接后客户端先发 [`ClientMsg::Hello`]，服务器回 [`ServerMsg::Welcome`] 或 [`ServerMsg::Incompatible`]。
//! 为了让**任何版本**的客户端和服务器都能互相告知"版本不一致"，这三条消息的编码永远不能改变：
//! 它们必须是各自枚举的前几个变体（序号 0 / 0 / 1），字段也不能增删。
//! 其它消息可以随 [`crate::PROTOCOL_VERSION`] 自由修改。`framing` 模块的测试锁定了它们的编码。
//!
//! # 重连凭证（resume tokens）
//!
//! 控制端连上被控端时，服务器在 [`SessionGrant::resume`] 里给一个重连凭证（服务器由被控端的控制码和密钥算出，
//! 没有密钥算不出来，也不能反推出密钥）。被控端掉线后，控制端带着凭证重连（[`ClientMsg::Connect::resume`]）：
//!
//! * 带凭证的请求不算猜码，也不受限速影响；
//! * 凭证有效且被控端在线时照常建立会话；其它情况（被控端不在线、凭证不对、控制码不存在）**一律**回答
//!   [`ConnectError::Offline`]，不透露控制码是否存在，所以凭证不能用来猜码；
//! * 回答 `Offline` 之后，服务器记下这个请求：被控端用这个控制码重新上线、并且凭证对得上时，
//!   发 [`ServerMsg::HostOnline`]，控制端收到后马上重连。等待期间控制端不需要反复尝试。
//!
//! 凭证由控制码和密钥算出：被控端取回原来的控制码（包括服务器重启之后）时凭证仍然有效；换了控制码、
//! 或者被控端的用户断开了所有控制端（被控端随即发 [`ClientMsg::Rekey`] 换一个密钥）时，之前发出的凭证全部作废。
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

use serde::{Deserialize, Serialize};

use crate::Role;

/// 客户端 → 服务器
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ClientMsg {
    /// 连接后的第一条消息（编码永远不变，见模块说明）。
    ///
    /// `name` 为空表示不知道本机名称，由显示的一方按自己的语言显示"未知设备"。
    Hello {
        version: u16,
        role: Role,
        name: String,
        os: String,
    },
    /// 被控端申请控制码。断线重连时携带上次的控制码与密钥，以便取回同一个控制码。
    ///
    /// `keep`：被控端会保存控制码（重启后也带着它来取回），服务器应该一直为它保留（见模块说明的"固定控制码"）。
    Register {
        reclaim: Option<Reclaim>,
        keep: bool,
    },
    /// 控制端请求连接某个控制码对应的被控端。
    ///
    /// `resume`：之前连上这个控制码时得到的重连凭证（见模块说明的"重连凭证"），第一次连接时为 `None`。
    Connect {
        req_id: u32,
        code: u32,
        resume: Option<ResumeToken>,
    },
    /// 被控端放弃当前的控制码（托盘"更换控制码"）：解除绑定，之后断开连接、不带 `reclaim` 重新注册。
    Release,
    /// 被控端的用户断开了所有控制端：换一个密钥，之前发出的重连凭证全部作废（被断开的控制端不能带着凭证马上连回来）。
    /// 服务器回 [`ServerMsg::Rekeyed`]，被控端保存新的密钥，下次取回控制码时用它
    Rekey,
}

/// 重连凭证（见模块说明）。对客户端来说是不透明的，原样带回即可
pub type ResumeToken = [u8; 16];

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reclaim {
    pub code: u32,
    pub secret: [u8; 16],
}

/// 服务器 → 客户端
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ServerMsg {
    /// 握手成功（编码永远不变，见模块说明）。
    Welcome { version: u16 },
    /// 协议版本不一致，客户端应停止重试，并根据双方版本号提示用户更新客户端还是服务器
    ///（编码永远不变，见模块说明）。
    Incompatible { server_version: u16 },
    /// 被控端注册成功。
    Registered { code: u32, secret: [u8; 16] },
    /// 对 [`ClientMsg::Connect`] 的应答。
    ConnectResult {
        req_id: u32,
        result: Result<SessionGrant, ConnectError>,
    },
    /// 通知被控端：有控制端发起会话，请打开会话流。
    Incoming { session_id: u64, peer: Peer },
    /// 通知控制端：它在等的被控端（收到过 [`ConnectError::Offline`]）重新上线了，可以重连。
    /// 每次 `Offline` 回答最多换来一次通知；控制端与服务器的连接断了要重新请求。
    HostOnline { code: u32 },
    /// 对 [`ClientMsg::Rekey`] 的回答：控制码不变，取回它的密钥换成了这个（已经保存到服务器的数据库里）
    Rekeyed { secret: [u8; 16] },
}

/// 发起会话的控制端
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// 计算机名，为空表示对方不知道自己的名称
    pub name: String,
    pub ip: IpAddr,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionGrant {
    pub session_id: u64,
    pub host_name: String,
    pub host_os: String,
    /// 重连凭证，被控端掉线后重连时带上
    pub resume: ResumeToken,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectError {
    /// 控制码不存在或对方不在线
    NotFound,
    /// 尝试过于频繁
    RateLimited,
    /// 对方会话数已满
    Busy,
    /// 对方暂时不在线，上线时会收到 [`ServerMsg::HostOnline`]（只回答带凭证的请求，见模块说明）
    Offline,
}
