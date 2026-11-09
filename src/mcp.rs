//! MCP (Model Context Protocol) Server 协议层。
//! 暴露 BLE 操作工具：scan / connect / disconnect / services / read / write / subscribe / status。
//!
//! 本模块只负责「解析一条 JSON-RPC 消息 -> 产出响应 Value」，不关心传输方式；
//! 传输层见 [`crate::mcp_http`]（MCP 2024-11-05 的 HTTP + SSE）。

use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::ble::BleCentral;
use crate::state::AppState;

/// MCP 服务器版本（协议 2024-11-05）。
const PROTOCOL_VERSION: &str = "2024-11-05";

/// 处理一条 JSON-RPC 消息，返回需要回送的响应。
/// 通知类消息（如 `notifications/initialized`）无响应，返回 `None`。
pub async fn handle_message(
    ble: &Arc<tokio::sync::Mutex<BleCentral>>,
    state: &Arc<RwLock<AppState>>,
    msg: Value,
) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    match method.as_str() {
        "initialize" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "ble_server", "version": env!("CARGO_PKG_VERSION") }
            }
        })),
        "notifications/initialized" => {
            // 客户端已初始化，无需响应
            None
        }
        "tools/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "tools": tools_list() }
        })),
        "ping" => Some(json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
        "tools/call" => {
            let name = msg
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let args = msg
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let result = dispatch_tool(ble, state, &name, &args).await;
            Some(match result {
                Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "content": [ { "type": "text", "text": r } ],
                    "isError": false
                }}),
                Err(e) => json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "content": [ { "type": "text", "text": format!("错误: {e}") } ],
                    "isError": true
                }}),
            })
        }
        _ => {
            if id.is_some() {
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("方法未找到: {method}")}
                }))
            } else {
                None
            }
        }
    }
}

fn tools_list() -> Value {
    json!([
        {
            "name": "scan",
            "description": "扫描周围 BLE 设备（持续指定秒数），返回设备列表（地址/名称/RSSI）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "seconds": { "type": "integer", "description": "扫描时长（秒），默认 5", "minimum": 1, "maximum": 60 }
                },
                "required": []
            }
        },
        {
            "name": "connect",
            "description": "连接指定地址的 BLE 设备，自动配对并发现 GATT 服务。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "address": { "type": "string", "description": "设备蓝牙地址（如 11:22:33:44:55:66）" }
                },
                "required": ["address"]
            }
        },
        {
            "name": "disconnect",
            "description": "断开当前 BLE 连接。",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        },
        {
            "name": "services",
            "description": "列出已连接设备的 GATT 服务与特征及其读写/通知能力。",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        },
        {
            "name": "read",
            "description": "读取指定 UUID 特征的当前值，返回十六进制。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "char_uuid": { "type": "string", "description": "特征 UUID，如 00002a00-0000-1000-8000-00805f9b34fb" }
                },
                "required": ["char_uuid"]
            }
        },
        {
            "name": "write",
            "description": "向指定 UUID 特征写入数据（十六进制，带响应）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "char_uuid": { "type": "string", "description": "特征 UUID" },
                    "data_hex": { "type": "string", "description": "十六进制数据，如 0102FF" }
                },
                "required": ["char_uuid", "data_hex"]
            }
        },
        {
            "name": "subscribe",
            "description": "订阅/取消订阅指定 UUID 特征的通知（收到的数据会显示在 GUI 日志中）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "char_uuid": { "type": "string", "description": "特征 UUID" },
                    "on": { "type": "boolean", "description": "true 订阅，false 取消" }
                },
                "required": ["char_uuid", "on"]
            }
        },
        {
            "name": "status",
            "description": "查询当前连接状态、已发现设备与收到数据的概览。",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        }
    ])
}

