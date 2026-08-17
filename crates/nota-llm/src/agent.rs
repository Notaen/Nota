use std::sync::Arc;

use anyhow::Result;

use crate::llm::{LlmClient, LlmItem, MessageRole, ToolCall, ToolDef};
use crate::tool::{Tool, ToolContext, ToolRegistry};

const MAX_ITERATIONS: usize = 16;

pub struct AgentRunner {
    llm: Arc<dyn LlmClient>,
    registry: Arc<dyn ToolRegistry>,
}

impl AgentRunner {
    pub fn new(llm: Arc<dyn LlmClient>, registry: Arc<dyn ToolRegistry>) -> Self {
        Self { llm, registry }
    }

    /// Register a tool on this runner (delegates to the shared registry), so
    /// its definition is automatically attached to every LLM request.
    pub fn register_tool(&self, tool: Arc<dyn Tool>) {
        self.registry.register(tool);
    }

    /// Remove a tool from this runner by name.
    pub fn unregister_tool(&self, name: &str) {
        self.registry.unregister(name);
    }

    pub async fn run(
        &self,
        system: &str,
        items: &[LlmItem],
        tool_ctx: ToolContext,
    ) -> Result<(Vec<LlmItem>, Option<String>)> {
        let mut conversation: Vec<LlmItem> = items.to_vec();
        let mut new_items: Vec<LlmItem> = Vec::new();
        let tool_defs = self.build_tool_defs();
        let mut last_response_id = None;

        for _iteration in 0..MAX_ITERATIONS {
            let response = self
                .llm
                .chat(system, &conversation, &tool_defs)
                .await?;
            if let Some(id) = &response.id {
                last_response_id = Some(id.clone());
            }

            if !response.tool_calls.is_empty() {
                for tc in &response.tool_calls {
                    // Each function_call is immediately followed by its
                    // function_call_output: DeepSeek's Responses endpoint
                    // rejects interleaved items between a call and its result.
                    let call_item = LlmItem::FunctionCall(tc.clone());
                    conversation.push(call_item.clone());
                    new_items.push(call_item);

                    match self.execute_tool(tc, &tool_ctx).await {
                        Ok(result) => {
                            let output_item = LlmItem::FunctionCallOutput {
                                call_id: tc.id.clone(),
                                output: result,
                            };
                            conversation.push(output_item.clone());
                            new_items.push(output_item);
                        }
                        Err(e) => {
                            let output_item = LlmItem::FunctionCallOutput {
                                call_id: tc.id.clone(),
                                output: format!("tool error: {e}"),
                            };
                            conversation.push(output_item.clone());
                            new_items.push(output_item);
                        }
                    }
                }
                continue;
            }

            if let Some(content) = response.content {
                let assistant_item = LlmItem::Message {
                    role: MessageRole::Assistant,
                    content,
                };
                new_items.push(assistant_item);
                return Ok((new_items, last_response_id));
            }

            return Ok((new_items, last_response_id));
        }

        anyhow::bail!("agent loop exceeded max iterations ({MAX_ITERATIONS})");
    }

    fn build_tool_defs(&self) -> Vec<ToolDef> {
        self.registry
            .list()
            .iter()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect()
    }

    async fn execute_tool(
        &self,
        tc: &ToolCall,
        ctx: &ToolContext,
    ) -> Result<String> {
        let tool = self
            .registry
            .get(&tc.name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool: {}", tc.name))?;
        tool.run(&tc.arguments, ctx.clone()).await
    }
}
