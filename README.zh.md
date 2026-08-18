# Nota

**English:** [README.md](README.md)

让 AI 以「人设」聊天的框架。给每个 AI 定义一个名字和性格，它就能在 OneBot、
网页等渠道上像真人一样回复消息——带记忆、会调用工具（联网搜索、读文件、
定时提醒），也可以只在你指定的好友/群里说话。

> **开发状态**：Nota 正处于活跃开发阶段，破坏性变更（配置项、CLI 命令、
> 工具名、存储布局等）随时可能出现，不会另行通知——请锁定你所依赖的版本。

- 多 persona：一人一档，各自独立记忆和性格
- OneBot 接入：对接 NapCat 等 OneBot 11 实现（原生 Rust，无 JS 依赖）
- 权限可控：白名单外的目标需要你「同意」才会发送
- 说话像人：有「不要回复」契约，不会话痨式刷屏

## 快速开始

需要 Rust 工具链（rustc ≥ 1.85）。

```sh
# 1. 首次配置：填 API Key / 模型，并创建第一个 persona
cargo run -p nota-cli -- onboard

# 2. 启动
cargo run -p nota-cli
```

配置保存在 `~/.nota/config.toml`，persona 文件在 `~/.nota/personas/<名字>/`
（`solo.md` 是它的性格设定，`memory.md` 是长期记忆）。

> 没有配置或没有 persona 时，`nota` 会直接报错并提示你运行 `nota onboard`，
> 不会自动乱建东西。

## 接入 OneBot

1. 先运行一个 OneBot 11 实现（推荐 [NapCat](https://napneko.github.io/)），
   让它监听 WebSocket（默认 `ws://127.0.0.1:3001`），并记下 token（如有）。
2. 在 `~/.nota/config.toml` 里启用：

```toml
[onebot]
enabled = true
mode = "ws"                        # 目前仅支持正向 WebSocket
ws_url = "ws://127.0.0.1:3001"     # 你的 OneBot 服务地址
access_token = ""                  # 有 token 就填（Bearer 认证）
persona = "default"                # 用哪个 persona 回复；留空 = 第一个
prefix = ""                        # 可选：只有以该前缀开头的消息才回复
friend_ids = [123456789]           # 白名单：只回复这些好友
group_ids = [987654321]            # 白名单：只回复这些群
```

3. 重启 `nota`，通过 OneBot 给你的 bot 发条消息试试。

**白名单是硬边界**：不在 `friend_ids` / `group_ids` 里的人或群发的消息
连 bot 都看不到，也绝不会被回复。想让 bot 主动向白名单外的目标发消息时，
它会在当前聊天里发一条询问，你回复「同意」才发送，回复「拒绝」就取消。

## 日常使用

- **直接聊天**：给 bot 发消息，它正常回复。
- **让它闭嘴**：消息里明确说「不要回复」——它会遵守，不强行回话。
- **开启新会话**：发送 `//clear`，它会开一个全新的 LLM session，之前的对话
  不再进入它的上下文（记录仍会保留，只是不再影响它）。
- **允许它读电脑上的文件**：发送 `//allow_read <路径>`，之后它读该路径
  不需要每次问你（工作目录内的文件无需授权）。
- **主动发消息**：你可以让它通过工具把消息发到白名单内的其他好友/群。

## 常见问题

**启动时报错让我先跑 `nota onboard`？**
还没配置 API。跑一次 `cargo run -p nota-cli -- onboard` 即可。

**怎么让 bot 只在某些人/群里说话？**
在 `[onebot]` 里配 `friend_ids` / `group_ids`。留空表示该类别全部不接受。

**不想让它联网搜索？**
`config.toml` 顶层设 `web_search = false`。

**它能看到图片 / 语音 / 表情吗？**
会以 `[image msg id:123]`、`[record msg id:99]` 这样的形式知道「收到了什么」，
需要具体内容时它会调用工具获取（如语音转文字）。视频、表情同理。

**它总爱抢话？**
把它说的「不要回复」当真，或在性格设定里调整回复习惯。

## 进一步阅读

- [CONTRIBUTING.md](CONTRIBUTING.md) — 面向开发者的架构、API、目录说明
