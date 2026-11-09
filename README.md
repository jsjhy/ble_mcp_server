# BLE Server (Central) — Rust + MCP + egui GUI

Windows 11 上的 BLE 中央设备（Central）工具：扫描、配对、连接外部 BLE 外设，
读写 GATT 服务、订阅通知；通过 **MCP（HTTP + SSE）协议**暴露给 AI 客户端；
并提供 **egui（eframe）GUI** 实时显示连接状态、设备列表与收到的数据流。

> GUI 曾用 `windows` crate 手写原生 Win32 控件，后改为 eframe/egui：
> 布局、中文字体、实时刷新都更顺手，`ble.rs` / `mcp.rs` 逻辑完全复用。

## 架构

```
┌───────────────────────────── main 进程 ─────────────────────────────┐
│                                                                     │
│   egui GUI（主线程，eframe）          tokio runtime（2 worker）       │
│   ├─ 顶部：菜单 + 状态 + MCP 地址（可复制）├─ BleCentral（btleplug）  │
│   ├─ 左：设备列表（可选中）+ GATT 树      │   ├─ 扫描/连接/读写/订阅 │
│   └─ 右：收到的数据流（自动滚到底）       │   └─ 通知监听任务         │
│         ▲                                 │         ▲                │
│         │ 每 250ms 重绘时读取             │         │ 写入          │
│         └──────── AppState(RwLock) ◄──────┴─────────┘                │
│                        ▲                                            │
│                        │ 读写                                        │
│               MCP HTTP+SSE server（后台任务，默认 127.0.0.1:8765）    │
│               /sse  /message  /rpc  /health                          │
│               scan/connect/disconnect/services/read/write/subscribe/status
└─────────────────────────────────────────────────────────────────────┘
```

## 构建

```powershell
# Debug
cargo build
# Release（推荐）
cargo build --release
# 产物
.\target\release\ble_server.exe
```

## 运行

```powershell
# 默认：GUI + MCP HTTP+SSE 服务（监听 127.0.0.1:8765）
.\target\release\ble_server.exe

# 只跑 MCP 服务、不弹窗（后台服务 / 服务器上用）
.\target\release\ble_server.exe --no-gui

# 换端口
.\target\release\ble_server.exe --port 9000

# 允许局域网内其它机器访问（默认只监听本机）
.\target\release\ble_server.exe --bind 0.0.0.0 --port 8765
```

MCP 只有 HTTP + SSE 一种传输（不再提供 stdio）。
界面顶部会显示当前 MCP 地址，点旁边的「复制」即可复制 `http://<host>:<port>/sse`。

## GUI 操作

顶部菜单栏 **BLE**：

- **扫描设备（5秒）**：扫描周围 BLE 设备，结果显示在左侧设备列表
- **连接选中设备**：先在左侧列表点选一行，再点此菜单即可连接并自动配对、发现 GATT 服务
- **断开连接**：断开当前 BLE 连接
- **写入数据（读取下方输入框）**：把底部「写入特征 + 数据(hex)」的内容写入已连接设备

界面区域：

- **顶部状态**：`● 已连接 <地址>`（绿）/ `○ 未连接`（灰）、MCP 服务地址 `http://<host>:<port>/sse`（等宽蓝色，可复制）、扫描中转圈、设备数/服务数/数据条数、最近一次操作的提示；
  操作失败（扫描/连接/写入/断开）时最右侧用红字显示具体原因，下一次操作成功后自动消失
- **左侧设备列表**：地址、名称、RSSI，点一行即选中（选中行高亮）；连接成功后该行前加 `●`；
  连接被设备**拒绝**过的地址前会标一个灰色 `×`（**仍然照常显示，不会漏设备**），
  标题旁有「隐藏拒连（N）」复选框（默认不勾选，勾上才过滤）与「清除标记」按钮。
  拒连记录保存在 exe 同目录的 `blocked_devices.txt`（一行一个地址），连上过一次会自动撤掉标记。
- **左侧 GATT 服务树**：连接后**自动出现并展开**（每次连接都是全新展开状态），列出每个服务下的特征与能力标记 `[R]` 读 / `[W]` 写 / `[N]` 通知；当前写入目标特征会用蓝色高亮
- **右侧「收到的数据」**：通知数据流，十六进制，自动滚到底部，可「清空视图」
- **右侧下半「发送的数据」**：发送历史，GUI 点「写入」或 MCP 调 `write` 都会记一条，
  格式 `[时:分:秒] ✓ 特征UUID (N字节) 十六进制`；失败记录为红色并附具体原因（UUID 不存在、设备拒绝写入等）。
  同样自动滚到底部、可单独「清空视图」；最多保留最近 200 条
- **底部写入区**：`写入特征` 下拉框（自动列出当前设备所有可写特征）、`数据(hex)`、`写入 / 扫描 / 连接 / 断开` 按钮

### 写入数据用法

1. 先扫描并连接目标设备
2. 连接后「写入特征」下拉框会自动选中第一个可写特征（透传模块通常是 `0000ffe1`），也可以点开下拉换一个
3. 需要写一个列表里没有的 UUID 时，下拉里选「手动输入 UUID…」，会多出一行输入框
4. 在「数据(hex)」填十六进制字节（空格可选，如 `41 54` 或 `4154`），点「写入」
5. 顶部提示与运行日志会显示 `GUI 触发写入` / `写入成功 (N)`；失败会给出原因

未连接设备时没有可写特征，「写入」按钮是禁用的。

