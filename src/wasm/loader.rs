use std::sync::{Arc, Mutex};
use wasmtime::*;

use crate::block::{Block, BlockInfo};
use crate::context::Context;
use crate::types::*;

use super::host::{register_host_module, HostState};
use super::memory::*;

/// Unpack a packed i64 into (ptr, len).
/// High 32 bits = pointer, low 32 bits = length.
fn unpack_i64(packed: i64) -> (i32, i32) {
    let ptr = (packed >> 32) as i32;
    let len = (packed & 0xFFFF_FFFF) as i32;
    (ptr, len)
}

/// WASMBlock wraps a compiled WASM module and implements Block.
pub struct WASMBlock {
    engine: Engine,
    module: Module,
    linker: Linker<HostState>,
    info_cache: Mutex<Option<BlockInfo>>,
}

impl WASMBlock {
    /// Load a WASM block from a file path.
    pub fn load(path: &str) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("reading WASM file: {}", e))?;
        Self::load_from_bytes(&bytes)
    }

    /// Load a WASM block from raw bytes.
    pub fn load_from_bytes(wasm_bytes: &[u8]) -> Result<Self, String> {
        let engine = Engine::default();
        let module =
            Module::new(&engine, wasm_bytes).map_err(|e| format!("compiling WASM module: {}", e))?;

        let mut linker = Linker::new(&engine);

        // Register host module
        register_host_module(&mut linker)?;

        // Validate required exports
        for name in &["info", "handle", "lifecycle", "malloc"] {
            let has_export = module
                .exports()
                .any(|e| e.name() == *name);
            if !has_export {
                return Err(format!("WASM module missing required export: {}", name));
            }
        }

        Ok(Self {
            engine,
            module,
            linker,
            info_cache: Mutex::new(None),
        })
    }

    fn create_instance(&self, ctx: Option<Arc<dyn Context>>) -> Result<(Store<HostState>, Instance), String> {
        let mut store = Store::new(&self.engine, HostState { context: ctx });
        let instance = self
            .linker
            .instantiate(&mut store, &self.module)
            .map_err(|e| format!("instantiating WASM module: {}", e))?;
        Ok((store, instance))
    }
}

impl Block for WASMBlock {
    fn info(&self) -> BlockInfo {
        // Check cache
        if let Ok(guard) = self.info_cache.lock() {
            if let Some(ref info) = *guard {
                return info.clone();
            }
        }

        let (mut store, instance) = match self.create_instance(None) {
            Ok(r) => r,
            Err(e) => {
                return BlockInfo {
                    name: "unknown".to_string(),
                    version: "0.0.0".to_string(),
                    interface: "error".to_string(),
                    summary: format!("failed to create instance: {}", e),
                    instance_mode: InstanceMode::PerNode,
                    allowed_modes: Vec::new(),
                    admin_ui: None,
                };
            }
        };

        let info_fn = match instance.get_typed_func::<(), i64>(&mut store, "info") {
            Ok(f) => f,
            Err(e) => {
                return BlockInfo {
                    name: "unknown".to_string(),
                    version: "0.0.0".to_string(),
                    interface: "error".to_string(),
                    summary: format!("info export not found: {}", e),
                    instance_mode: InstanceMode::PerNode,
                    allowed_modes: Vec::new(),
                    admin_ui: None,
                };
            }
        };

        let (ptr, len) = match info_fn.call(&mut store, ()) {
            Ok(packed) => unpack_i64(packed),
            Err(e) => {
                return BlockInfo {
                    name: "unknown".to_string(),
                    version: "0.0.0".to_string(),
                    interface: "error".to_string(),
                    summary: format!("calling info failed: {}", e),
                    instance_mode: InstanceMode::PerNode,
                    allowed_modes: Vec::new(),
                    admin_ui: None,
                };
            }
        };

        let data = match read_from_memory(&mut store, &instance, ptr as u32, len as u32) {
            Ok(d) => d,
            Err(e) => {
                return BlockInfo {
                    name: "unknown".to_string(),
                    version: "0.0.0".to_string(),
                    interface: "error".to_string(),
                    summary: format!("reading info from memory: {}", e),
                    instance_mode: InstanceMode::PerNode,
                    allowed_modes: Vec::new(),
                    admin_ui: None,
                };
            }
        };

        let wbi: WasmBlockInfo = match serde_json::from_slice(&data) {
            Ok(i) => i,
            Err(e) => {
                return BlockInfo {
                    name: "unknown".to_string(),
                    version: "0.0.0".to_string(),
                    interface: "error".to_string(),
                    summary: format!("parsing info: {}", e),
                    instance_mode: InstanceMode::PerNode,
                    allowed_modes: Vec::new(),
                    admin_ui: None,
                };
            }
        };

        let info = block_info_from_wasm(wbi);

        // Cache it
        if let Ok(mut guard) = self.info_cache.lock() {
            *guard = Some(info.clone());
        }

        info
    }

