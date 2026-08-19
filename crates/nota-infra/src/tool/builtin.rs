use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use chrono::DateTime;
use nota_core::conversation::Conversation;
use nota_core::permissions::PathPolicy;
use nota_core::scheduler::Scheduler;
use nota_core::tool::{PropertyDef, Tool, ToolContext, ToolParams, ToolRegistry, Value};

pub struct FileReadTool {
    personas_dir: PathBuf,
    policy: Arc<PathPolicy>,
    conversation_id: Option<String>,
}

impl FileReadTool {
    pub fn new(
        personas_dir: PathBuf,
        policy: Arc<PathPolicy>,
        conversation_id: Option<String>,
    ) -> Self {
        Self {
            personas_dir,
            policy,
            conversation_id,
        }
    }

    fn workspace(&self, name: &str) -> PathBuf {
        self.personas_dir.join(name)
    }

    async fn ask_permission(&self, ctx: &ToolContext, prompt: String) -> bool {
        match &self.conversation_id {
            Some(conversation_id) => ctx.request_permission(conversation_id, prompt).await,
            None => false,
        }
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "file_read"
    }

    fn description(&self) -> &str {
        "Read a file within the persona workspace"
    }

    fn parameters(&self) -> ToolParams {
        let mut props = HashMap::new();
        props.insert(
            "path".to_string(),
            PropertyDef {
                prop_type: "string".to_string(),
                description: "Relative path within the persona workspace".to_string(),
                r#enum: vec![],
            },
        );
        ToolParams::object(props, vec!["path".to_string()])
    }

    async fn run(
        &self,
        args: HashMap<String, Value>,
        ctx: ToolContext,
    ) -> Result<String> {
        let rel = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'path' argument"))?;

        let workspace = self.workspace(&ctx.persona_name);
        let resolved = workspace.join(rel);

        let workspace_canonical = tokio::fs::canonicalize(&workspace).await?;
        let canonical = match tokio::fs::canonicalize(&resolved).await {
            Ok(c) => c,
            Err(_) => {
                if !resolved.starts_with(&workspace_canonical) {
                    // User-guided allowlist lets the persona read outside the
                    // workspace without per-call approval.
                    if !self.policy.is_read_allowed(&resolved).await {
                        let prompt = format!(
                            "{} wants to read outside its workspace: {}",
                            ctx.persona_name,
                            resolved.display()
                        );
                        let approved = self.ask_permission(&ctx, prompt).await;
                        if !approved {
                            anyhow::bail!("permission denied");
                        }
                    }
                }
                tokio::fs::canonicalize(&resolved).await?
            }
        };

        if !canonical.starts_with(&workspace_canonical)
            && !self.policy.is_read_allowed(&canonical).await
        {
            let prompt = format!(
                "{} wants to read outside its workspace: {}",
                ctx.persona_name,
                resolved.display()
            );
            let approved = self.ask_permission(&ctx, prompt).await;
            if !approved {
                anyhow::bail!("permission denied");
            }
        }

        let content = tokio::fs::read_to_string(&canonical).await?;
        Ok(content)
    }
}

pub struct FileWriteTool {
    personas_dir: PathBuf,
    conversation_id: Option<String>,
}

impl FileWriteTool {
    pub fn new(personas_dir: PathBuf, conversation_id: Option<String>) -> Self {
        Self {
            personas_dir,
            conversation_id,
        }
    }

    fn workspace(&self, name: &str) -> PathBuf {
        self.personas_dir.join(name)
    }
}

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "file_write"
    }

    fn description(&self) -> &str {
        "Write content to a file within the persona workspace. Creates parent directories if needed."
    }

    fn parameters(&self) -> ToolParams {
        let mut props = HashMap::new();
        props.insert(
            "path".to_string(),
            PropertyDef {
                prop_type: "string".to_string(),
                description: "Relative path within the persona workspace".to_string(),
                r#enum: vec![],
            },
        );
        props.insert(
            "content".to_string(),
            PropertyDef {
                prop_type: "string".to_string(),
                description: "Content to write".to_string(),
                r#enum: vec![],
            },
        );
        ToolParams::object(props, vec!["path".to_string(), "content".to_string()])
    }

    async fn run(
        &self,
        args: HashMap<String, Value>,
        ctx: ToolContext,
    ) -> Result<String> {
        let rel = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'path' argument"))?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'content' argument"))?;

        let workspace = self.workspace(&ctx.persona_name);
        let resolved = workspace.join(rel);

        let canonical = tokio::fs::canonicalize(&workspace).await?;
        let target = if let Ok(c) = tokio::fs::canonicalize(&resolved).await {
            c
        } else {
            let mut clean = std::path::PathBuf::new();
            for component in resolved.components() {
                clean.push(component);
            }
            clean
        };

        if !target.starts_with(&canonical) && target != canonical {
            let prompt = format!(
                "{} wants to write outside its workspace: {}",
                ctx.persona_name,
                resolved.display()
            );
            let approved = match &self.conversation_id {
                Some(conversation_id) => ctx.request_permission(conversation_id, prompt).await,
                None => false,
            };
            if !approved {
                anyhow::bail!("permission denied");
            }
        }

        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&target, content).await?;
        Ok(format!("ok: wrote {} bytes", content.len()))
    }
}

