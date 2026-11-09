#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! BLE Server：中央设备（Central）模式。
//! - BLE：扫描 / 配对 / 连接 / GATT 读写 / 通知订阅（btleplug + WinRT）
//! - MCP：HTTP + SSE 传输（MCP 2024-11-05），暴露 BLE 操作给 AI 客户端
//! - GUI：egui 窗口，显示连接状态与收到的数据
//!
//! 架构：一个共享 AppState（RwLock）承载连接状态/设备列表/数据日志，
//!       tokio runtime 运行 BLE 层与 MCP HTTP 服务，
//!       主线程运行 egui 事件循环（每 250ms 重绘）。
//!
//! 命令行：
//!   ble_server.exe                  GUI + MCP HTTP（默认 127.0.0.1:8765）
//!   ble_server.exe --no-gui         仅 MCP HTTP（无窗口，适合做后台服务）
//!   --port 8765 / --bind 0.0.0.0    指定端口与监听地址（默认只监听本机）

mod ble;
mod gui;
mod logging;
mod mcp;
mod mcp_http;
mod state;

use std::sync::Arc;

use anyhow::Result;

fn main() -> Result<()> {
    logging::init()?;
    let result = run();
    if let Err(error) = &result {
        log::error!("程序退出: {error:#}");
    } else {
        log::info!("程序正常退出");
    }
    result
}

fn run() -> Result<()> {
    // ---- 命令行参数 ----
    let args: Vec<String> = std::env::args().collect();
    let no_gui = args.iter().any(|a| a == "--no-gui");
    let mut bind = "127.0.0.1".to_string();
    let mut port: u16 = 8765;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--port" && i + 1 < args.len() {
            port = args[i + 1].parse().unwrap_or(8765);
            i += 2;
        } else if a == "--bind" && i + 1 < args.len() {
            bind = args[i + 1].clone();
            i += 2;
        } else if let Some(v) = a.strip_prefix("--port=") {
            port = v.parse().unwrap_or(8765);
            i += 1;
        } else if let Some(v) = a.strip_prefix("--bind=") {
            bind = v.to_string();
            i += 1;
        } else {
            i += 1;
        }
    }

    let state = state::AppState::new();

    // GUI -> BLE 命令通道
    let (gui_tx, mut gui_rx) = tokio::sync::mpsc::unbounded_channel::<state::GuiCmd>();

    // tokio runtime：跑 BLE、MCP HTTP 服务与 GUI 命令控制
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    // BLE 中央管理器（在 runtime 内创建，因为 btleplug 是异步的）
    let ble = rt.block_on(async {
        let b = ble::BleCentral::new(state.clone()).await?;
        log::info!("BLE Central 就绪: {}", b.status());
        Ok::<_, anyhow::Error>(Arc::new(tokio::sync::Mutex::new(b)))
    })?;

    // 纯服务模式：主线程直接驱动 MCP HTTP 服务
    if no_gui {
        log::info!("无界面模式：仅运行 MCP HTTP+SSE 服务");
        let bind = bind.clone();
        rt.block_on(mcp_http::serve(ble, state, &bind, port))?;
        return Ok(());
    }

    // 后台任务：GUI 菜单命令 -> BLE 操作
    {
        let ble = ble.clone();
        let state = state.clone();
        rt.spawn(async move {
            // 把失败原因写到共享状态，GUI 顶部用红字显示；成功则清空
            let set_err = |msg: Option<String>| {
                if let Ok(mut st) = state.write() {
                    st.last_error = msg;
                }
            };
            while let Some(cmd) = gui_rx.recv().await {
                use state::GuiCmd;
                let mut b = ble.lock().await;
                match cmd {
                    GuiCmd::Scan { seconds } => {
                        log::info!("GUI 触发扫描 {seconds}s");
                        match b.scan(seconds).await {
                            Ok(devs) => {
                                log::info!("扫描完成，发现 {} 个设备", devs.len());
                                set_err(None);
                            }
                            Err(e) => {
                                log::error!("扫描失败: {e}");
                                set_err(Some(format!("扫描失败: {e}")));
                            }
                        }
                    }
                    GuiCmd::Connect { address } => {
                        log::info!("GUI 触发连接 {address}");
                        match b.connect(&address).await {
                            Ok(_) => {
                                log::info!("连接成功");
                                set_err(None);
                            }
                            Err(e) => {
                                log::error!("连接失败: {e}");
                                set_err(Some(format!("连接失败: {e}")));
                            }
                        }
                    }
                    GuiCmd::Disconnect => {
                        log::info!("GUI 触发断开");
                        if let Err(e) = b.disconnect().await {
                            log::error!("断开失败: {e}");
                            set_err(Some(format!("断开失败: {e}")));
                        } else {
                            set_err(None);
                        }
                    }
                    GuiCmd::Write {
                        char_uuid,
                        data_hex,
                    } => {
                        log::info!("GUI 触发写入 {char_uuid} data={data_hex}");
                        match crate::ble::parse_hex(&data_hex) {
                            Ok(bytes) => match b.write(&char_uuid, &bytes).await {
                                Ok(_) => {
                                    log::info!("写入成功 ({}) -> {char_uuid}", bytes.len());
                                    set_err(None);
                                }
                                Err(e) => {
                                    log::error!("写入失败: {e}");
                                    set_err(Some(format!("写入失败: {e}")));
                                }
                            },
                            Err(e) => {
                                log::error!("数据格式错误: {e}");
                                set_err(Some(format!("数据格式错误: {e}")));
                            }
                        }
                    }
                }
            }
        });
    }

    // 后台任务：MCP HTTP + SSE 服务
    {
        let ble = ble.clone();
        let state = state.clone();
        let bind = bind.clone();
        rt.spawn(async move {
            if let Err(e) = mcp_http::serve(ble, state, &bind, port).await {
                log::error!("MCP HTTP 服务退出: {e}");
            }
        });
    }

    // 主线程：egui GUI（把 MCP 监听地址显示在界面上）
    let mcp_addr = format!("{bind}:{port}");
    log::info!("启动 GUI...");
    gui::run_gui(state, gui_tx, mcp_addr)?;

    // GUI 关闭后停止后台任务
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
    Ok(())
}
