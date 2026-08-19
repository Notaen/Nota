# OneBot 11 adapter

Agent-facing deep dive. The canonical system description lives in
`CONTRIBUTING.md` (Runtime model, HTTP API); this file holds the adapter
details agents need before touching `nota-onebot`.

## Config (`[onebot]` in `config.toml`)

`enabled`, `mode` (forward WebSocket only), `ws_url` (default
`ws://127.0.0.1:3001`), `access_token` (Bearer auth), `persona` (empty =
first persona found), `prefix` (only answer messages starting with this),
`friend_ids`, `group_ids`. `enabled = true` requires ≥ 1 persona or a valid
`persona` name, or the server refuses to start.

## Routing

- Inbound: message → targeted persona inbox carrying the `Conversation`
  (`onebot_private_<id>` / `onebot_group_<id>`).
- Outbound: `reply` / `onebot_send_msg` → `route_outbound` → the adapter
  bridge enforces the allowlist and chunks sends at 4000 chars. The turn's
  final assistant text is auto-routed by `PersonaRuntime` through the same
  path.
- Allowlist: only `friend_ids` / `group_ids` reach the persona or get
  replies; an empty list means nobody in that category. Outbound to a
  non-allowlisted target triggers a permission oneshot answered in the
  originating conversation with plain 「同意」 / 「拒绝」 (or 「同意N」 / 「拒绝N」
  for queued requests). OneBot has no interactive approval panel, so tool
  permission requests are auto-denied with a notice to the chat.

## Message rendering

- Non-text segments render as `[{segment_type} msg id:<id>]` (e.g.
  `[image msg id:123]`; `reply` quotes become `[reply msg id:…]`); text
  keeps its content.
- Inbound headers: `[好友 昵称(id)]` (private) / `[群 群号 昵称(id)]` (group).
- Voice → `[record msg id:<id>]`, transcribed on demand via
  `onebot_voice_text` (NapCat `fetch_ptt_text`, retries 3× 2s apart, id sent
  as string).

## Tools (`onebot_*`, registered via `OneBotBridge::register_tools`)

- `onebot_send_msg` — send to a specific allowlisted chat
  (`target = private:<id>` / `group:<id>`).
- `onebot_get_msg_history` — `target`-based history (private + group) via
  NapCat `get_group_msg_history` / `get_friend_msg_history`; reading any
  chat is not allowlist-gated.
- `onebot_get_content` — fetch message content by id.
- `onebot_status` — connection state + login info.
- `onebot_voice_text` — voice transcription.

Registration fails on a name collision, like every other registration site.

## NapCat quirks (verified live 2026-08-13)

- Resets the WS connection when the **client** sends an unsolicited Ping —
  the client only pongs the server's pings.
- Must listen on `0.0.0.0` for LAN clients.
- Does not queue events for disconnected clients (messages sent during a
  disconnect window are lost).
- `fetch_ptt_text` is intermittent (retcode=1200 then success on retry).
