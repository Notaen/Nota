use serde::{Deserialize, Serialize};

/// OneBot 11 adapter configuration (`[onebot]` in config.toml).
///
/// OneBot is a transport adapter, not part of `nota-core`; it is wired by
/// `nota-cli` when `enabled` is true.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnebotConfig {
    /// Whether to start the OneBot bridge with the server.
    #[serde(default)]
    pub enabled: bool,
    /// Connection mode. Only `"ws"` (forward WebSocket) is implemented for now.
    #[serde(default = "default_onebot_mode")]
    pub mode: String,
    /// Forward WebSocket URL of the OneBot implementation,
    /// e.g. `ws://127.0.0.1:3001` (NapCat / LLOneBot default).
    #[serde(default)]
    pub ws_url: String,
    /// Access token sent as `Authorization: Bearer <token>` (optional).
    #[serde(default)]
    pub access_token: String,
    /// Persona that handles OneBot messages; empty means "first persona found".
    #[serde(default)]
    pub persona: String,
    /// Optional prefix: only messages starting with it are answered,
    /// and the prefix is stripped before handing the text to the persona.
    #[serde(default)]
    pub prefix: String,
    /// Allowlisted friend QQ ids (private chats). The persona only responds
    /// to these friends; everyone else is ignored without calling the LLM.
    /// Empty list = no private chat is allowed.
    #[serde(default)]
    pub friend_ids: Vec<i64>,
    /// Allowlisted group ids. The persona only responds in these groups;
    /// other groups are ignored without calling the LLM.
    /// Empty list = no group is allowed.
    #[serde(default)]
    pub group_ids: Vec<i64>,
}

fn default_onebot_mode() -> String {
    "ws".to_string()
}
