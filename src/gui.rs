//! GUI 层：基于 eframe / egui 的即时渲染界面。
//! 替代原先手搓的 Win32 控件——同样显示连接状态、设备列表、收到的数据流，
//! 并保留「BLE 菜单」（扫描 / 连接 / 断开 / 写入数据）。
//!
//! 与后台的分工保持不变：
//! - 读：每帧从共享 `AppState` 快照（RwLock）
//! - 写：通过 `GuiCmd` 通道把操作下发给 BLE 控制任务

use std::sync::{Arc, RwLock};

use anyhow::Result;
use eframe::egui;
use tokio::sync::mpsc::UnboundedSender;

use crate::state::{AppState, GuiCmd};

/// GUI 启动入口（阻塞直到窗口关闭）。
/// `mcp_addr` 为 MCP HTTP 服务监听地址（如 `127.0.0.1:8765`），显示在界面上供客户端接入。
pub fn run_gui(
    state: Arc<RwLock<AppState>>,
    gui_tx: UnboundedSender<GuiCmd>,
    mcp_addr: String,
) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 760.0])
            .with_min_inner_size([760.0, 520.0])
            .with_title("BLE Server - Central")
            .with_icon(egui::IconData {
                rgba: include_bytes!("../app-icon.rgba").to_vec(),
                width: 256,
                height: 256,
            }),
        ..Default::default()
    };

    eframe::run_native(
        "BLE Server - Central",
        options,
        Box::new(move |cc| {
            load_cjk_font(&cc.egui_ctx);
            Ok(Box::new(BleApp::new(state, gui_tx, mcp_addr)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("GUI 运行失败: {e}"))
}

/// 加载系统中文字体。egui 内置字体不含 CJK，不加载会全是方块。
fn load_cjk_font(ctx: &egui::Context) {
    // 从前到后尝试：微软雅黑(ttc) / 黑体(ttf) / 宋体(ttc)
    const CANDIDATES: [&str; 5] = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\simsun.ttc",
        r"C:\Windows\Fonts\Deng.ttf",
    ];

    let mut fonts = egui::FontDefinitions::default();
    let mut loaded = None;
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("cjk".to_owned(), egui::FontData::from_owned(bytes).into());
            if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                f.insert(0, "cjk".to_owned());
            }
            if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                f.push("cjk".to_owned());
            }
            loaded = Some(path);
            break;
        }
    }

    if let Some(p) = loaded {
        ctx.set_fonts(fonts);
        log::info!("已加载中文字体: {p}");
    } else {
        log::warn!("未找到可用的中文字体，界面中文可能显示为方块");
    }
}

struct BleApp {
    state: Arc<RwLock<AppState>>,
    gui_tx: UnboundedSender<GuiCmd>,
    /// MCP HTTP 服务监听地址（显示在顶部，供客户端接入）
    mcp_addr: String,
    /// 设备列表中当前选中的地址
    selected: Option<String>,
    /// 写入区：特征 UUID 与十六进制数据
    write_uuid: String,
    write_data: String,
    /// 最近一次操作提示（就地反馈，避免用户以为没响应）
    hint: String,
    /// 连接状态变化计数：每次（重新）连接设备 +1。
    /// 用作 GATT 折叠区的 id，保证新连接时折叠区以「全新」状态出现并自动展开。
    conn_epoch: u64,
    /// 上一帧看到的连接地址，用于检测连接状态变化
    last_connected: Option<String>,
    /// 写入区是否使用「手动输入 UUID」模式（否则从可写特征下拉里选）
    manual_uuid: bool,
    /// 新连接后自动选中第一个可写特征（服务发现完成后才生效）
    auto_pick: bool,
    /// 是否隐藏「拒连」设备。默认**不隐藏**——扫描结果要全量显示，不能漏设备；
    /// 勾上才过滤。
    hide_rejected: bool,
}

impl BleApp {
    fn new(
        state: Arc<RwLock<AppState>>,
        gui_tx: UnboundedSender<GuiCmd>,
        mcp_addr: String,
    ) -> Self {
        Self {
            state,
            gui_tx,
            mcp_addr,
            selected: None,
            write_uuid: "0000ffe1-0000-1000-8000-00805f9b34fb".to_owned(),
            write_data: "41 54".to_owned(),
            hint: String::new(),
            conn_epoch: 0,
            last_connected: None,
            manual_uuid: false,
            auto_pick: false,
            hide_rejected: false,
        }
    }

