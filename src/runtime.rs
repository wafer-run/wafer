use std::collections::HashMap;
use std::panic;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::block::Block;
use crate::config::*;
use crate::context::RuntimeContext;
use crate::executor::{matches_pattern, extract_path_vars};
use crate::observability::{ObservabilityBus, ObservabilityContext};
use crate::registry::{BlockFactory, Registry, StructBlockFactory};
use crate::services::Services;
use crate::types::*;

/// Wafer is the WAFER runtime. It manages block registration, chain storage,
/// and execution.
pub struct Wafer {
    pub(crate) registry: Registry,
    pub(crate) chains: HashMap<String, Chain>,
    pub(crate) resolved: HashMap<String, Arc<dyn Block>>,
    pub(crate) named_services: Arc<HashMap<String, Box<dyn std::any::Any + Send + Sync>>>,
    pub(crate) platform_services: Option<Arc<Services>>,
    pub hooks: ObservabilityBus,
}

impl Wafer {
    /// Create a new Wafer runtime.
    pub fn new() -> Self {
        Self {
            registry: Registry::new(),
            chains: HashMap::new(),
            resolved: HashMap::new(),
            named_services: Arc::new(HashMap::new()),
            platform_services: None,
            hooks: ObservabilityBus::new(),
        }
    }

    /// Registry returns the block registry.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// RegisterService registers a named service accessible to blocks.
    pub fn register_service(&mut self, name: impl Into<String>, svc: Box<dyn std::any::Any + Send + Sync>) {
        Arc::get_mut(&mut self.named_services)
            .expect("cannot register service after cloning named_services")
            .insert(name.into(), svc);
    }

    /// Service returns a registered service by name.
    pub fn service(&self, name: &str) -> Option<&(dyn std::any::Any + Send + Sync)> {
        self.named_services.get(name).map(|s| s.as_ref())
    }

    /// RegisterPlatformServices sets the typed platform services.
    pub fn register_platform_services(&mut self, svc: Services) {
        self.platform_services = Some(Arc::new(svc));
    }

    /// HasBlock returns true if a block with the given type name is registered.
    pub fn has_block(&self, type_name: &str) -> bool {
        self.registry.has(type_name)
    }

    /// RegisterBlock registers a block instance under the given type name.
    pub fn register_block(&mut self, type_name: impl Into<String>, block: Arc<dyn Block>) {
        let block_clone = block.clone();
        let _ = self.registry.register(
            type_name,
            Arc::new(StructBlockFactory {
                new_func: move || block_clone.clone(),
            }),
        );
    }

    /// RegisterBlockFunc registers an inline handler function as a block.
    pub fn register_block_func(
        &mut self,
        type_name: impl Into<String>,
        handler: impl Fn(&dyn crate::context::Context, &mut Message) -> Result_ + Send + Sync + 'static,
    ) {
        let _ = self.registry.register_func(type_name, handler);
    }

    /// AddChain adds a programmatically-built chain to the runtime.
    pub fn add_chain(&mut self, chain: Chain) {
        self.chains.insert(chain.id.clone(), chain);
    }

    /// AddChainDef adds a chain from a JSON definition.
    pub fn add_chain_def(&mut self, def: &ChainDef) {
        let chain = chain_def_to_chain(def);
        self.add_chain(chain);
    }

    /// Resolve walks all chain trees and resolves block references.
    pub fn resolve(&mut self) -> Result<(), String> {
        let chain_ids: Vec<String> = self.chains.keys().cloned().collect();
        for chain_id in chain_ids {
            // Take chain out temporarily
            let mut chain = self.chains.remove(&chain_id).unwrap();
            self.resolve_node(&mut chain.root)?;
            self.chains.insert(chain_id.clone(), chain);
        }
        Ok(())
    }

