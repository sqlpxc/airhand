//! 显示给用户的问题（错误），以**类型**而不是文字的形式传给界面。
//!
//! * 托盘（Rust，见 `client/src-tauri/src/tray.rs`）和控制台（前端，见 `client/src/lib/messages.ts`）各自按本机**当前的**界面语言
//!   把它变成文字：界面上不会出现另一台电脑或服务器的语言，切换语言时已经显示出来的问题也会跟着切换；
//! * `Display` 是写日志用的英文；
//! * `detail` 是给排查问题用的技术信息（英文），界面上原样显示在说明文字后面。
//!
//! 新增变体时，前端的 `problem.<kind>` 词条也要补上（`client/` 里的 `npm test` 会检查），
//! Android 的文案也要补上（`android/app/.../ui/Messages.kt` 和两种语言的 `strings.xml`，`android/` 里的
//! `./gradlew testDebugUnitTest` 会检查）。

use std::fmt;

use airhand_proto::session::ByeReason;
use airhand_proto::signal::ConnectError;
use serde::{Deserialize, Serialize};

/// 序列化为 `{ "kind": "serverLost", "detail": "..." }` 这样的 JSON。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Problem {
    // ── 与中转服务器的连接 ──
    /// 连不上服务器（解析地址失败、网络不通、超时……）
    ServerUnreachable { detail: String },
    /// 服务器证书与编译进程序的指纹不一致（可能是中间人攻击，或者服务器换了证书）
    CertMismatch,
    /// 与服务器的协议版本不一致（由双方版本号判断该更新哪一方）
    Incompatible { server: u16, client: u16 },
    /// 与服务器的连接断开了，或者连上之后出错
    ServerLost { detail: String },

    // ── 连接被控端 ──
    /// 还没有连上服务器
    NotConnected,
    /// 控制码不是 6 位数字
    InvalidCode,
    /// 控制码不存在或对方不在线
    CodeNotFound,
    /// 尝试过于频繁
    RateLimited,
    /// 对方的会话数已满
    HostBusy,
    /// 服务器没有及时回应连接请求
    ServerTimeout,
    /// 对方暂时不在线（重连时才会遇到：服务器确认过是之前连上的那台），对方重新上线后自动重连
    HostOffline,

    // ── 端到端握手（见 airhand_proto::handshake）──
    /// 对方的身份（端到端加密的公钥）和之前记下的不一样：对方重装过、换了身份，或者有人在中间冒充它。
    /// 不自动重连；用户确认之后忘掉记下的身份再连
    HostKeyChanged,
    /// 对方要求配对码：这台控制端还没和它配对过（配对码显示在对方的托盘菜单里）
    PairingRequired,
    /// 配对码不对
    PairingFailed,
    /// 对方现在不接受配对（有人多次输错了配对码，要对方的用户换一个配对码）
    PairingUnavailable,
    /// 和对方的会话版本不兼容。`peer` 比 `ours` 大：要更新本机的 Airhand；否则要对方更新
    PeerIncompatible { peer: u16, ours: u16 },

    // ── 会话 ──
    /// 被控端结束了会话（会话流关闭）
    HostEnded,
    /// 被控端长时间没有任何消息
    HostUnresponsive,
    /// 被控端的用户断开了所有控制端。重连凭证作废了（[`ConnectError::Revoked`]）也是这个：
    /// 通常就是因为对方断开过所有控制端（也可能是对方换了控制码、这个控制码给了别人）
    Kicked,
    /// 被控端的用户更换了控制码
    CodeChanged,
    /// 被控端的用户拒绝了这次连接
    Declined,
    /// 被控端的用户一直没有回应连接请求
    Unanswered,
    /// 会话已结束
    SessionEnded,

    // ── 其它 ──
    /// 当前不是控制端模式
    NotController,
    /// 会话不存在（已经结束）
    NoSession,
    /// 本机剪贴板没有文本
    NoClipboardText,
    /// 其它错误
    Failed { detail: String },
}

impl Problem {
    /// 取出错误里的 `Problem`（经过 `?` 或 `.context()` 也能取到）；
    /// 没有的话用 `fallback` 把整条错误链（英文）包装起来。
    pub fn from_error(e: anyhow::Error, fallback: impl FnOnce(String) -> Problem) -> Problem {
        match e.downcast::<Problem>() {
            Ok(p) => p,
            Err(e) => fallback(format!("{e:#}")),
        }
    }

    pub fn failed(e: impl fmt::Display) -> Problem {
        Problem::Failed {
            detail: e.to_string(),
        }
    }

    /// 重试也不会好的问题（版本不一致），应该放慢重连
    pub fn is_fatal(&self) -> bool {
        matches!(self, Problem::Incompatible { .. })
    }