pub struct ScheduleTool {
    scheduler: Arc<dyn Scheduler>,
    conversation_id: Option<String>,
}

impl ScheduleTool {
    pub fn new(scheduler: Arc<dyn Scheduler>, conversation_id: Option<String>) -> Self {
        Self {
            scheduler,
            conversation_id,
        }
    }
}

#[async_trait]
impl Tool for ScheduleTool {
    fn name(&self) -> &str {
        "schedule"
    }

    fn description(&self) -> &str {
        "Schedule a reminder to be delivered in this conversation at a future time (ISO 8601 trigger_at)."
    }

    fn parameters(&self) -> ToolParams {
        let mut props = HashMap::new();
        props.insert(
            "message".to_string(),
            PropertyDef {
                prop_type: "string".to_string(),
                description: "Message content to deliver".to_string(),
                r#enum: vec![],
            },
        );
        props.insert(
            "trigger_at".to_string(),
            PropertyDef {
                prop_type: "string".to_string(),
                description: "ISO 8601 datetime when the message should be delivered".to_string(),
                r#enum: vec![],
            },
        );
        ToolParams::object(
            props,
            vec!["message".to_string(), "trigger_at".to_string()],
        )
    }

    async fn run(
        &self,
        args: HashMap<String, Value>,
        ctx: ToolContext,
    ) -> Result<String> {
        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing or empty 'message'"))?;
        let trigger_at = args
            .get("trigger_at")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing 'trigger_at'"))?;
        let parsed = DateTime::parse_from_rfc3339(trigger_at)
            .map_err(|e| anyhow::anyhow!("invalid trigger_at (expected ISO 8601): {e}"))?;
        let conversation_id = self
            .conversation_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no conversation available for scheduling"))?;
        let conversation = Conversation::new(ctx.persona_name.clone(), conversation_id);
        self.scheduler
            .schedule_reminder(parsed.timestamp(), conversation, message.to_string())
            .await?;
        Ok(format!("已安排于 {trigger_at} 提醒"))
    }
}

pub struct StatusTool {
    started: std::time::Instant,
    conversation_id: Option<String>,
}

impl StatusTool {
    pub fn new(conversation_id: Option<String>) -> Self {
        Self {
            started: std::time::Instant::now(),
            conversation_id,
        }
    }
}

impl Default for StatusTool {
    fn default() -> Self {
        Self::new(None)
    }
}

#[async_trait]
impl Tool for StatusTool {
    fn name(&self) -> &str {
        "status"
    }

    fn description(&self) -> &str {
        "Get detailed Nota runtime status: version, platform, process id, uptime, and the current persona/conversation"
    }

    fn parameters(&self) -> ToolParams {
        ToolParams::object(HashMap::new(), vec![])
    }

    async fn run(
        &self,
        _args: HashMap<String, Value>,
        ctx: ToolContext,
    ) -> Result<String> {
        let status = serde_json::json!({
            "name": "nota",
            "version": env!("CARGO_PKG_VERSION"),
            "platform": {
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "family": std::env::consts::FAMILY,
            },
            "pid": std::process::id(),
            "uptime_secs": self.started.elapsed().as_secs(),
            "persona": ctx.persona_name,
            "conversation_id": self.conversation_id,
            "request_id": ctx.request_id,
        });
        Ok(serde_json::to_string_pretty(&status)?)
    }
}

pub fn register_builtin_tools(
    registry: &ToolRegistry,
    personas_dir: PathBuf,
    scheduler: Arc<dyn Scheduler>,
    policy: Arc<PathPolicy>,
    conversation_id: Option<String>,
) -> Result<()> {
    registry.register(Arc::new(FileReadTool::new(
        personas_dir.clone(),
        policy.clone(),
        conversation_id.clone(),
    )))?;
    registry.register(Arc::new(FileWriteTool::new(
        personas_dir,
        conversation_id.clone(),
    )))?;
    registry.register(Arc::new(ScheduleTool::new(scheduler, conversation_id.clone())))?;
    registry.register(Arc::new(StatusTool::new(conversation_id)))?;
    Ok(())
}