    fn handle(&self, ctx: &dyn Context, msg: &mut Message) -> Result_ {
        // We need to create a fresh instance for each handle call
        // because WASM instances are not thread-safe.
        // SAFETY: The WASM call is synchronous — ctx outlives the call and the
        // Arc is dropped before this function returns.
        let ctx_arc: Arc<dyn Context> = unsafe {
            let ctx_static: *const (dyn Context + 'static) =
                std::mem::transmute(ctx as *const dyn Context);
            Arc::new(ContextWrapper(ctx_static))
        };

        let (mut store, instance) = match self.create_instance(Some(ctx_arc)) {
            Ok(r) => r,
            Err(e) => {
                return msg.clone().err(WaferError::new("wasm_error", e));
            }
        };

        let handle_fn = match instance.get_typed_func::<(i32, i32), i64>(&mut store, "handle") {
            Ok(f) => f,
            Err(e) => {
                return msg.clone().err(WaferError::new(
                    "wasm_error",
                    format!("handle export not found: {}", e),
                ));
            }
        };

        // Write message to WASM memory
        let wm = message_to_wasm(msg);
        let msg_data = match serde_json::to_vec(&wm) {
            Ok(d) => d,
            Err(e) => {
                return msg.clone().err(WaferError::new("wasm_memory_error", e.to_string()));
            }
        };

        let (msg_ptr, msg_len) = match write_to_memory(&mut store, &instance, &msg_data) {
            Ok(r) => r,
            Err(e) => {
                return msg.clone().err(WaferError::new("wasm_memory_error", e));
            }
        };

        // Call handle
        let (result_ptr, result_len) = match handle_fn.call(&mut store, (msg_ptr as i32, msg_len as i32)) {
            Ok(packed) => unpack_i64(packed),
            Err(e) => {
                return msg.clone().err(WaferError::new(
                    "wasm_call_error",
                    format!("calling handle: {}", e),
                ));
            }
        };

        let result_data = match read_from_memory(&mut store, &instance, result_ptr as u32, result_len as u32) {
            Ok(d) => d,
            Err(e) => {
                return msg.clone().err(WaferError::new("wasm_decode_error", e));
            }
        };

        let wr: WasmResult = match serde_json::from_slice(&result_data) {
            Ok(r) => r,
            Err(e) => {
                return msg.clone().err(WaferError::new(
                    "wasm_decode_error",
                    format!("reading result: {}", e),
                ));
            }
        };

        let mut result = result_from_wasm(wr);
        result.message = Some(msg.clone());
        result
    }

    fn lifecycle(
        &self,
        ctx: &dyn Context,
        event: LifecycleEvent,
    ) -> std::result::Result<(), WaferError> {
        // SAFETY: Same as handle — synchronous call, ctx outlives it.
        let ctx_arc: Arc<dyn Context> = unsafe {
            let ctx_static: *const (dyn Context + 'static) =
                std::mem::transmute(ctx as *const dyn Context);
            Arc::new(ContextWrapper(ctx_static))
        };

        let (mut store, instance) = self
            .create_instance(Some(ctx_arc))
            .map_err(|e| WaferError::new("wasm_error", e))?;

        let lifecycle_fn = match instance.get_typed_func::<(i32, i32), i64>(&mut store, "lifecycle") {
            Ok(f) => f,
            Err(_) => return Ok(()), // lifecycle is optional
        };

        let we = lifecycle_event_to_wasm(&event);
        let evt_data = serde_json::to_vec(&we)
            .map_err(|e| WaferError::new("wasm_error", format!("marshaling lifecycle event: {}", e)))?;

        let (evt_ptr, evt_len) = write_to_memory(&mut store, &instance, &evt_data)
            .map_err(|e| WaferError::new("wasm_error", e))?;

        let (result_ptr, result_len) = {
            let packed = lifecycle_fn
                .call(&mut store, (evt_ptr as i32, evt_len as i32))
                .map_err(|e| WaferError::new("wasm_error", format!("calling lifecycle: {}", e)))?;
            unpack_i64(packed)
        };

        if result_len > 0 {
            let data = read_from_memory(&mut store, &instance, result_ptr as u32, result_len as u32)
                .map_err(|e| WaferError::new("wasm_error", e))?;

            #[derive(serde::Deserialize)]
            struct LcResult {
                #[serde(default)]
                ok: bool,
                #[serde(default)]
                error: String,
            }

            if let Ok(lc) = serde_json::from_slice::<LcResult>(&data) {
                if !lc.error.is_empty() {
                    return Err(WaferError::new("lifecycle_error", lc.error));
                }
            }
        }

        Ok(())
    }
}

// Helper: wrap a &dyn Context as an Arc<dyn Context> by using unsafe pointer tricks.
// This is safe because the WASMBlock::handle call is synchronous and the context
// outlives the WASM call.
struct ContextWrapper(*const dyn Context);
unsafe impl Send for ContextWrapper {}
unsafe impl Sync for ContextWrapper {}

impl Context for ContextWrapper {
    fn send(&self, msg: &Message) -> Result_ {
        unsafe { &*self.0 }.send(msg)
    }

    fn capabilities(&self) -> Vec<crate::context::CapabilityInfo> {
        unsafe { &*self.0 }.capabilities()
    }

    fn is_cancelled(&self) -> bool {
        unsafe { &*self.0 }.is_cancelled()
    }

    fn service(&self, name: &str) -> Option<&dyn std::any::Any> {
        unsafe { &*self.0 }.service(name)
    }

    fn services(&self) -> Option<&crate::services::Services> {
        unsafe { &*self.0 }.services()
    }

    fn config_get(&self, key: &str) -> Option<&str> {
        unsafe { &*self.0 }.config_get(key)
    }
}

impl From<ContextWrapper> for Arc<dyn Context> {
    fn from(w: ContextWrapper) -> Self {
        Arc::new(w)
    }
}