    fn resolve_node(&mut self, node: &mut Node) -> Result<(), String> {
        // Parse config map
        if let Some(ref config) = node.config {
            node.config_map = parse_config_map(config);
        }

        if !node.block.is_empty() {
            if let Some(block) = self.resolved.get(&node.block) {
                node.resolved_block = Some(block.clone());
            } else {
                let factory = self
                    .registry
                    .get(&node.block)
                    .ok_or_else(|| format!("block type not found: {}", node.block))?;
                let block = factory.create(node.config.as_ref());

                // Initialize block
                let ctx = RuntimeContext {
                    chain_id: String::new(),
                    node_id: String::new(),
                    config: node.config_map.clone(),
                    cancelled: Arc::new(AtomicBool::new(false)),
                    named_services: self.named_services.clone(),
                    platform_services: self.platform_services.clone(),
                };

                block
                    .lifecycle(
                        &ctx,
                        LifecycleEvent {
                            event_type: LifecycleType::Init,
                            data: node
                                .config
                                .as_ref()
                                .map(|c| serde_json::to_vec(c).unwrap_or_default())
                                .unwrap_or_default(),
                        },
                    )
                    .map_err(|e| format!("init block {:?}: {}", node.block, e))?;

                self.resolved.insert(node.block.clone(), block.clone());
                node.resolved_block = Some(block);
            }
        }

        for child in &mut node.next {
            self.resolve_node(child)?;
        }
        Ok(())
    }

    /// Start initializes the runtime.
    pub fn start(&mut self) -> Result<(), String> {
        if self.resolved.is_empty() {
            return self.resolve();
        }
        Ok(())
    }

    /// Stop shuts down all resolved block instances.
    pub fn stop(&self) {
        let ctx = RuntimeContext {
            chain_id: "shutdown".to_string(),
            node_id: "shutdown".to_string(),
            config: HashMap::new(),
            cancelled: Arc::new(AtomicBool::new(false)),
            named_services: self.named_services.clone(),
            platform_services: self.platform_services.clone(),
        };
        for block in self.resolved.values() {
            let _ = block.lifecycle(
                &ctx,
                LifecycleEvent {
                    event_type: LifecycleType::Stop,
                    data: Vec::new(),
                },
            );
        }
    }

    /// Execute runs a chain by ID with the given message.
    pub fn execute(&self, chain_id: &str, msg: &mut Message) -> Result_ {
        let chain = match self.chains.get(chain_id) {
            Some(c) => c,
            None => {
                return Result_ {
                    action: Action::Error,
                    error: Some(WaferError::new(
                        "chain_not_found",
                        format!("chain not found: {}", chain_id),
                    )),
                    response: None,
                    message: None,
                };
            }
        };

        // Observability: chain start
        self.hooks.fire_chain_start(chain_id, msg);
        let start = Instant::now();

        // Set up chain-level timeout via cancellation flag
        let cancelled = Arc::new(AtomicBool::new(false));
        let timeout = chain.config.timeout;

        if !timeout.is_zero() {
            let cancelled_clone = cancelled.clone();
            std::thread::spawn(move || {
                std::thread::sleep(timeout);
                cancelled_clone.store(true, Ordering::Relaxed);
            });
        }

        let result = self.execute_node(&chain.root, msg, chain_id, &chain.config.on_error, &cancelled, "root");

        // Check timeout
        let result = if !timeout.is_zero() && cancelled.load(Ordering::Relaxed) && result.action != Action::Error {
            Result_ {
                action: Action::Error,
                error: Some(WaferError::new(
                    "deadline_exceeded",
                    format!("chain {:?} timed out after {:?}", chain_id, timeout),
                )),
                response: None,
                message: result.message,
            }
        } else {
            result
        };

        // Observability: chain end
        self.hooks.fire_chain_end(chain_id, &result, start.elapsed());

        result
    }