    fn send(&mut self, cmd: GuiCmd) {
        if self.gui_tx.send(cmd).is_err() {
            self.hint = "后台 BLE 任务已停止，命令未送达".to_owned();
        }
    }

    fn scan(&mut self) {
        self.hint = "已下发扫描（5 秒），请稍候…".to_owned();
        self.send(GuiCmd::Scan { seconds: 5 });
    }

    fn connect_selected(&mut self) {
        match self.selected.clone() {
            Some(address) => {
                self.hint = format!("正在连接 {address} …");
                self.send(GuiCmd::Connect { address });
            }
            None => self.hint = "请先在左侧列表中选中一个设备".to_owned(),
        }
    }

    fn disconnect(&mut self) {
        self.hint = "已下发断开".to_owned();
        self.send(GuiCmd::Disconnect);
    }

    fn write(&mut self) {
        let uuid = self.write_uuid.trim().to_owned();
        let data = self.write_data.trim().to_owned();
        if uuid.is_empty() || data.is_empty() {
            self.hint = "特征UUID 与 数据 都不能为空".to_owned();
            return;
        }
        self.hint = format!("已下发写入 {data} -> {uuid}");
        self.send(GuiCmd::Write {
            char_uuid: uuid,
            data_hex: data,
        });
    }
}

