//! Airhand 客户端一侧的链路，桌面客户端（`client/src-tauri`）、网页控制台（`web/`）和 Android（`android/native`）共用：
//!
//! * [`net`]：QUIC 连接、证书固定、版本握手、断线重连；
//! * [`wire`]：从流里读写长度前缀帧（帧的格式和消息的编解码在 `airhand_proto::framing`，那边不碰 IO）；
//! * [`transport`]：会话的四条流（控制、画面、声音、大块）；
//! * [`secure`]：端到端加密：设备的身份密钥、控制通道上的握手、每一帧的加解密；
//! * [`controller`]：控制端服务与会话。与界面无关：界面的命令是 [`controller::Command`]，
//!   画面和事件交给 [`controller::SessionUi`]；
//! * [`host`]：被控端与服务器的连接和会话（协议层的事在这里，采集、编码、注入输入由各平台实现 [`host::Platform`]）；
//! * [`flow`]：被控端的在途帧控制和自适应码率；
//! * [`Problem`]：显示给用户的问题，以类型而不是文字的形式传给界面。
//!
//! # feature
//!
//! 除了 [`wire`]，都在默认打开的 `client` 里。中转服务器关掉它（`default-features = false`）、只用 [`wire`]：
//! 读写几帧不用把 Noise、SPAKE2 这些它用不到的库也编译进去。

#[cfg(feature = "client")]
pub mod controller;
#[cfg(feature = "client")]
pub mod flow;
#[cfg(feature = "client")]
pub mod host;
#[cfg(feature = "client")]
pub mod net;
#[cfg(feature = "client")]
mod problem;
#[cfg(feature = "client")]
pub mod secure;
#[cfg(feature = "client")]
pub mod tasks;
#[cfg(feature = "client")]
pub mod transport;
pub mod wire;

#[cfg(feature = "client")]
pub use net::{LinkConfig, ServerState};
#[cfg(feature = "client")]
pub use problem::Problem;
