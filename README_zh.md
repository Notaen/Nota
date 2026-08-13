# Nota

基于进程内**事件总线**的 persona 驱动 AI Agent 框架。每个 persona 是独立
runtime，拥有自己的聊天记录、系统提示词和 LLM 会话。存储是按 persona
分文件的——没有数据库，没有全局 session 注册表。基于 `axum` 的适配器暴露
一套小巧的 REST API，外加一个 WebSocket 通道用于流式聊天和权限请求。

## 构建与运行

```sh
cargo build
cargo run -p nota-cli -- onboard   # 配置 API + 创建首个 persona
cargo run -p nota-cli              # 启动服务（REST + WS，端口 :2349）
```

## OneBot 11（QQ 机器人）

OneBot 11 支持是独立 Rust crate `nota-onebot`（不再用 JS/插件运行时）。
目前支持**正向 WebSocket**：`nota` 主动连接 OneBot 实现（NapCat /
LLOneBot / Lagrange，默认 `ws://127.0.0.1:3001`），把收到的私聊/群聊消息
交给 persona，再把 persona 的回复发回原会话。

在 `~/.nota/config.toml` 里配置（或在 `nota onboard` 向导里按提示填写）：

```toml
[onebot]
enabled = true
mode = "ws"                        # 目前仅支持正向 WebSocket
ws_url = "ws://127.0.0.1:3001"     # 你的 OneBot 实现的 WS 服务地址
access_token = ""                  # 可选
persona = "default"                # 处理消息的 persona；留空则用第一个
prefix = ""                        # 可选：只响应以此开头的内容，并去掉前缀
friend_ids = [123456789]           # 好友白名单：只回复这些好友的私聊
group_ids = []                     # 群白名单：只回复这些群
```

注意：

- 非文本消息段（图片、表情、@ 等）会先转成占位符再交给 LLM；回复以纯文本
  发送，超过 4000 字符会自动分段。
- 入站消息会带上会话身份头和 QQ 号（`[私聊 昵称(QQ) → bot(QQ)]` /
  `[群 群号 昵称(QQ) → bot(QQ)]`），persona 始终知道是谁在说话（发送者
  QQ、群号、bot 自己的 QQ）。
- 引用（回复）消息段会带上消息 id：`[回复消息ID:…]`；`get_msg` 工具可按
  该 id 取回被引用的那条消息，群历史每行也带 `消息ID:…`，persona 能把
  引用和历史对应起来。
- **会话与收发**：每个聊天端点（QQ 好友、群、Web 客户端）都是独立的
  对话 **session**，历史分两层：`deep.json`（完整 LLM 上下文）与
  `shallow.json`（真正发送给用户的消息）。persona 的最终回答自动路由回
  原会话；`skip_reply` / 空输出可抑制回复（"不要回答"会真的不回）。
  `send_message(target: "private:<QQ>" | "group:<QQ>", content)` 让
  persona 可以主动向任意白名单会话发消息（比如在私聊里让它去群里说），
  每条实际发出的消息都会记入目标会话的浅层。
- **白名单机制**：persona 只对 `friend_ids` / `group_ids` 里指定的人/群回复；
  其他人发来的消息会在调用 LLM 之前直接被丢弃。列表为空 = 该类别谁也不回复。
- `read_group_chat` 工具：persona 可以主动拉取**任意群**的最近消息（通过
  NapCat 的 `get_group_msg_history` 扩展接口，走同一条 WS），例如你问它
  “群 123456 最近聊了什么”，它读完后回答你，但不会在群里发言。每行都会
  带上发言人的 QQ 号。
- `get_login_info` 工具：persona 可以通过标准 OneBot API 查询 bot 自己的
  QQ 号和昵称。
- OneBot 工具统一由 `OneBotBridge::register_tools` 注册，CLI 不直接接触
  具体的 OneBot 工具类型。
- OneBot 目前没有在线授权通道，工具需要授权时会自动拒绝并在聊天里提示。
- `enabled = true` 时至少需要一个 persona（或配置有效的 `persona` 名字），
  否则服务拒绝启动。

## 架构

Cargo 工作区有四个 crate；依赖方向严格单向
`nota-cli → nota-infra → nota-core`。

| Crate | 职责 | 关键依赖 |
|-------|------|---------|
| `nota-core` | 领域实体、端口 trait（`PersonaStore`、`LlmClient`、`Tool`、`ToolRegistry`、`AgentRunner`）、`EventBus`、`PermissionRegistry`。纯净：无 I/O，无 JSON 序列化。 | `log`、`serde`、`async-trait`、`chrono`、`anyhow`、`tokio`（sync） |
| `nota-infra` | 适配器：`axum` HTTP（REST + WebSocket）、文件系统 persona store、`OpenAiLlm`、TOML 配置、内置工具。实现 `nota-core` 的端口。 | `nota-core`、`nota-onebot`、`axum`（含 `ws` feature）、`reqwest`、`serde_json` |
| `nota-onebot` | OneBot 11 传输适配器（正向 WebSocket）：协议类型、WS 客户端、总线桥接、`read_group_chat` 工具。不属于 core/infra。 | `nota-core`、`tokio-tungstenite`、`serde_json`、`uuid` |
| `nota-cli` | 二进制（`nota`）。子命令 `onboard`（向导）/ 默认（运行服务）。装配并启动一切。 | `nota-core`、`nota-infra`、`nota-onebot`、`tracing`、`dialoguer` |

