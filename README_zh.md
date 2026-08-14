# Nota

基于 persona 的 AI Agent 框架。每个 persona 是独立 runtime，拥有自己的系统
提示词（`solo.md`）、记忆（`memory.md`）和 LLM 会话；persona 存储是纯文件。
聊天按**会话（session）**组织（`~/.nota/sessions/<id>/history.db`）。基于
`axum` 的适配器暴露一套小巧的 REST API，外加一个 WebSocket 通道用于流式
聊天和权限请求。

## 构建与运行

```sh
cargo build
cargo run -p nota-cli -- onboard   # 配置 API + 创建首个 persona
cargo run -p nota-cli              # 启动服务（REST + WS，端口 :2349）
```

首次运行：`nota` 会快速失败而不是自动配置——先运行一次 `nota onboard`
配置 API 并创建首个 persona（或用 `nota persona create`）。

## OneBot 11（QQ 机器人）

`nota-onebot` 是独立 Rust crate（无 JS/插件运行时），目前支持**正向
WebSocket**：`nota` 主动连接 OneBot 实现（NapCat / LLOneBot / Lagrange，
默认 `ws://127.0.0.1:3001`），把私聊/群聊消息交给 persona，再把回复路由回去。

在 `~/.nota/config.toml` 里配置（或在 `nota onboard` 向导里填写）：

```toml
[onebot]
enabled = true
mode = "ws"                        # 目前仅支持正向 WebSocket
ws_url = "ws://127.0.0.1:3001"     # 你的 OneBot 实现的 WS 服务地址
access_token = ""                  # 可选（以 Authorization: Bearer 发送）
persona = "default"                # 处理消息的 persona；留空则用第一个
prefix = ""                        # 可选：只响应以此开头的内容，并去掉前缀
friend_ids = [123456789]           # 好友白名单：只回复这些好友的私聊
group_ids = []                     # 群白名单：只回复这些群
```

注意：

- **路由**：每个聊天端点（好友/群/Web）都是独立 session。persona 的最终回答
  自动路由回原会话；`skip_reply` / 空输出可抑制回复（"不要回答"会真的不回）。
  `send_message(target: "private:<QQ>" | "group:<QQ>", content)` 让 persona
  可以主动向任意白名单会话发消息。
- **白名单**：只有 `friend_ids` / `group_ids` 里的人/群能到达 persona 并收到
  回复；列表为空 = 该类别谁也不回复。向非白名单目标外发消息需用 `同意`/`拒绝`
  批准。
- **媒体**：非文本消息段（图片、表情、@ 等）以 `[{segment_type} msg id:<id>]`
  （如 `[image msg id:123]`）形式到达，persona 知道收到了什么、能用哪个工具
  取内容；回复为纯文本，超过 4000 字符自动分段。
- **工具**：`read_group_chat`（经 NapCat `get_group_msg_history` 拉取**任意群**
  的最近消息，只读不发言）、`get_msg`、`get_login_info`、`get_voice_text`
  （NapCat `fetch_ptt_text` 语音转写）。
- OneBot 没有在线授权通道，工具需要授权时会自动拒绝并在聊天里提示。
- `enabled = true` 时至少需要一个 persona（或配置有效的 `persona` 名字）。

## 架构

四个 crate；依赖方向严格单向 `nota-cli → nota-infra → nota-core`
（`nota-onebot` 也只依赖 core）。

| Crate | 职责 | 关键依赖 |
|-------|------|---------|
| `nota-core` | 领域实体、端口 trait（`PersonaStore`、`LlmClient`、`Tool`、`ToolRegistry`、`AgentRunner`）、`EventBus`、`PermissionRegistry`、`SessionManager`。纯净：无 I/O。 | `log`、`serde`、`async-trait`、`chrono`、`anyhow`、`tokio`（sync） |
| `nota-infra` | 适配器：`axum` HTTP（REST + WS）、文件系统 persona store、`OpenAiLlm`（Responses API）、SQLite 历史存储、TOML 配置、内置工具。 | `nota-core`、`nota-onebot`、`axum`（ws）、`reqwest`、`rusqlite`、`serde_json` |
| `nota-onebot` | OneBot 11 正向 WS 传输：协议类型、WS 客户端、总线桥接、工具。 | `nota-core`、`tokio-tungstenite`、`serde_json`、`uuid` |
| `nota-cli` | 二进制（`nota`）：`onboard` 向导 / 运行服务。装配适配器（DI）。 | `nota-core`、`nota-infra`、`nota-onebot`、`tracing`、`dialoguer`、`console` |

### 运行时模型

总线传递 `BusEvent { kind, sender, content, request_id, parent_request_id,
target, … }`。`target` 把消息路由到指定 persona；缺省时所有订阅者都会收到。
每个 persona 运行自己的 `PersonaRuntime` 循环：接收事件 → 用 `solo.md` +
历史拼装 prompt → 调用 LLM → 处理工具调用 → 把回复投回总线。HTTP/WS 层是
一个订阅者，只转发匹配各连接 `active_request_ids` 的事件，多个客户端之间
不会互相泄露消息。

### 权限流程

工具要做需要批准的事（如 `file_read` 访问工作区之外路径）时调用
`ToolContext::request_permission(prompt)` → 在 `PermissionRegistry` 注册
oneshot + 发 `PermissionRequest` 总线事件 → WS 层转发
`{type:"permission_needed", permission_id, prompt, request_id}` 给客户端 →
用户回复 `{type:"permission", permission_id, approved}` → resolver 完成
oneshot → 工具恢复执行，最终回复以 `{type:"message", content, request_id}`
流回。

## 技术栈

Rust 2024 · Axum 0.8（REST + WebSocket）· Tokio · reqwest · rusqlite ·
serde · TOML · `log`（core/infra）/ `tracing`（cli）· dialoguer + console（向导）

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
| GET | `/api/personas/:name/chatlog/:session_id` | 读会话历史 |
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
{ "type": "permission_needed", "permission_id": "<uuid>", "prompt": "...", "request_id": "<uuid>" }
{ "type": "error",             "content": "..." }
```

## 目录结构

```
crates/
├── nota-core/    # 领域 + 端口 + EventBus + PermissionRegistry
├── nota-infra/   # 适配器（HTTP/WS、persona_store、llm、history、config、tools）
├── nota-onebot/  # OneBot 11 正向 WS 传输
└── nota-cli/     # 二进制：`nota`（服务）/ `nota onboard`

~/.nota/
├── personas/<name>/       # solo.md（系统提示词）、memory.md
├── sessions/<id>/history.db  # 每会话一个 SQLite 历史库
├── .logs/                 # 日志（30 天轮转）
└── config.toml            # api_url、api_key、model、web_search、[onebot]
```

`base_dir()` 在 `nota-cli` 里解析（`dirs::home_dir().join(".nota")`），注入到
适配器；core 不接触路径。

## 文档

- [`.agent/guide.md`](.agent/guide.md) — 架构、提交规范、踩坑记录
- [`.agent/notes.md`](.agent/notes.md) — 设计决策与当前架构
- [`AGENTS.md`](AGENTS.md) — AI 编程助手必读