写入失败时程序会自动重连并改用 no-response 写方式重试一次，兼容只支持 write-without-response 的透传模块。

### 中文字体

egui 内置字体不含 CJK，程序启动时会依次尝试加载
`C:\Windows\Fonts\msyh.ttc`（微软雅黑）→ `simhei.ttf` → `msyh.ttf` → `simsun.ttc` → `Deng.ttf`，
加载成功会在日志打印 `已加载中文字体: <路径>`。若全部缺失，界面中文会显示为方块。

## MCP 协议

实现 MCP 2024-11-05 规范，JSON-RPC 2.0，传输层为 **HTTP + SSE**。

### 端点

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/sse` | 建立 SSE 会话；首个事件 `event: endpoint`，data 为后续 POST 的地址 |
| POST | `/message?sessionId=xxx` | 发 JSON-RPC 消息；立即返回 `202 Accepted`，结果从 SSE 以 `event: message` 推回 |
| POST | `/rpc` | 非规范的便捷接口：一次请求直接返回 JSON-RPC 响应（方便脚本调试） |
| GET | `/health` | 健康检查：`{"ok":true,"sessions":N,"connected":"地址或null"}` |
| GET | `/` | 端点清单（JSON） |

### 客户端流程

1. `GET /sse`，读到 `event: endpoint` 与 `data: /message?sessionId=<id>`
2. `POST /message?sessionId=<id>` 发 `initialize`，从 SSE 流收到 `event: message` 的响应
3. 同一个 endpoint 继续发 `notifications/initialized`、`tools/list`、`tools/call` …
4. 服务端会周期性发 SSE 心跳（keep-alive 注释行），防止连接被中间设备掐断

### 工具列表（tools/list）

| 工具 | 说明 | 参数 |
|------|------|------|
| `scan` | 扫描周围 BLE 设备 | `seconds`(1-60，默认5) |
| `connect` | 连接并自动配对、发现 GATT | `address`(必填) |
| `disconnect` | 断开当前连接 | — |
| `services` | 列出 GATT 服务/特征及能力 | — |
| `read` | 读取特征值(hex) | `char_uuid`(必填) |
| `write` | 写入特征(hex)；失败自动重连并按 no-response 重试 | `char_uuid`,`data_hex`(必填) |
| `subscribe` | 订阅/取消通知 | `char_uuid`(必填),`on`(bool) |
| `status` | 连接状态概览 | — |

### 调用示例

同步调试接口（`/rpc`，直接拿结果，最省事）：

```powershell
$body = '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"scan","arguments":{"seconds":5}}}'
Invoke-RestMethod -Uri http://127.0.0.1:8765/rpc -Method Post -Body $body -ContentType "application/json"

$body = '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"connect","arguments":{"address":"11:22:33:44:55:66"}}}'
Invoke-RestMethod -Uri http://127.0.0.1:8765/rpc -Method Post -Body $body -ContentType "application/json"

$body = '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"write","arguments":{"char_uuid":"0000ffe1-0000-1000-8000-00805f9b34fb","data_hex":"0102FF"}}}'
Invoke-RestMethod -Uri http://127.0.0.1:8765/rpc -Method Post -Body $body -ContentType "application/json"
```

规范 SSE 流程（先连 SSE 再 POST）：

```powershell
curl.exe -N http://127.0.0.1:8765/sse
# 另开一个窗口，用上面 endpoint 事件给出的 sessionId 发消息：
curl.exe -X POST "http://127.0.0.1:8765/message?sessionId=<id>" -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
```

## 作为 MCP 客户端接入

本服务是 **SSE 类型**（不是 stdio）。在客户端的 MCP 配置里填 URL：

```json
{
  "mcpServers": {
    "ble_server": {
      "url": "http://127.0.0.1:8765/sse",
      "transport": "sse"
    }
  }
}
```

不同客户端的字段名可能不同（有的用 `url`，有的要 `type: "sse"`），核心是把 `/sse` 这个地址填进去。
若客户端只支持 stdio，可以自己用 `/rpc` 包一层 stdio 适配，或换一个支持 SSE 的客户端。

配置后，AI 客户端即可调用 `scan / connect / read / write / subscribe` 等工具，
实现对 BLE 设备的配对连接与通讯。

## 源码结构

```
src/
├── main.rs     # 入口：命令行参数、组装 AppState + tokio(runtime) + GUI(主线程)
├── ble.rs      # BLE Central：btleplug 封装（扫描/连接/读写/订阅/自动重连）
├── mcp.rs      # MCP 协议层：JSON-RPC 分发 + 8 个工具（与传输无关）
├── mcp_http.rs # MCP HTTP+SSE 传输层（/sse、/message、/rpc、/health）
├── gui.rs      # egui（eframe/glow）界面：菜单、设备表、GATT 树、数据流、写入区
└── state.rs    # 共享状态 AppState（RwLock）+ GuiCmd 命令枚举
```

## 依赖说明

- `btleplug 0.11`：Windows 自动用 WinRT 后端
- `axum 0.7`：MCP 的 HTTP + SSE 传输
- `eframe 0.31`（`glow` 后端 + `default_fonts`）：**必须用 0.31**。
  0.32+ 的 egui API 大改（`App::ui` 取代 `App::update`、面板 API 变更），
  0.37 默认走 `wgpu` 后端，在本机会因 `wgpu-hal` 与 windows crate 版本冲突编译失败。
- `glow` 用 OpenGL 渲染，免去 wgpu/D3D12 的依赖链。
