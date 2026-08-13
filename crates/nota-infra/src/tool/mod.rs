use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use async_trait::async_trait;
use nota_core::tool::{Tool, ToolRegistry};

pub mod builtin;
pub mod chat;

pub struct ToolRegistryImpl {
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
}

impl ToolRegistryImpl {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for ToolRegistryImpl {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolRegistry for ToolRegistryImpl {
    fn register(&self, tool: Arc<dyn Tool>) {
        let mut tools = self.tools.write().unwrap();
        tools.insert(tool.name().to_string(), tool);
    }

    fn unregister(&self, name: &str) {
        let mut tools = self.tools.write().unwrap();
        tools.remove(name);
    }

    fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        let tools = self.tools.read().unwrap();
        tools.get(name).cloned()
    }

    fn list(&self) -> Vec<Arc<dyn Tool>> {
        let tools = self.tools.read().unwrap();
        tools.values().cloned().collect()
    }
}