impl eframe::App for BleApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 数据由后台异步写入，定时重绘以保证界面跟得上
        ctx.request_repaint_after(std::time::Duration::from_millis(250));

        // ---- 状态快照（短暂持锁，避免阻塞后台写入）----
        let (scanning, devices, connected, services, log, tx_log, blocked, last_error) = {
            match self.state.read() {
                Ok(st) => (
                    st.scanning,
                    st.devices.clone(),
                    st.connected_address.clone(),
                    st.services.clone(),
                    st.data_log.iter().cloned().collect::<Vec<_>>(),
                    st.tx_log.iter().cloned().collect::<Vec<_>>(),
                    st.blocked.clone(),
                    st.last_error.clone(),
                ),
                Err(_) => (
                    false,
                    Vec::new(),
                    None,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Default::default(),
                    None,
                ),
            }
        };

        // ---- 连接状态变化：新连接时重置 GATT 折叠与特征选择 ----
        if self.last_connected != connected {
            let newly_connected = connected.is_some();
            self.last_connected = connected.clone();
            self.conn_epoch += 1;
            if newly_connected {
                self.auto_pick = true; // 服务发现完成后自动选中第一个可写特征
                self.manual_uuid = false;
            }
        }

        // 当前设备上所有可写特征（写入下拉框的候选）
        let writable_chars: Vec<String> = {
            let mut v: Vec<String> = services
                .iter()
                .flat_map(|s| s.characteristics.iter())
                .filter(|c| c.writable)
                .map(|c| c.uuid.clone())
                .collect();
            v.dedup();
            v
        };

        // 新连接后自动选中第一个可写特征（通常是 0000ffe1 透传通道）
        if self.auto_pick {
            if let Some(first) = writable_chars.first() {
                self.write_uuid = first.clone();
                self.auto_pick = false;
            }
        }
        // 已选中但不属于本设备可写特征（例如换了设备），回退到第一个
        if !self.manual_uuid
            && !writable_chars.is_empty()
            && !writable_chars.iter().any(|u| u == &self.write_uuid)
        {
            self.write_uuid = writable_chars[0].clone();
        }

        let sidebar_width = (ctx.screen_rect().width() * 0.32).clamp(250.0, 380.0);

        // ---- 顶部：菜单栏 + 状态 ----
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.menu_button("BLE", |ui| {
                    if ui.button("扫描设备（5秒）").clicked() {
                        self.scan();
                        ui.close_menu();
                    }
                    if ui.button("连接选中设备").clicked() {
                        self.connect_selected();
                        ui.close_menu();
                    }
                    if ui.button("断开连接").clicked() {
                        self.disconnect();
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("写入数据（读取下方输入框）").clicked() {
                        self.write();
                        ui.close_menu();
                    }
                });
                ui.separator();

                // 连接状态徽标
                if let Some(addr) = &connected {
                    ui.colored_label(
                        egui::Color32::from_rgb(70, 200, 120),
                        egui::RichText::new(format!("● 已连接 {addr}")).strong(),
                    );
                } else {
                    ui.colored_label(
                        egui::Color32::GRAY,
                        egui::RichText::new("○ 未连接").strong(),
                    );
                }

                if ui
                    .add_enabled(!scanning, egui::Button::new("扫描设备"))
                    .clicked()
                {
                    self.scan();
                }
                if ui
                    .add_enabled(self.selected.is_some(), egui::Button::new("连接"))
                    .clicked()
                {
                    self.connect_selected();
                }
                if ui
                    .add_enabled(connected.is_some(), egui::Button::new("断开"))
                    .clicked()
                {
                    self.disconnect();
                }

                if scanning {
                    ui.spinner();
                    ui.label("扫描中…");
                }

                ui.separator();
                let visible_devices = if self.hide_rejected {
                    devices
                        .iter()
                        .filter(|d| !blocked.contains(&d.address))
                        .count()
                } else {
                    devices.len()
                };
                ui.label(format!("设备 {visible_devices}"));
                ui.label(format!("服务 {}", services.len()));
                ui.label(format!("数据 {}", log.len()));
            });
            ui.horizontal_wrapped(|ui| {
                // MCP 服务地址（HTTP + SSE），供客户端接入
                ui.separator();
                ui.label(egui::RichText::new("MCP").small().weak());
                let mcp_url = format!("http://{}/sse", self.mcp_addr);
                ui.label(
                    egui::RichText::new(&mcp_url)
                        .monospace()
                        .small()
                        .color(egui::Color32::from_rgb(120, 180, 255)),
                );
                if ui
                    .small_button("复制")
                    .on_hover_text("复制 SSE 地址")
                    .clicked()
                {
                    ui.ctx().copy_text(mcp_url.clone());
                    self.hint = "已复制 MCP 地址".to_owned();
                }
            });
            if let Some(err) = &last_error {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("⚠ {err}"))
                            .color(egui::Color32::from_rgb(235, 120, 100)),
                    )
                    .wrap(),
                );
            } else if !self.hint.is_empty() {
                ui.add(egui::Label::new(egui::RichText::new(&self.hint).weak().small()).wrap());
            }
        });

        // ---- 底部：写入区 + 快捷操作 ----
        egui::TopBottomPanel::bottom("write_bar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.strong("发送数据");
                ui.label("特征");
                let combo_text = if self.manual_uuid {
                    "手动输入…".to_owned()
                } else if self.write_uuid.is_empty() {
                    if writable_chars.is_empty() {
                        "（无可写特征，请先连接设备）".to_owned()
                    } else {
                        "（请选择）".to_owned()
                    }
                } else {
                    self.write_uuid.clone()
                };
                egui::ComboBox::from_id_salt("write_char_combo")
                    .width((ui.available_width() - 80.0).clamp(220.0, 400.0))
                    .selected_text(combo_text)
                    .show_ui(ui, |ui| {
                        if writable_chars.is_empty() {
                            ui.label(egui::RichText::new("未发现可写特征").weak().small());
                        }
                        for u in &writable_chars {
                            let sel = !self.manual_uuid && &self.write_uuid == u;
                            if ui.selectable_label(sel, u).clicked() {
                                self.write_uuid = u.clone();
                                self.manual_uuid = false;
                                ui.close_menu();
                            }
                        }
                        ui.separator();
                        if ui
                            .selectable_label(self.manual_uuid, "手动输入 UUID…")
                            .clicked()
                        {
                            self.manual_uuid = true;
                            ui.close_menu();
                        }
                    });
            });
            ui.horizontal(|ui| {
                ui.label("数据(hex)");
                ui.add(
                    egui::TextEdit::singleline(&mut self.write_data)
                        .desired_width((ui.available_width() - 145.0).max(100.0))
                        .hint_text("41 54"),
                );
                let can_write = connected.is_some()
                    && !self.write_uuid.trim().is_empty()
                    && !self.write_data.trim().is_empty()
                    && (self.manual_uuid || !writable_chars.is_empty());
                if ui
                    .add_enabled(can_write, egui::Button::new("写入"))
                    .clicked()
                {
                    self.write();
                }
            });

            // 手动输入模式：额外给一行 UUID 输入框
            if self.manual_uuid {
                ui.horizontal_wrapped(|ui| {
                    ui.label("自定义 UUID");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.write_uuid)
                            .desired_width(330.0)
                            .hint_text("0000ffe1-0000-1000-8000-00805f9b34fb"),
                    );
                    ui.label(
                        egui::RichText::new("（下拉框里可切回选择模式）")
                            .small()
                            .weak(),
                    );
                });
            }

            ui.add_space(4.0);
        });

        // ---- 左侧：设备列表 ----
        egui::SidePanel::left("devices_panel")
            .resizable(true)
            .default_width(sidebar_width)
            .width_range(240.0..=(ctx.screen_rect().width() * 0.48).max(240.0))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                let n_rejected = devices
                    .iter()
                    .filter(|d| blocked.contains(&d.address))
                    .count();
                ui.horizontal_wrapped(|ui| {
                    ui.heading("设备");
                    if n_rejected > 0 {
                        ui.add_space(6.0);
                        ui.checkbox(&mut self.hide_rejected, format!("隐藏拒连（{n_rejected}）"))
                            .on_hover_text(
                                "默认全部显示（不漏设备）。勾选后才隐藏连接被拒的设备。",
                            );
                        if ui
                            .small_button("清除标记")
                            .on_hover_text("清空拒连记录；设备再次连接失败会重新标记")
                            .clicked()
                        {
                            if let Ok(mut st) = self.state.write() {
                                st.blocked.clear();
                                st.persist_blocked();
                            }
                            self.hint = "已清除拒连记录".to_owned();
                        }
                    }
                });
                ui.label(
                    egui::RichText::new("点一行选中，再点「连接」")
                        .small()
                        .weak(),
                );
                ui.separator();

                // 连接后给下方 GATT 树预留空间，否则列表会把它挤出可视区
                let reserve = if services.is_empty() {
                    0.0
                } else {
                    ui.available_height() * 0.45
                };
                let list_h = (ui.available_height() - reserve - 12.0).max(60.0);

                egui::ScrollArea::vertical()
                    .id_salt("devices_scroll")
                    .max_height(list_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if devices.is_empty() {
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new(
                                    "（暂无设备）点上方「扫描」或菜单 BLE→扫描设备（5秒）",
                                )
                                .weak(),
                            );
                            return;
                        }

                        let row_width = ui.available_width();
                        for d in &devices {
                            if self.hide_rejected && blocked.contains(&d.address) {
                                continue;
                            }
                            let selected = self.selected.as_deref() == Some(d.address.as_str());
                            let active = connected.as_deref() == Some(d.address.as_str());
                            let rejected = blocked.contains(&d.address);
                            let name = d.name.as_deref().unwrap_or("未命名设备");
                            let marker = if active {
                                "● "
                            } else if rejected {
                                "× "
                            } else {
                                ""
                            };
                            let rssi = d
                                .rssi
                                .map(|r| format!("{r} dBm"))
                                .unwrap_or_else(|| "未知信号".into());
                            let text = format!("{marker}{name}  ·  {rssi}\n{}", d.address);
                            let response = ui.add_sized(
                                [row_width, 48.0],
                                egui::Button::new(text).selected(selected).wrap(),
                            );
                            if response.clicked() {
                                self.selected = Some(d.address.clone());
                            }
                            response.on_hover_text(if rejected {
                                "曾连接失败，可再次尝试"
                            } else {
                                "选择设备后点击顶部连接"
                            });
                        }
                    });

                ui.separator();
                if !services.is_empty() {
                    // 每次连接用新的 id：连接后必定以展开状态出现（不受上次手动折叠影响）
                    let epoch = self.conn_epoch;
                    egui::CollapsingHeader::new(format!("GATT 服务（{}）", services.len()))
                        .id_salt(format!("gatt_root_{epoch}"))
                        .default_open(true)
                        .show(ui, |ui| {
                            egui::ScrollArea::both()
                                .id_salt(format!("services_scroll_{epoch}"))
                                .max_height((ui.available_height() - 8.0).max(30.0))
                                .show(ui, |ui| {
                                    for s in &services {
                                        egui::CollapsingHeader::new(
                                            egui::RichText::new(&s.uuid).monospace().strong(),
                                        )
                                        .id_salt(format!("svc_{epoch}_{}", s.uuid))
                                        .default_open(true)
                                        .show(ui, |ui| {
                                            if s.characteristics.is_empty() {
                                                ui.label(
                                                    egui::RichText::new("（无特征）")
                                                        .small()
                                                        .weak(),
                                                );
                                            }
                                            for c in &s.characteristics {
                                                let mut flags = Vec::new();
                                                if c.readable {
                                                    flags.push("R");
                                                }
                                                if c.writable {
                                                    flags.push("W");
                                                }
                                                if c.notifiable {
                                                    flags.push("N");
                                                }
                                                let label =
                                                    format!("{} [{}]", c.uuid, flags.join(""));
                                                let text =
                                                    egui::RichText::new(label).monospace().small();
                                                if &self.write_uuid == &c.uuid && !self.manual_uuid
                                                {
                                                    ui.label(text.strong().color(
                                                        egui::Color32::from_rgb(120, 180, 255),
                                                    ));
                                                } else {
                                                    ui.label(text);
                                                }
                                            }
                                        });
                                    }
                                });
                        });
                }
            });

        // ---- 中间：收到的数据流 + 发送历史（上下两块）----
        egui::CentralPanel::default().show(ctx, |ui| {
            let half = ((ui.available_height() - 100.0) / 2.0).max(30.0);

            // 上：收到的数据
            ui.horizontal(|ui| {
                ui.heading("收到的数据");
                ui.label(
                    egui::RichText::new(format!("最近 {} 条", log.len()))
                        .small()
                        .weak(),
                );
                if ui.small_button("清空视图").clicked() {
                    if let Ok(mut st) = self.state.write() {
                        st.data_log.clear();
                    }
                }
            });
            ui.separator();

            egui::ScrollArea::both()
                .id_salt("data_log_scroll")
                .max_height(half)
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if log.is_empty() {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(
                                "（暂无收到数据）连接设备并订阅通知（notify）后，这里会实时滚动显示",
                            )
                            .weak(),
                        );
                        return;
                    }
                    for line in &log {
                        ui.label(egui::RichText::new(line).monospace().small());
                    }
                });

            ui.add_space(8.0);

            // 下：发送的数据（GUI 写入与 MCP write 都记在这里）
            ui.horizontal(|ui| {
                ui.heading("发送的数据");
                ui.label(
                    egui::RichText::new(format!("最近 {} 条", tx_log.len()))
                        .small()
                        .weak(),
                );
                if ui.small_button("清空视图").clicked() {
                    if let Ok(mut st) = self.state.write() {
                        st.tx_log.clear();
                    }
                }
            });
            ui.separator();

            egui::ScrollArea::both()
                .id_salt("tx_log_scroll")
                .max_height(half)
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if tx_log.is_empty() {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(
                                "（暂无发送记录）在下方写入区发数据，或由 MCP 客户端调用 write，这里会记录每一条（成功/失败）",
                            )
                            .weak(),
                        );
                        return;
                    }
                    for line in &tx_log {
                        let color = if line.contains('✓') {
                            egui::Color32::from_rgb(120, 190, 140)
                        } else {
                            egui::Color32::from_rgb(220, 130, 110)
                        };
                        ui.label(
                            egui::RichText::new(line)
                                .monospace()
                                .small()
                                .color(color),
                        );
                    }
                });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actions_send_expected_commands_and_reject_empty_selection() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = BleApp::new(
            Arc::new(RwLock::new(AppState::default())),
            tx,
            "127.0.0.1:8765".into(),
        );
        app.connect_selected();
        assert!(rx.try_recv().is_err());
        assert!(!app.hint.is_empty());
        app.scan();
        assert!(matches!(
            rx.try_recv().unwrap(),
            GuiCmd::Scan { seconds: 5 }
        ));
        app.selected = Some("address".into());
        app.connect_selected();
        assert!(matches!(rx.try_recv().unwrap(),GuiCmd::Connect{address} if address=="address"));
        app.disconnect();
        assert!(matches!(rx.try_recv().unwrap(), GuiCmd::Disconnect));
        app.write_uuid.clear();
        app.write();
        assert!(rx.try_recv().is_err());
        app.write_uuid = " uuid ".into();
        app.write_data = " 01 ff ".into();
        app.write();
        assert!(
            matches!(rx.try_recv().unwrap(),GuiCmd::Write{char_uuid,data_hex} if char_uuid=="uuid" && data_hex=="01 ff")
        );
        drop(rx);
        app.scan();
        assert!(app.hint.contains("未送达"));
    }
}
