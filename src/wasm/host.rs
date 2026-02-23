use std::sync::Arc;
use wasmtime::*;

use crate::context::Context;
use super::memory::*;

/// HostState stores the wafer Context for host function calls.
pub struct HostState {
    pub context: Option<Arc<dyn Context>>,
}

/// Pack a (ptr, len) pair into a single i64.
/// High 32 bits = ptr, low 32 bits = len.
/// This matches the convention used by all guest SDKs (Rust, Go, AssemblyScript).
fn pack_i64(ptr: i32, len: i32) -> i64 {
    ((ptr as i64) << 32) | ((len as i64) & 0xFFFF_FFFF)
}

/// Register the "wafer" host module with send, capabilities, and is_cancelled functions.
pub fn register_host_module(linker: &mut Linker<HostState>) -> Result<(), String> {
    // send(msg_ptr: i32, msg_len: i32) -> i64 (packed ptr|len)
    linker
        .func_wrap(
            "wafer",
            "send",
            |mut caller: Caller<'_, HostState>, msg_ptr: i32, msg_len: i32| -> i64 {
                let ctx = match &caller.data().context {
                    Some(c) => c.clone(),
                    None => return write_error_result_inline(&mut caller, "internal", "no wafer context"),
                };

                // Read message from guest memory
                let memory = match caller.get_export("memory") {
                    Some(Extern::Memory(m)) => m,
                    _ => return write_error_result_inline(&mut caller, "memory_error", "memory not found"),
                };

                const MAX_WASM_MSG_SIZE: usize = 16 * 1024 * 1024; // 16 MB
                if msg_len < 0 || msg_len as usize > MAX_WASM_MSG_SIZE {
                    return write_error_result_inline(&mut caller, "msg_too_large",
                        &format!("message size {} exceeds maximum {}", msg_len, MAX_WASM_MSG_SIZE));
                }
                let mut buf = vec![0u8; msg_len as usize];
                if let Err(_) = memory.read(&caller, msg_ptr as usize, &mut buf) {
                    return write_error_result_inline(&mut caller, "memory_error", "read failed");
                }

                let wm: WasmMessage = match serde_json::from_slice(&buf) {
                    Ok(m) => m,
                    Err(e) => return write_error_result_inline(&mut caller, "decode_error", &e.to_string()),
                };

                let msg = message_from_wasm(wm);
                let result = ctx.send(&msg);
                let wr = result_to_wasm(&result);

                let result_data = match serde_json::to_vec(&wr) {
                    Ok(d) => d,
                    Err(e) => return write_error_result_inline(&mut caller, "encode_error", &e.to_string()),
                };

                // Write result back to guest memory via malloc
                let malloc = match caller.get_export("malloc") {
                    Some(Extern::Func(f)) => f,
                    _ => return 0,
                };

                let size = result_data.len() as i32;
                let mut result_vals = vec![Val::I32(0)];
                if let Err(_) = malloc.call(&mut caller, &[Val::I32(size)], &mut result_vals) {
                    return 0;
                }

                let ptr = match &result_vals[0] {
                    Val::I32(p) => *p,
                    _ => return 0,
                };

                let memory = match caller.get_export("memory") {
                    Some(Extern::Memory(m)) => m,
                    _ => return 0,
                };

                if let Err(_) = memory.write(&mut caller, ptr as usize, &result_data) {
                    return 0;
                }

                pack_i64(ptr, size)
            },
        )
        .map_err(|e| e.to_string())?;

    // capabilities() -> i64 (packed ptr|len)
    linker
        .func_wrap(
            "wafer",
            "capabilities",
            |mut caller: Caller<'_, HostState>| -> i64 {
                let caps = match &caller.data().context {
                    Some(ctx) => ctx.capabilities(),
                    None => Vec::new(),
                };

                let data = serde_json::to_vec(&caps).unwrap_or_else(|_| b"[]".to_vec());

                let malloc = match caller.get_export("malloc") {
                    Some(Extern::Func(f)) => f,
                    _ => return 0,
                };

                let size = data.len() as i32;
                let mut result_vals = vec![Val::I32(0)];
                if let Err(_) = malloc.call(&mut caller, &[Val::I32(size)], &mut result_vals) {
                    return 0;
                }

                let ptr = match &result_vals[0] {
                    Val::I32(p) => *p,
                    _ => return 0,
                };

                let memory = match caller.get_export("memory") {
                    Some(Extern::Memory(m)) => m,
                    _ => return 0,
                };

                if let Err(_) = memory.write(&mut caller, ptr as usize, &data) {
                    return 0;
                }

                pack_i64(ptr, size)
            },
        )
        .map_err(|e| e.to_string())?;

    // is_cancelled() -> i32
    linker
        .func_wrap(
            "wafer",
            "is_cancelled",
            |caller: Caller<'_, HostState>| -> i32 {
                match &caller.data().context {
                    Some(ctx) => {
                        if ctx.is_cancelled() {
                            1
                        } else {
                            0
                        }
                    }
                    None => 0,
                }
            },
        )
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// Helper to write an error result directly into WASM memory.
fn write_error_result_inline(caller: &mut Caller<'_, HostState>, code: &str, message: &str) -> i64 {
    let wr = WasmResult {
        action: "error".to_string(),
        response: None,
        error: Some(WasmError {
            code: code.to_string(),
            message: message.to_string(),
            meta: Vec::new(),
        }),
    };

    let data = match serde_json::to_vec(&wr) {
        Ok(d) => d,
        Err(_) => return 0,
    };

    let malloc = match caller.get_export("malloc") {
        Some(Extern::Func(f)) => f,
        _ => return 0,
    };

    let size = data.len() as i32;
    let mut result_vals = vec![Val::I32(0)];
    if let Err(_) = malloc.call(&mut *caller, &[Val::I32(size)], &mut result_vals) {
        return 0;
    }

    let ptr = match &result_vals[0] {
        Val::I32(p) => *p,
        _ => return 0,
    };

    let memory = match caller.get_export("memory") {
        Some(Extern::Memory(m)) => m,
        _ => return 0,
    };

    if let Err(_) = memory.write(&mut *caller, ptr as usize, &data) {
        return 0;
    }

    pack_i64(ptr, size)
}
