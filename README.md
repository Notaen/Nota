# Nota

**中文版：** [README.zh.md](README.zh.md)

A framework for AI agents that chat with a persona. Give each AI a name and a
personality, and it will reply on OneBot, the web, and other channels like a real
person — with memory, tools (web search, file access, scheduled reminders),
and an allowlist so it only speaks where you want it to.

> **Development status**: Nota is under active development. Breaking changes
> (config keys, CLI commands, tool names, storage layout) can land at any
> time without notice — pin the version you depend on.

- Multiple personas: independent memory and personality per persona
- OneBot support: works with NapCat and other OneBot 11 implementations
  (native Rust, no JS runtime)
- Permission-first: sending outside the allowlist requires your approval
- Human-like replies: honors "don't reply" instead of chattering on

## Quick start

Requires a Rust toolchain (rustc >= 1.85).

```sh
# 1. First-time setup: API key, model, and your first persona
cargo run -p nota-cli -- onboard

# 2. Run
cargo run -p nota-cli
```

Configuration lives in `~/.nota/config.toml`; persona files live in
`~/.nota/personas/<name>/` (`solo.md` defines its personality, `memory.md`
its long-term memory).

> With no config or no persona, `nota` fails fast and tells you to run
> `nota onboard` — it never creates things behind your back.

## Connect OneBot

1. Run an OneBot 11 implementation ([NapCat](https://napneko.github.io/) is a
   good choice), have it listen on WebSocket (default `ws://127.0.0.1:3001`),
   and note the access token if there is one.
2. Enable it in `~/.nota/config.toml`:

```toml
[onebot]
enabled = true
mode = "ws"                        # forward WebSocket only, for now
ws_url = "ws://127.0.0.1:3001"     # your OneBot server address
access_token = ""                  # set it if your server has one (Bearer auth)
persona = "default"                # which persona replies; empty = first one
prefix = ""                        # optional: only answer messages starting with this
friend_ids = [123456789]           # allowlist: only reply to these friends
group_ids = [987654321]            # allowlist: only reply to these groups
```

3. Restart `nota` and send your bot a message through OneBot.

**The allowlist is a hard boundary**: messages from people/groups outside
`friend_ids` / `group_ids` never even reach the bot, and never get a reply.
If the bot wants to message a target outside the allowlist, it asks you in the
current chat — reply 「同意」 to allow or 「拒绝」 to deny.

## Everyday use

- **Just chat**: send the bot a message and it replies.
- **Make it stay quiet**: explicitly say 「不要回复」 — it will comply.
- **Start a fresh conversation**: send `//clear`; it opens a new LLM session,
  so earlier conversation no longer enters its context (records are kept,
  they just stop affecting it).
- **Let it read files on your computer**: send `//allow_read <path>`, and
  reads under that path no longer require per-call approval (files inside its
  workspace never require approval).
- **Proactive messages**: ask it to send messages to other allowlisted
  friends/groups via its tools.

## FAQ

**It says I need to run `nota onboard`?**
The API isn't configured yet. Run `cargo run -p nota-cli -- onboard` once.

**How do I restrict it to certain people/groups?**
Set `friend_ids` / `group_ids` under `[onebot]`. An empty list means that
category is fully disabled.

**Can it see images / voice / stickers?**
It receives them as markers like `[image msg id:123]` / `[record msg id:99]`
and fetches the actual content through tools when needed (e.g. voice
transcription). Videos and stickers work the same way.

**It keeps replying when it shouldn't?**
Take 「不要回复」 seriously, or tune its personality file.

## Further reading

- [CONTRIBUTING.md](CONTRIBUTING.md) — architecture, API, and development guide
