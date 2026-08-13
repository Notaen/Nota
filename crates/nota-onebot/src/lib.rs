//! OneBot 11 protocol adapter (forward WebSocket).
//!
//! OneBot is intentionally **not** part of `nota-core` or `nota-infra`: it is
//! a standalone transport crate that implements the OneBot 11 protocol,
//! bridges events onto the persona event bus, and provides tools (e.g.
//! reading group history) for the persona.

pub mod api;
pub mod bridge;
pub mod client;
pub mod config;
pub mod tools;
pub mod types;

pub use api::OneBotApi;
pub use bridge::{OneBotBridge, Outbound};
pub use config::OnebotConfig;
pub use tools::{
    GetLoginInfoTool, GetMsgTool, ReadGroupChatTool, ReplyTool, SendGroupMsgTool,
    SendPrivateMsgTool,
};