    fn execute_node(
        &self,
        node: &Node,
        msg: &mut Message,
        chain_id: &str,
        on_error: &str,
        cancelled: &Arc<AtomicBool>,
        node_path: &str,
    ) -> Result_ {
        // Handle chain references
        if !node.chain.is_empty() {
            return self.execute_chain_ref(node, msg, on_error, cancelled);
        }

        let block = match &node.resolved_block {
            Some(b) => b.clone(),
            None => {
                return Result_ {
                    action: Action::Error,
                    error: Some(WaferError::new(
                        "unresolved",
                        format!("block not resolved: {}", node.block),
                    )),
                    response: None,
                    message: None,
                };
            }
        };

        // Build context for this node
        let ctx = RuntimeContext {
            chain_id: chain_id.to_string(),
            node_id: node_path.to_string(),
            config: node.config_map.clone(),
            cancelled: cancelled.clone(),
            named_services: self.named_services.clone(),
            platform_services: self.platform_services.clone(),
        };

        // Observability: block start
        let obs_ctx = ObservabilityContext {
            chain_id: chain_id.to_string(),
            node_path: node_path.to_string(),
            block_name: node.block.clone(),
            trace_id: msg.get_meta("trace_id").to_string(),
            message: Some(msg.clone()),
        };
        self.hooks.fire_block_start(&obs_ctx);
        let start = Instant::now();

        // Execute block with panic recovery
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            block.handle(&ctx, msg)
        }));

        let result = match result {
            Ok(r) => r,
            Err(panic_info) => {
                let panic_msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                Result_ {
                    action: Action::Error,
                    error: Some(WaferError::new("panic", format!("block panicked: {}", panic_msg))),
                    response: None,
                    message: Some(msg.clone()),
                }
            }
        };

        // Observability: block end
        self.hooks.fire_block_end(&obs_ctx, &result, start.elapsed());

        // Process result
        match result.action {
            Action::Respond | Action::Drop => return result,
            Action::Error => {
                if on_error == "stop" {
                    return result;
                }
                // on_error=continue: fall through to children
            }
            Action::Continue => {}
        }

        // Update message from result if available
        if let Some(ref result_msg) = result.message {
            *msg = result_msg.clone();
        }

        if node.next.is_empty() {
            if result.action == Action::Error {
                // on_error=continue with no more nodes: swallow error
                return Result_::continue_with(msg.clone());
            }
            return result;
        }

        self.execute_first_match(&node.next, msg, chain_id, on_error, cancelled, node_path)
    }

    fn execute_chain_ref(
        &self,
        node: &Node,
        msg: &mut Message,
        on_error: &str,
        cancelled: &Arc<AtomicBool>,
    ) -> Result_ {
        let chain = match self.chains.get(&node.chain) {
            Some(c) => c,
            None => {
                return Result_ {
                    action: Action::Error,
                    error: Some(WaferError::new(
                        "not_found",
                        format!("referenced chain not found: {}", node.chain),
                    )),
                    response: None,
                    message: None,
                };
            }
        };

        let result = self.execute_node(&chain.root, msg, &chain.id, &chain.config.on_error, cancelled, "root");

        if result.action == Action::Continue && !node.next.is_empty() {
            return self.execute_first_match(
                &node.next,
                msg,
                &chain.id,
                on_error,
                cancelled,
                &format!("ref:{}", node.chain),
            );
        }

        result
    }

    fn execute_first_match(
        &self,
        nodes: &[Box<Node>],
        msg: &mut Message,
        chain_id: &str,
        on_error: &str,
        cancelled: &Arc<AtomicBool>,
        parent_path: &str,
    ) -> Result_ {
        for (i, child) in nodes.iter().enumerate() {
            if !matches_pattern(&child.match_pattern, &msg.kind) {
                continue;
            }
            // Extract path variables from HTTP route patterns
            if !child.match_pattern.is_empty() {
                if let Some(idx) = child.match_pattern.find(":/") {
                    let pattern_path = &child.match_pattern[idx + 1..];
                    if let Some(msg_idx) = msg.kind.find(":/") {
                        let msg_path = msg.kind[msg_idx + 1..].to_string();
                        extract_path_vars(pattern_path, &msg_path, msg);
                    }
                }
            }
            let child_path = format!("{}.{}", parent_path, i);
            return self.execute_node(child, msg, chain_id, on_error, cancelled, &child_path);
        }
        Result_::continue_with(msg.clone())
    }

    /// ChainsWithHTTP returns all chains that have HTTP route declarations.
    pub fn chains_with_http(&self) -> Vec<&Chain> {
        self.chains
            .values()
            .filter(|c| c.http.as_ref().map_or(false, |h| !h.routes.is_empty()))
            .collect()
    }

    /// GetChain returns a chain by ID.
    pub fn get_chain(&self, id: &str) -> Option<&Chain> {
        self.chains.get(id)
    }

    /// Chains returns info about all loaded chains.
    pub fn chains_info(&self) -> Vec<ChainInfo> {
        self.chains
            .values()
            .map(|c| ChainInfo {
                id: c.id.clone(),
                summary: c.summary.clone(),
                on_error: c.config.on_error.clone(),
                timeout: if c.config.timeout.is_zero() {
                    String::new()
                } else {
                    format!("{}s", c.config.timeout.as_secs())
                },
            })
            .collect()
    }

    /// ChainDefs serializes all runtime chains back to ChainDef format.
    pub fn chain_defs(&self) -> Vec<ChainDef> {
        self.chains.values().map(chain_to_chain_def).collect()
    }
}

impl Default for Wafer {
    fn default() -> Self {
        Self::new()
    }
}
