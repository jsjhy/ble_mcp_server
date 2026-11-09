//! BLE 中央设备层：扫描、连接、服务发现、读写、通知订阅。
//! 基于 btleplug 0.11（Windows 11 上自动使用 WinRT 后端）。

use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Result};
#[cfg(not(test))]
use btleplug::api::{Central as _, Manager as _, Peripheral as _};
use btleplug::api::{CharPropFlags, Characteristic, ScanFilter, WriteType};
#[cfg(not(test))]
use btleplug::platform::{Adapter, Manager, Peripheral};
#[cfg(test)]
#[path = "ble_mock.rs"]
pub(crate) mod mock;
use futures::StreamExt;
#[cfg(test)]
use mock::{Adapter, Manager, Peripheral};

use crate::state::{AppState, CharInfo, DeviceInfo, ServiceInfo};

/// BLE Central 管理器。
pub struct BleCentral {
    _manager: Manager,
    adapter: Adapter,
    /// 当前连接的外设（None 表示未连接）
    peripheral: Option<Peripheral>,
    /// 通知监听任务
    notify_task: Option<tokio::task::JoinHandle<()>>,
    state: Arc<RwLock<AppState>>,
    subscriptions: std::collections::BTreeSet<String>,
}

struct ScanGuard(Arc<RwLock<AppState>>);
impl Drop for ScanGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.write() {
            state.scanning = false;
        }
    }
}