### 运行时模型

```
                         EventBus (mpsc broadcast)
                              │
        ┌──────────────┬──────┴──────────────┐
        ▼              ▼                     ▼
  Persona "alice"  Persona "bob"       HTTP /ws/chat
```

- 总线传递 `BusEvent { kind, sender, content, request_id, parent_request_id, target, … }`。
- `BusEvent.target`（可选）把消息路由到指定 persona；缺省时所有订阅者都收到事件。
- 每个 persona 有自己的 `PersonaRuntime` 事件循环：接收事件 → 用 `solo.md` + chatlog 拼装 prompt → 调用 LLM → 处理工具调用 → 把 assistant 回复投回总线。
- HTTP/WS 层也是总线订阅者。每个 WebSocket 连接维护自己的 `active_request_ids`，
  只转发匹配的事件——多个浏览器标签页之间不会互相泄露消息。

### 权限流程

当工具要做需要用户批准的事（比如 `file_read` 访问 persona 工作区之外的路径），
调用 `ToolContext::request_permission(prompt)`：

1. 在 `PermissionRegistry` 里以新 UUID 注册一个 oneshot。
2. 向总线发一个 `PermissionRequest` 事件，`parent_request_id` 设为原始用户请求 id。
3. 等待 oneshot。

WS handler 把事件转发给对应的浏览器标签：
`{type:"permission_needed", permission_id, prompt, request_id}`。用户点
Allow/Deny，浏览器发回 `{type:"permission", permission_id, approved}`。
WS handler 直接调 `PermissionRegistry::resolve(id, approved)`（不再走总线）。
工具恢复执行，persona 完成，最终回复以 `{type:"message", content, request_id}`
流回。

## 技术栈

Rust 2024 · Axum 0.8（REST + WebSocket）· Tokio · reqwest · serde ·
serde_json · TOML · `log`（core/infra）/ `tracing`（cli）· dialoguer（向导）

## 接口

REST：

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/health` | 健康检查 |
| GET | `/api/personas` | 列出 persona |
| POST | `/api/personas` | 创建 persona（`{"name": "..."}`） |
| GET | `/api/personas/:name` | persona 信息 |
| DELETE | `/api/personas/:name` | 删除 persona |
| GET | `/api/personas/:name/files/:filename` | 读 persona 文件 |
| PUT | `/api/personas/:name/files/:filename` | 写 persona 文件 |
| GET | `/api/personas/:name/chatlog` | 读 chatlog |
| GET | `/api/settings` | 取配置 |
| PUT | `/api/settings` | 更新配置 |
| POST | `/admin/stop` | 优雅停机 |

WebSocket（`/ws/chat`）：

```
# 客户端 → 服务端
{ "type": "send",       "persona": "alice", "content": "你好", "request_id": "<uuid>" }
{ "type": "permission", "permission_id": "<uuid>", "approved": true }

# 服务端 → 客户端
{ "type": "message",           "content": "你好", "request_id": "<uuid>" }
{ "type": "permission_needed", "permission_id": "<uuid>", "prompt": "允许 file_read /etc/passwd？", "request_id": "<uuid>" }
{ "type": "error",             "content": "..." }
```

## 目录结构

```
nota/
└── crates/
    ├── nota-core/    # 领域 + 端口 + EventBus + PermissionRegistry
    ├── nota-infra/   # 适配器（axum HTTP/WS、persona_store、llm、config、tools）
    ├── nota-onebot/  # OneBot 11 正向 WS 传输 + read_group_chat 工具
    └── nota-cli/     # 二进制：`nota`（服务）/ `nota onboard`
```

运行时数据位于用户主目录：

```
~/.nota/
├── personas/
│   └── <name>/
│       ├── solo.md        # 系统提示词
│       ├── memory.md      # 长期记忆
├── .logs/                 # 日志（30 天轮转）
├── sessions/
│   └── <session_id>/      # 按会话隔离的历史（独立于 personas）
│       ├── deep.json      # LLM 上下文——由 persona 模块管理
│       └── shallow.json   # 真正发送给用户的消息——由 session 模块管理
└── config.toml            # api_url、api_key、model
```

`base_dir()` 在 `nota-cli` 里解析（`dirs::home_dir().join(".nota")`），注入到
适配器；core 不接触路径。

## 文档

- [`.agent/guide.md`](.agent/guide.md) — 架构、提交规范、踩坑记录
- [`.agent/notes.md`](.agent/notes.md) — 设计决策与重构历史
- [`AGENTS.md`](AGENTS.md) — AI 编程助手必读