    /// 对方有意结束（或者对方的用户没有同意）的会话，或者要用户来决定的问题（身份变了、要配对码……），
    /// 不应该自动重连
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            Problem::Kicked
                | Problem::CodeChanged
                | Problem::Declined
                | Problem::Unanswered
                | Problem::HostKeyChanged
                | Problem::PairingRequired
                | Problem::PairingFailed
                | Problem::PairingUnavailable
                | Problem::PeerIncompatible { .. }
        )
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::ServerUnreachable { detail } => {
                write!(f, "can't connect to the server: {detail}")
            }
            Problem::CertMismatch => {
                f.write_str("the server certificate doesn't match the pinned fingerprint")
            }
            Problem::Incompatible { server, client } => {
                write!(
                    f,
                    "protocol version mismatch (server {server}, client {client})"
                )
            }
            Problem::ServerLost { detail } => write!(f, "lost connection to the server: {detail}"),
            Problem::NotConnected => f.write_str("not connected to the server yet"),
            Problem::InvalidCode => f.write_str("the control code must be 6 digits"),
            Problem::CodeNotFound => f.write_str("control code not found, or the host is offline"),
            Problem::RateLimited => f.write_str("too many attempts"),
            Problem::HostBusy => f.write_str("the host has reached its session limit"),
            Problem::ServerTimeout => f.write_str("the server didn't answer in time"),
            Problem::HostOffline => f.write_str("the host is offline, waiting for it to come back"),
            Problem::HostKeyChanged => f.write_str(
                "the host's identity changed (it was reinstalled, or someone is impersonating it)",
            ),
            Problem::PairingRequired => f.write_str("the host requires a pairing code"),
            Problem::PairingFailed => f.write_str("wrong pairing code"),
            Problem::PairingUnavailable => {
                f.write_str("the host doesn't accept pairing right now (too many wrong codes)")
            }
            Problem::PeerIncompatible { peer, ours } => write!(
                f,
                "incompatible session protocol (host {peer}, this computer {ours})"
            ),
            Problem::HostEnded => f.write_str("the host ended the session"),
            Problem::HostUnresponsive => f.write_str("the host stopped responding"),
            Problem::Kicked => f.write_str("the user at the host disconnected all controllers"),
            Problem::CodeChanged => f.write_str("the user at the host changed the control code"),
            Problem::Declined => f.write_str("the user at the host declined the connection"),
            Problem::Unanswered => {
                f.write_str("the user at the host didn't answer the connection request")
            }
            Problem::SessionEnded => f.write_str("session ended"),
            Problem::NotController => f.write_str("not in controller mode"),
            Problem::NoSession => f.write_str("no such session"),
            Problem::NoClipboardText => f.write_str("no text on the local clipboard"),
            Problem::Failed { detail } => f.write_str(detail),
        }
    }
}

impl std::error::Error for Problem {}

impl From<ConnectError> for Problem {
    fn from(e: ConnectError) -> Self {
        match e {
            ConnectError::NotFound => Problem::CodeNotFound,
            ConnectError::RateLimited => Problem::RateLimited,
            ConnectError::Busy => Problem::HostBusy,
            ConnectError::Offline => Problem::HostOffline,
            ConnectError::Revoked => Problem::Kicked,
        }
    }
}

impl From<ByeReason> for Problem {
    fn from(r: ByeReason) -> Self {
        match r {
            ByeReason::Kicked => Problem::Kicked,
            ByeReason::CodeChanged => Problem::CodeChanged,
            ByeReason::Declined => Problem::Declined,
            ByeReason::Unanswered => Problem::Unanswered,
            ByeReason::Other => Problem::HostEnded,
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use serde_json::json;

    use super::*;
    use crate::ServerState;

    /// 前端（client/src/lib/api.ts 的 Problem / ServerState 类型）依赖这个 JSON 形状
    #[test]
    fn json_shape() {
        let v = |p: &Problem| serde_json::to_value(p).unwrap();
        assert_eq!(v(&Problem::CertMismatch), json!({ "kind": "certMismatch" }));
        assert_eq!(
            v(&Problem::Incompatible {
                server: 3,
                client: 2
            }),
            json!({ "kind": "incompatible", "server": 3, "client": 2 })
        );
        assert_eq!(
            v(&Problem::NoClipboardText),
            json!({ "kind": "noClipboardText" })
        );
        assert_eq!(
            v(&Problem::HostKeyChanged),
            json!({ "kind": "hostKeyChanged" })
        );
        assert_eq!(
            v(&Problem::PeerIncompatible { peer: 2, ours: 1 }),
            json!({ "kind": "peerIncompatible", "peer": 2, "ours": 1 })
        );
        let st = ServerState::Disconnected {
            reason: Problem::ServerLost {
                detail: "timed out".into(),
            },
        };
        assert_eq!(
            serde_json::to_value(&st).unwrap(),
            json!({ "state": "disconnected", "reason": { "kind": "serverLost", "detail": "timed out" } })
        );
    }

    #[test]
    fn survives_question_mark_and_context() {
        fn inner() -> anyhow::Result<()> {
            Err(Problem::HostEnded)?
        }
        assert_eq!(
            Problem::from_error(inner().unwrap_err(), Problem::failed),
            Problem::HostEnded
        );
        let wrapped = Err::<(), _>(Problem::CertMismatch)
            .context("handshake")
            .unwrap_err();
        assert_eq!(
            Problem::from_error(wrapped, Problem::failed),
            Problem::CertMismatch
        );
        // 不是 Problem：整条错误链（含 context）作为 detail
        let other = Err::<(), _>(std::io::Error::other("boom"))
            .context("reading")
            .unwrap_err();
        assert_eq!(
            Problem::from_error(other, |detail| Problem::ServerLost { detail }),
            Problem::ServerLost {
                detail: "reading: boom".into()
            }
        );
    }
}