impl BleCentral {
    /// 创建管理器并获取适配器。
    pub async fn new(state: Arc<RwLock<AppState>>) -> Result<Self> {
        let manager = Manager::new().await?;
        let adapter = manager
            .adapters()
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("未找到蓝牙适配器（请确认蓝牙已开启）"))?;
        log::info!("使用蓝牙适配器: {}", adapter.adapter_info().await?);
        Ok(Self {
            _manager: manager,
            adapter,
            peripheral: None,
            notify_task: None,
            state,
            subscriptions: Default::default(),
        })
    }

    /// 开始扫描，持续 seconds 秒，返回发现的设备列表。
    pub async fn scan(&mut self, seconds: u64) -> Result<Vec<DeviceInfo>> {
        {
            let mut st = self.state.write().unwrap();
            st.scanning = true;
            st.devices.clear();
        }
        let _scanning = ScanGuard(self.state.clone());
        self.adapter
            .start_scan(ScanFilter::default())
            .await
            .map_err(|e| anyhow!("启动扫描失败: {e}"))?;

        tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;

        self.adapter
            .stop_scan()
            .await
            .map_err(|e| anyhow!("停止扫描失败: {e}"))?;

        let mut out = Vec::new();
        for p in self.adapter.peripherals().await? {
            let props = match p.properties().await? {
                Some(v) => v,
                None => continue,
            };
            let addr = props.address.to_string();
            out.push(DeviceInfo {
                address: addr.clone(),
                name: props.local_name,
                rssi: props.rssi,
            });
        }

        // 按 RSSI 排序（信号强的在前），None 排最后
        out.sort_by(|a, b| match (a.rssi, b.rssi) {
            (Some(x), Some(y)) => y.cmp(&x),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });

        {
            let mut st = self.state.write().unwrap();
            st.scanning = false;
            st.devices = out.clone();
        }
        Ok(out)
    }

    /// 按地址查找并连接外设，自动配对并发现 GATT 服务。
    pub async fn connect(&mut self, address: &str) -> Result<()> {
        self.disconnect().await.ok();

        let periph = self.find_by_address(address).await?;

        if let Err(e) = periph.connect().await {
            // 设备拒绝连接：记一笔，界面上打标记（默认不隐藏，避免扫描漏设备）
            {
                let mut st = self.state.write().unwrap();
                st.blocked.insert(address.to_string());
                st.persist_blocked();
                st.last_error = Some(format!("{address} 拒绝连接: {e}"));
            }
            log::warn!("设备 {address} 拒绝连接，已标记为「拒连」: {e}");
            return Err(anyhow!("连接失败（设备拒绝连接）: {e}"));
        }

        if let Err(e) = periph.discover_services().await {
            let _ = periph.disconnect().await;
            return Err(anyhow!("GATT 服务发现失败: {e}"));
        }

        // 收集服务/特征
        let services = self.collect_services(&periph).await?;

        // 启动通知监听（0.11: notifications() 流）
        let st = self.state.clone();
        let p_clone = periph.clone();
        let handle = tokio::spawn(async move {
            let stream = match p_clone.notifications().await {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("获取通知流失败: {e}");
                    return;
                }
            };
            futures::pin_mut!(stream);
            while let Some(notif) = stream.next().await {
                AppState::push_data(
                    &st,
                    format!(
                        "[{:02X}] RX ({}): {}",
                        notif.value.len(),
                        notif.uuid,
                        hex(&notif.value)
                    ),
                );
            }
        });

        self.peripheral = Some(periph.clone());
        self.notify_task = Some(handle);

        {
            let mut st = self.state.write().unwrap();
            st.connected_address = Some(address.to_string());
            st.services = services;
            st.last_error = None;
            // 之前被拒过、这次连上了：撤掉标记
            if st.blocked.remove(address) {
                st.persist_blocked();
            }
        }
        Ok(())
    }

    /// 断开当前连接。
    pub async fn disconnect(&mut self) -> Result<()> {
        self.subscriptions.clear();
        if let Some(p) = self.peripheral.take() {
            let _ = p.disconnect().await;
        }
        if let Some(t) = self.notify_task.take() {
            t.abort();
        }
        {
            let mut st = self.state.write().unwrap();
            st.connected_address = None;
            st.services.clear();
        }
        Ok(())
    }

    /// 读取指定 UUID 特征。
    pub async fn read(&mut self, char_uuid: &str) -> Result<Vec<u8>> {
        self.ensure_connected().await?;
        let p = self
            .peripheral
            .clone()
            .ok_or_else(|| anyhow!("当前未连接任何设备"))?;
        let chr = self.find_characteristic(&p, char_uuid)?;
        let val = p.read(&chr).await.map_err(|e| anyhow!("读取失败: {e}"))?;
        Ok(val)
    }

    /// 写入指定 UUID 特征。
    /// 连接若已失效会自动重连；带响应写失败则降级为不带响应再试一次。
    /// 不论成功失败，都会在「发送的数据」历史里留一条记录。
    pub async fn write(&mut self, char_uuid: &str, data: &[u8]) -> Result<()> {
        let res = self.write_impl(char_uuid, data).await;
        let line = match &res {
            Ok(_) => format!(
                "[{}] ✓ {} ({}B) {}",
                AppState::local_hms(),
                char_uuid,
                data.len(),
                hex(data)
            ),
            Err(e) => format!(
                "[{}] ✗ {} ({}B) {} — {}",
                AppState::local_hms(),
                char_uuid,
                data.len(),
                hex(data),
                e
            ),
        };
        AppState::push_tx(&self.state, line);
        res
    }

    async fn write_impl(&mut self, char_uuid: &str, data: &[u8]) -> Result<()> {
        let uuid = uuid::Uuid::parse_str(char_uuid).map_err(|e| anyhow!("UUID 格式错误: {e}"))?;
        for attempt in 0..2u8 {
            self.ensure_connected().await?;
            let p = self
                .peripheral
                .clone()
                .ok_or_else(|| anyhow!("当前未连接任何设备"))?;
            let chr = self.find_characteristic(&p, char_uuid)?;
            let wt = if attempt == 0 {
                WriteType::WithResponse
            } else {
                WriteType::WithoutResponse
            };
            let res =
                tokio::time::timeout(std::time::Duration::from_secs(8), p.write(&chr, data, wt))
                    .await;
            match res {
                Err(_elapsed) => {
                    log::warn!("写入尝试 {}（{wt:?}）超时（8s）", attempt + 1);
                    let _ = p.disconnect().await;
                }
                Ok(Ok(())) => {
                    log::info!("已写入 {} 字节 -> {uuid}（{wt:?}）", data.len());
                    return Ok(());
                }
                Ok(Err(e)) => {
                    log::warn!("写入尝试 {}（{wt:?}）失败: {e}", attempt + 1);
                    let _ = p.disconnect().await;
                }
            }
        }
        Err(anyhow!("写入失败：带响应与不带响应两次尝试均失败"))
    }

    /// 订阅/取消订阅指定 UUID 特征的通知。
    pub async fn subscribe(&mut self, char_uuid: &str, on: bool) -> Result<()> {
        self.ensure_connected().await?;
        let p = self
            .peripheral
            .clone()
            .ok_or_else(|| anyhow!("当前未连接任何设备"))?;
        let chr = self.find_characteristic(&p, char_uuid)?;
        if on {
            p.subscribe(&chr)
                .await
                .map_err(|e| anyhow!("订阅失败: {e}"))?;
            self.subscriptions.insert(chr.uuid.to_string());
        } else {
            p.unsubscribe(&chr)
                .await
                .map_err(|e| anyhow!("取消订阅失败: {e}"))?;
            self.subscriptions.remove(&chr.uuid.to_string());
        }
        Ok(())
    }

    /// 确保连接可用：若 WinRT GATT 会话已关闭（设备空闲断开等），自动重连并重新发现服务。
    async fn ensure_connected(&mut self) -> Result<()> {
        let p = match self.peripheral.clone() {
            Some(p) => p,
            None => return Err(anyhow!("当前未连接任何设备")),
        };
        if p.is_connected().await.unwrap_or(false) {
            return Ok(());
        }
        log::warn!("检测到 BLE 连接已关闭，自动重连...");
        tokio::time::timeout(std::time::Duration::from_secs(12), p.connect())
            .await
            .map_err(|_| anyhow!("自动重连超时"))?
            .map_err(|e| anyhow!("自动重连失败: {e}"))?;
        tokio::time::timeout(std::time::Duration::from_secs(12), p.discover_services())
            .await
            .map_err(|_| anyhow!("重连后服务发现超时"))?
            .map_err(|e| anyhow!("重连后服务发现失败: {e}"))?;
        for uuid in &self.subscriptions {
            let chr = self.find_characteristic(&p, uuid)?;
            p.subscribe(&chr)
                .await
                .map_err(|e| anyhow!("恢复订阅失败: {e}"))?;
        }
        self.state.write().unwrap().services = self.collect_services(&p).await?;
        log::info!("自动重连成功");
        Ok(())
    }

    /// 读取当前连接状态摘要。
    pub fn status(&self) -> String {
        match &self.connected_address() {
            Some(a) => format!("已连接: {a}"),
            None => "未连接".to_string(),
        }
    }

    fn connected_address(&self) -> Option<String> {
        self.state.read().unwrap().connected_address.clone()
    }

    async fn find_by_address(&self, address: &str) -> Result<Peripheral> {
        for p in self.adapter.peripherals().await? {
            let props = p.properties().await?;
            if let Some(props) = props {
                if props.address.to_string() == address {
                    return Ok(p);
                }
            }
        }
        Err(anyhow!("未找到地址为 {address} 的设备（请先扫描）"))
    }

    fn find_characteristic<'a>(
        &self,
        p: &'a Peripheral,
        char_uuid: &str,
    ) -> Result<Characteristic> {
        let uuid = uuid::Uuid::parse_str(char_uuid).map_err(|e| anyhow!("UUID 格式错误: {e}"))?;
        for s in p.services().iter() {
            for c in s.characteristics.iter() {
                if c.uuid == uuid {
                    return Ok(c.clone());
                }
            }
        }
        Err(anyhow!("未找到特征 {char_uuid}（请先连接并发现服务）"))
    }

    async fn collect_services(&self, p: &Peripheral) -> Result<Vec<ServiceInfo>> {
        let mut out = Vec::new();
        for s in p.services().iter() {
            let mut chars = Vec::new();
            for c in s.characteristics.iter() {
                let props = c.properties;
                chars.push(CharInfo {
                    uuid: c.uuid.to_string(),
                    readable: props.contains(CharPropFlags::READ),
                    writable: props.contains(CharPropFlags::WRITE)
                        || props.contains(CharPropFlags::WRITE_WITHOUT_RESPONSE),
                    notifiable: props.contains(CharPropFlags::NOTIFY)
                        || props.contains(CharPropFlags::INDICATE),
                });
            }
            out.push(ServiceInfo {
                uuid: s.uuid.to_string(),
                characteristics: chars,
            });
        }
        Ok(out)
    }
}

