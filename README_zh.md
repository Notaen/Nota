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

Web UI 在独立仓库（[`Notaen/Nota.Webui`](https://github.com/Notaen/Nota.Webui)），
作为 git 子模块引入。构建一次后通过 CLI serve：

```sh
git submodule update --init
cd webui && bun install && bun run build && cd ..
cargo run -p nota-cli -- webui     # 在 :5173 serve webui/dist/
```

然后打开 <http://127.0.0.1:5173>。浏览器通过 WebSocket（`/ws/chat`）和
REST（`/api/*`）与 Rust 服务通信。

## 架构

Cargo 工作区有三个 crate；依赖方向严格单向
`nota-cli → nota-infra → nota-core`。

| Crate | 职责 | 关键依赖 |
|-------|------|---------|
| `nota-core` | 领域实体、端口 trait（`PersonaStore`、`LlmClient`、`Tool`、`ToolRegistry`、`AgentRunner`）、`EventBus`、`PermissionRegistry`。纯净：无 I/O，无 JSON 序列化。 | `log`、`serde`、`async-trait`、`chrono`、`anyhow`、`tokio`（sync） |
| `nota-infra` | 适配器：`axum` HTTP（REST + WebSocket）、文件系统 persona store、`OpenAiLlm`、TOML 配置、内置工具。实现 `nota-core` 的端口。 | `nota-core`、`axum`（含 `ws` feature）、`reqwest`、`serde_json`、`tower-http` |
| `nota-cli` | 二进制（`nota`）。子命令 `onboard`（向导）/ `webui`（静态服务）/ 默认（运行服务）。装配并启动一切。 | `nota-core`、`nota-infra`、`tracing`、`dialoguer` |

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

前端：Bun + React 19 + Vite 6 + Tailwind 3（位于 `Nota.Webui`）

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
├── crates/
│   ├── nota-core/    # 领域 + 端口 + EventBus + PermissionRegistry
│   ├── nota-infra/   # 适配器（axum HTTP/WS、persona_store、llm、config、tools）
│   ├── nota-cli/     # 二进制：`nota`（服务）/ `nota webui`（静态）/ `nota onboard`
└── webui/            # git 子模块 → github.com/Notaen/Nota.Webui
```

运行时数据位于用户主目录：

```
~/.nota/
├── personas/
│   └── <name>/
│       ├── solo.md        # 系统提示词
│       ├── memory.md      # 长期记忆
│       └── chatlog.json   # 最近对话（dream 时回看、改写）
├── .logs/                 # 日志（30 天轮转）
└── config.toml            # api_url、api_key、model
```

`base_dir()` 在 `nota-cli` 里解析（`dirs::home_dir().join(".nota")`），注入到
适配器；core 不接触路径。

## 文档

- [`.agent/guide.md`](.agent/guide.md) — 架构、提交规范、踩坑记录
- [`.agent/notes.md`](.agent/notes.md) — 设计决策与重构历史
- [`AGENTS.md`](AGENTS.md) — AI 编程助手必读