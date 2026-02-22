use serde::{Deserialize, Serialize};
use wasmtime::{Instance, Store};

use crate::types::*;

/// Base64 serde module for binary data in wire format.
/// Go's encoding/json marshals []byte as base64 by default, so we match that
/// convention to ensure cross-language compatibility.
mod base64_serde {
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = base64_encode(bytes);
        serializer.serialize_str(&encoded)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if s.is_empty() {
            return Ok(Vec::new());
        }
        base64_decode(&s).map_err(serde::de::Error::custom)
    }

    /// Simple base64 encoder (standard alphabet with padding).
    fn base64_encode(input: &[u8]) -> String {
        const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut output = String::with_capacity((input.len() + 2) / 3 * 4);
        for chunk in input.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
            let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
            let triple = (b0 << 16) | (b1 << 8) | b2;
            output.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
            output.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
            if chunk.len() > 1 {
                output.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
            } else {
                output.push('=');
            }
            if chunk.len() > 2 {
                output.push(CHARS[(triple & 0x3F) as usize] as char);
            } else {
                output.push('=');
            }
        }
        output
    }

    /// Simple base64 decoder (standard alphabet with padding).
    fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
        let input = input.trim_end_matches('=');
        let mut output = Vec::with_capacity(input.len() * 3 / 4);
        let mut buf: u32 = 0;
        let mut bits: u32 = 0;
        for c in input.chars() {
            let val = match c {
                'A'..='Z' => (c as u32) - ('A' as u32),
                'a'..='z' => (c as u32) - ('a' as u32) + 26,
                '0'..='9' => (c as u32) - ('0' as u32) + 52,
                '+' => 62,
                '/' => 63,
                _ => return Err(format!("invalid base64 character: {}", c)),
            };
            buf = (buf << 6) | val;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                output.push(((buf >> bits) & 0xFF) as u8);
            }
        }
        Ok(output)
    }
}

/// WasmMessage is the JSON-serializable format for messages crossing the WASM boundary.
/// Data is base64-encoded for compatibility with Go's json.Marshal([]byte).
#[derive(Debug, Serialize, Deserialize)]
pub struct WasmMessage {
    pub kind: String,
    #[serde(with = "base64_serde", default)]
    pub data: Vec<u8>,
    pub meta: Vec<[String; 2]>,
}

/// WasmResult is the JSON-serializable format for results crossing the WASM boundary.
#[derive(Debug, Serialize, Deserialize)]
pub struct WasmResult {
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<WasmResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WasmError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WasmResponse {
    #[serde(with = "base64_serde", default)]
    pub data: Vec<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub meta: Vec<[String; 2]>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WasmError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub meta: Vec<[String; 2]>,
}

/// wasmBlockInfo is the JSON-serializable format for BlockInfo.
#[derive(Debug, Serialize, Deserialize)]
pub struct WasmBlockInfo {
    pub name: String,
    pub version: String,
    pub interface: String,
    pub summary: String,
    #[serde(default)]
    pub instance_mode: String,
    #[serde(default)]
    pub allowed_modes: Vec<String>,
}

/// wasmLifecycleEvent is the JSON-serializable format for LifecycleEvent.
#[derive(Debug, Serialize, Deserialize)]
pub struct WasmLifecycleEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub data: Vec<u8>,
}

/// Convert a Message to the WASM format.
pub fn message_to_wasm(msg: &Message) -> WasmMessage {
    WasmMessage {
        kind: msg.kind.clone(),
        data: msg.data.clone(),
        meta: msg
            .meta
            .iter()
            .map(|(k, v)| [k.clone(), v.clone()])
            .collect(),
    }
}

/// Convert a WASM message back to a Message.
pub fn message_from_wasm(wm: WasmMessage) -> Message {
    let mut meta = std::collections::HashMap::new();
    for pair in wm.meta {
        meta.insert(pair[0].clone(), pair[1].clone());
    }
    Message {
        kind: wm.kind,
        data: wm.data,
        meta,
    }
}