/// 字节数组转十六进制字符串。
pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 解析十六进制字符串（允许空格，如 "41 54" 或 "0102"）为字节数组。
pub fn parse_hex(s: &str) -> Result<Vec<u8>> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !clean.is_ascii() {
        return Err(anyhow!("无效十六进制：仅支持 ASCII 字符"));
    }
    if clean.len() % 2 != 0 {
        return Err(anyhow!("十六进制长度必须为偶数"));
    }
    (0..clean.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).map_err(|e| anyhow!("无效十六进制: {e}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_compact_spaced_and_mixed_case_hex() {
        for input in ["005aFF", "00 5a FF", "00\t5a\nFF", "00\u{3000}5a FF"] {
            assert_eq!(parse_hex(input).unwrap(), vec![0, 0x5a, 0xff]);
        }
        assert_eq!(parse_hex("  ").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn invalid_hex_returns_error_without_panicking() {
        for input in ["0", "012", "GG", "0x12", "中0", "é", "💡", "１２"] {
            assert!(parse_hex(input).is_err(), "input: {input:?}");
        }
    }

    #[test]
    fn every_byte_round_trips_through_display_hex() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(parse_hex(&hex(&bytes)).unwrap(), bytes);
        assert_eq!(hex(&[0, 10, 255]), "00 0A FF");
    }
}

#[cfg(test)]
impl BleCentral {
    pub(crate) async fn simulated(state: Arc<RwLock<AppState>>) -> Self {
        Self::new(state).await.unwrap()
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    const ADDR: &str = "AA:BB:CC:DD:EE:FF";
    const UUID: &str = "00000000-0000-0000-0000-000000000000";
    fn state() -> Arc<RwLock<AppState>> {
        Arc::new(RwLock::new(AppState {
            data_log_limit: 10,
            ..Default::default()
        }))
    }
    #[tokio::test]
    async fn scan_success_and_each_failure_reset_flag() {
        for fail in [
            None,
            Some("start_scan"),
            Some("stop_scan"),
            Some("peripherals"),
            Some("properties"),
        ] {
            let state = state();
            let mut b = BleCentral::simulated(state.clone()).await;
            b.adapter.0 .0.lock().unwrap().fail = fail;
            assert_eq!(b.scan(0).await.is_ok(), fail.is_none());
            assert!(!state.read().unwrap().scanning);
            if fail.is_none() {
                assert_eq!(state.read().unwrap().devices[0].address, ADDR);
            }
        }
    }
    #[tokio::test]
    async fn cancelling_scan_resets_flag() {
        let state = state();
        let mut b = BleCentral::simulated(state.clone()).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(5), b.scan(60))
                .await
                .is_err()
        );
        assert!(!state.read().unwrap().scanning);
    }
    #[tokio::test]
    async fn discovery_failure_disconnects_untracked_device() {
        let state = state();
        let mut b = BleCentral::simulated(state.clone()).await;
        b.adapter.0 .0.lock().unwrap().fail = Some("discover");
        assert!(b.connect(ADDR).await.is_err());
        assert!(!b.adapter.0 .0.lock().unwrap().connected);
        assert!(state.read().unwrap().connected_address.is_none());
    }
    #[tokio::test]
    async fn connect_read_write_subscribe_reconnect_and_disconnect() {
        let state = state();
        let mut b = BleCentral::simulated(state.clone()).await;
        assert!(b.read(UUID).await.is_err());
        assert!(b.connect("missing").await.is_err());
        b.connect(ADDR).await.unwrap();
        assert_eq!(state.read().unwrap().services.len(), 1);
        assert_eq!(b.read(UUID).await.unwrap(), vec![0, 255]);
        assert!(b.read("bad").await.is_err());
        assert!(b
            .read("00000000-0000-0000-0000-000000000001")
            .await
            .is_err());
        b.subscribe(UUID, true).await.unwrap();
        b.adapter.0 .0.lock().unwrap().connected = false;
        b.read(UUID).await.unwrap();
        assert_eq!(
            b.adapter
                .0
                 .0
                .lock()
                .unwrap()
                .calls
                .iter()
                .filter(|s| *s == "subscribe")
                .count(),
            2
        );
        b.subscribe(UUID, false).await.unwrap();
        assert!(b.subscriptions.is_empty());
        b.write(UUID, &[1]).await.unwrap();
        b.adapter.0 .0.lock().unwrap().fail = Some("write_response");
        b.write(UUID, &[2]).await.unwrap();
        assert!(b
            .adapter
            .0
             .0
            .lock()
            .unwrap()
            .calls
            .contains(&"write_no_response".into()));
        assert!(b.write("bad", &[3]).await.is_err());
        assert_eq!(state.read().unwrap().tx_log.len(), 3);
        b.disconnect().await.unwrap();
        b.disconnect().await.unwrap();
        assert!(state.read().unwrap().services.is_empty());
        assert!(b.notify_task.is_none());
    }
    #[tokio::test]
    async fn read_and_subscription_failures_propagate() {
        let mut b = BleCentral::simulated(state()).await;
        b.connect(ADDR).await.unwrap();
        for (fail, on) in [("subscribe", true), ("unsubscribe", false)] {
            b.adapter.0 .0.lock().unwrap().fail = Some(fail);
            assert!(b.subscribe(UUID, on).await.is_err());
        }
        b.adapter.0 .0.lock().unwrap().fail = Some("read");
        assert!(b.read(UUID).await.is_err());
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    #[tokio::test]
    async fn reconnect_connection_discovery_and_resubscribe_errors_propagate() {
        for fail in ["connect", "discover", "subscribe"] {
            let state = Arc::new(RwLock::new(AppState::default()));
            let mut b = BleCentral::simulated(state).await;
            b.connect("AA:BB:CC:DD:EE:FF").await.unwrap();
            b.subscribe("00000000-0000-0000-0000-000000000000", true)
                .await
                .unwrap();
            {
                let mut c = b.adapter.0 .0.lock().unwrap();
                c.connected = false;
                c.fail = Some(fail);
            }
            assert!(
                b.read("00000000-0000-0000-0000-000000000000")
                    .await
                    .is_err(),
                "{fail}"
            );
        }
    }
    #[tokio::test]
    async fn unsubscribed_characteristic_is_not_restored() {
        let mut b = BleCentral::simulated(Arc::new(RwLock::new(AppState::default()))).await;
        b.connect("AA:BB:CC:DD:EE:FF").await.unwrap();
        let uuid = "00000000-0000-0000-0000-000000000000";
        b.subscribe(uuid, true).await.unwrap();
        b.subscribe(uuid, false).await.unwrap();
        {
            let mut c = b.adapter.0 .0.lock().unwrap();
            c.calls.clear();
            c.connected = false;
        }
        b.read(uuid).await.unwrap();
        assert!(!b
            .adapter
            .0
             .0
            .lock()
            .unwrap()
            .calls
            .contains(&"subscribe".into()));
    }
}