async fn dispatch_tool(
    ble: &Arc<tokio::sync::Mutex<BleCentral>>,
    state: &Arc<RwLock<AppState>>,
    name: &str,
    args: &Value,
) -> Result<String> {
    let mut ble = ble.lock().await;
    match name {
        "scan" => {
            let secs = args
                .get("seconds")
                .and_then(Value::as_u64)
                .unwrap_or(5)
                .clamp(1, 60);
            let devs = ble.scan(secs).await?;
            Ok(serde_json::to_string_pretty(&devs)?)
        }
        "connect" => {
            let addr = args
                .get("address")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("缺少 address 参数"))?
                .to_string();
            ble.connect(&addr).await?;
            Ok(format!("已连接并发现服务: {addr}"))
        }
        "disconnect" => {
            ble.disconnect().await?;
            Ok("已断开".to_string())
        }
        "services" => {
            let st = state.read().unwrap();
            let svcs = st.services.clone();
            Ok(serde_json::to_string_pretty(&svcs)?)
        }
        "read" => {
            let uuid = args
                .get("char_uuid")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("缺少 char_uuid 参数"))?
                .to_string();
            let val = ble.read(&uuid).await?;
            let hex = val
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ");
            Ok(format!(
                "{}",
                json!({ "uuid": uuid, "len": val.len(), "hex": hex })
            ))
        }
        "write" => {
            let uuid = args
                .get("char_uuid")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("缺少 char_uuid 参数"))?;
            let hex = args
                .get("data_hex")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("缺少 data_hex 参数"))?;
            let data = hex_to_bytes(hex).map_err(|e| anyhow!("data_hex 格式错误: {e}"))?;
            ble.write(uuid, &data).await?;
            Ok(format!("已写入 {} 字节", data.len()))
        }
        "subscribe" => {
            let uuid = args
                .get("char_uuid")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("缺少 char_uuid 参数"))?;
            let on = args.get("on").and_then(Value::as_bool).unwrap_or(true);
            ble.subscribe(uuid, on).await?;
            Ok(if on {
                "已订阅通知".to_string()
            } else {
                "已取消订阅".to_string()
            })
        }
        "status" => {
            let st = state.read().unwrap();
            let summary = json!({
                "connected": st.connected_address,
                "scanning": st.scanning,
                "device_count": st.devices.len(),
                "data_log_entries": st.data_log.len(),
            });
            Ok(serde_json::to_string_pretty(&summary)?)
        }
        _ => Err(anyhow!("未知工具: {name}")),
    }
}

fn hex_to_bytes(s: &str) -> Result<Vec<u8>> {
    crate::ble::parse_hex(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_hex_rejects_unicode_and_invalid_input() {
        for input in ["中0", "é", "💡", "0", "ZZ"] {
            assert!(hex_to_bytes(input).is_err(), "input: {input:?}");
        }
        assert_eq!(hex_to_bytes("01 ff\n02").unwrap(), vec![1, 255, 2]);
    }

    #[test]
    fn tool_catalog_has_unique_names_and_valid_required_fields() {
        let tools = tools_list();
        let tools = tools.as_array().unwrap();
        assert_eq!(tools.len(), 8);
        let mut names = std::collections::HashSet::new();
        for tool in tools {
            assert!(names.insert(tool["name"].as_str().unwrap()));
            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], "object");
            for field in schema["required"].as_array().unwrap() {
                assert!(schema["properties"].get(field.as_str().unwrap()).is_some());
            }
        }
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    async fn request(msg: Value) -> Option<Value> {
        let state = Arc::new(RwLock::new(AppState {
            data_log_limit: 10,
            ..Default::default()
        }));
        let ble = Arc::new(tokio::sync::Mutex::new(
            BleCentral::simulated(state.clone()).await,
        ));
        handle_message(&ble, &state, msg).await
    }
    #[tokio::test]
    async fn initialization_catalog_ping_and_unknown_method() {
        for method in ["initialize", "tools/list", "ping"] {
            let r = request(json!({"jsonrpc":"2.0","id":"abc","method":method}))
                .await
                .unwrap();
            assert_eq!(r["id"], "abc");
            assert_eq!(r["jsonrpc"], "2.0");
            assert!(r.get("result").is_some());
        }
        assert_eq!(
            request(json!({"id":1,"method":"missing"})).await.unwrap()["error"]["code"],
            -32601
        );
        assert!(request(json!({"method":"notifications/initialized"}))
            .await
            .is_none());
        assert!(request(json!({"method":"unknown"})).await.is_none());
    }
    #[tokio::test]
    async fn every_tool_validates_or_reports_disconnected_errors() {
        for (name, args, error) in [
            ("connect", json!({}), true),
            ("read", json!({}), true),
            ("write", json!({}), true),
            ("subscribe", json!({}), true),
            ("missing", json!({}), true),
            ("write", json!({"char_uuid":"bad","data_hex":"中0"}), true),
            ("read", json!({"char_uuid":"bad"}), true),
            ("status", json!({}), false),
            ("services", json!({}), false),
            ("disconnect", json!({}), false),
            ("connect", json!({"address":"AA:BB:CC:DD:EE:FF"}), false),
        ] {
            let r=request(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":args}})).await.unwrap();
            assert_eq!(r["result"]["isError"], error, "{name}: {r}");
        }
    }
}