/// Convert a Result_ to the WASM format.
pub fn result_to_wasm(r: &Result_) -> WasmResult {
    let mut wr = WasmResult {
        action: r.action.to_string(),
        response: None,
        error: None,
    };

    if let Some(ref resp) = r.response {
        wr.response = Some(WasmResponse {
            data: resp.data.clone(),
            meta: resp
                .meta
                .iter()
                .map(|(k, v)| [k.clone(), v.clone()])
                .collect(),
        });
    }

    if let Some(ref err) = r.error {
        wr.error = Some(WasmError {
            code: err.code.clone(),
            message: err.message.clone(),
            meta: err
                .meta
                .iter()
                .map(|(k, v)| [k.clone(), v.clone()])
                .collect(),
        });
    }

    wr
}

/// Convert a WASM result to a Result_.
pub fn result_from_wasm(wr: WasmResult) -> Result_ {
    let action = match wr.action.as_str() {
        "continue" => Action::Continue,
        "respond" => Action::Respond,
        "drop" => Action::Drop,
        "error" => Action::Error,
        _ => Action::Continue,
    };

    let response = wr.response.map(|r| {
        let mut meta = std::collections::HashMap::new();
        for pair in r.meta {
            meta.insert(pair[0].clone(), pair[1].clone());
        }
        Response { data: r.data, meta }
    });

    let error = wr.error.map(|e| {
        let mut meta = std::collections::HashMap::new();
        for pair in e.meta {
            meta.insert(pair[0].clone(), pair[1].clone());
        }
        WaferError {
            code: e.code,
            message: e.message,
            meta,
        }
    });

    Result_ {
        action,
        response,
        error,
        message: None,
    }
}

/// Convert a WasmBlockInfo to a BlockInfo.
pub fn block_info_from_wasm(wbi: WasmBlockInfo) -> crate::block::BlockInfo {
    let instance_mode = InstanceMode::parse(&wbi.instance_mode).unwrap_or(InstanceMode::PerNode);

    let allowed_modes: Vec<InstanceMode> = wbi
        .allowed_modes
        .iter()
        .filter_map(|m| InstanceMode::parse(m))
        .collect();

    crate::block::BlockInfo {
        name: wbi.name,
        version: wbi.version,
        interface: wbi.interface,
        summary: wbi.summary,
        instance_mode,
        allowed_modes,
        admin_ui: None,
    }
}

/// Convert a LifecycleEvent to the WASM format.
pub fn lifecycle_event_to_wasm(event: &LifecycleEvent) -> WasmLifecycleEvent {
    let type_str = match event.event_type {
        LifecycleType::Init => "init",
        LifecycleType::Start => "start",
        LifecycleType::Stop => "stop",
    };
    WasmLifecycleEvent {
        event_type: type_str.to_string(),
        data: event.data.clone(),
    }
}

/// Write data bytes into WASM linear memory via malloc.
pub fn write_to_memory(
    store: &mut Store<super::host::HostState>,
    instance: &Instance,
    data: &[u8],
) -> Result<(u32, u32), String> {
    let malloc = instance
        .get_typed_func::<i32, i32>(&mut *store, "malloc")
        .map_err(|e| format!("malloc not found: {}", e))?;

    let size = data.len() as i32;
    let ptr = malloc
        .call(&mut *store, size)
        .map_err(|e| format!("malloc failed: {}", e))?;

    let memory = instance
        .get_memory(&mut *store, "memory")
        .ok_or("memory not found")?;

    memory
        .write(&mut *store, ptr as usize, data)
        .map_err(|e| format!("failed to write {} bytes at offset {}: {}", size, ptr, e))?;

    Ok((ptr as u32, size as u32))
}

/// Read bytes from WASM linear memory.
pub fn read_from_memory(
    store: &mut Store<super::host::HostState>,
    instance: &Instance,
    ptr: u32,
    length: u32,
) -> Result<Vec<u8>, String> {
    let memory = instance
        .get_memory(&mut *store, "memory")
        .ok_or("memory not found")?;

    let mut buf = vec![0u8; length as usize];
    memory
        .read(&*store, ptr as usize, &mut buf)
        .map_err(|e| format!("failed to read {} bytes at offset {}: {}", length, ptr, e))?;

    Ok(buf)
}
