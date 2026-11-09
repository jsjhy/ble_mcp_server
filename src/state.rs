//! 共享应用状态：连接状态、设备列表、收到的数据流。
//! 被 BLE 层写入、被 GUI 与 MCP 层读取。

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, RwLock};

use serde::Serialize;

/// GUI 向 BLE 控制任务下发的命令。
#[derive(Clone, Debug)]
pub enum GuiCmd {
    Scan {
        seconds: u64,
    },
    Connect {
        address: String,
    },
    Disconnect,
    /// 向指定特征写入十六进制数据（data_hex 形如 "41 54" 或 "0102"）。
    Write {
        char_uuid: String,
        data_hex: String,
    },
}

/// 一个发现的 BLE 外设。
#[derive(Clone, Debug, Default, Serialize)]
pub struct DeviceInfo {
    pub address: String,
    pub name: Option<String>,
    pub rssi: Option<i16>,
}

/// 一个 GATT 特征。
#[derive(Clone, Debug, Serialize)]
pub struct CharInfo {
    pub uuid: String,
    pub readable: bool,
    pub writable: bool,
    pub notifiable: bool,
}

/// 一个 GATT 服务。
#[derive(Clone, Debug, Serialize)]
pub struct ServiceInfo {
    pub uuid: String,
    pub characteristics: Vec<CharInfo>,
}

/// 全局共享状态。
#[derive(Clone, Debug, Default)]
pub struct AppState {
    /// 当前是否正在扫描
    pub scanning: bool,
    /// 已发现的设备列表
    pub devices: Vec<DeviceInfo>,
    /// 当前连接的设备地址（None 表示未连接）
    pub connected_address: Option<String>,
    /// 已连接设备上发现的 GATT 服务
    pub services: Vec<ServiceInfo>,
    /// 收到的通知数据流（环形缓冲）
    pub data_log: VecDeque<String>,
    /// 发送历史（GUI 与 MCP 的写入都记一笔，环形缓冲）
    pub tx_log: VecDeque<String>,
    /// 数据日志上限
    pub data_log_limit: usize,
    /// 连接失败（被设备拒绝）过的地址。
    /// 只用于在列表里打标记 / 按用户勾选过滤，默认仍然显示，避免漏设备。
    pub blocked: HashSet<String>,
    /// 最近一次 GUI 操作失败的原因（GUI 顶部红字显示，成功后清空）
    pub last_error: Option<String>,
}

/// 拒连记录存放位置：exe 同目录 blocked_devices.txt（一行一个地址）。
fn blocked_file() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|d| d.join("blocked_devices.txt"))
}

pub fn load_blocked() -> HashSet<String> {
    let mut set = HashSet::new();
    if let Some(p) = blocked_file() {
        if let Ok(txt) = std::fs::read_to_string(&p) {
            for line in txt.lines() {
                let s = line.trim();
                if !s.is_empty() {
                    set.insert(s.to_owned());
                }
            }
        }
    }
    set
}

impl AppState {
    pub fn new() -> Arc<RwLock<Self>> {
        Arc::new(RwLock::new(Self {
            data_log_limit: 200,
            blocked: load_blocked(),
            ..Default::default()
        }))
    }

    /// 把拒连记录写回磁盘（失败则静默，不影响主流程）。
    pub fn persist_blocked(&self) {
        if let Some(p) = blocked_file() {
            let mut v: Vec<&String> = self.blocked.iter().collect();
            v.sort();
            let txt = v.into_iter().cloned().collect::<Vec<_>>().join("\n");
            if let Err(e) = std::fs::write(&p, txt) {
                log::warn!("写入拒连记录失败 {p:?}: {e}");
            }
        }
    }

    /// 追加一条收到的数据到日志。
    pub fn push_data(state: &Arc<RwLock<Self>>, line: String) {
        crate::logging::traffic("RX", &line);
        if let Ok(mut s) = state.write() {
            if s.data_log_limit == 0 {
                s.data_log.clear();
                return;
            }
            while s.data_log.len() >= s.data_log_limit {
                s.data_log.pop_front();
            }
            s.data_log.push_back(line);
        }
    }

    /// 追加一条发送记录（成功/失败都记）。
    pub fn push_tx(state: &Arc<RwLock<Self>>, line: String) {
        crate::logging::traffic("TX", &line);
        if let Ok(mut s) = state.write() {
            if s.data_log_limit == 0 {
                s.tx_log.clear();
                return;
            }
            while s.tx_log.len() >= s.data_log_limit {
                s.tx_log.pop_front();
            }
            s.tx_log.push_back(line);
        }
    }

    /// 本地时间 HH:MM:SS（本项目跑在国内机器上，按 UTC+8 换算）。
    pub fn local_hms() -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let t = (secs + 8 * 3600) % 86400;
        format!("{:02}:{:02}:{:02}", t / 3600, (t % 3600) / 60, t % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_keep_latest_entries_and_evict_independently() {
        let state = Arc::new(RwLock::new(AppState {
            data_log_limit: 2,
            ..Default::default()
        }));
        for line in ["first", "second", "third"] {
            AppState::push_data(&state, line.into());
        }
        AppState::push_tx(&state, "sent".into());
        {
            let s = state.read().unwrap();
            assert_eq!(
                s.data_log.iter().map(String::as_str).collect::<Vec<_>>(),
                ["second", "third"]
            );
            assert_eq!(s.tx_log.front().unwrap(), "sent");
        }
        AppState::push_tx(&state, "sent2".into());
        AppState::push_tx(&state, "sent3".into());
        let s = state.read().unwrap();
        assert_eq!(
            s.tx_log.iter().map(String::as_str).collect::<Vec<_>>(),
            ["sent2", "sent3"]
        );
        assert_eq!(s.data_log.len(), 2);
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[test]
    fn zero_and_reduced_log_limits_are_respected() {
        let state = Arc::new(RwLock::new(AppState {
            data_log_limit: 4,
            ..Default::default()
        }));
        for _ in 0..4 {
            AppState::push_data(&state, "rx".into());
            AppState::push_tx(&state, "tx".into());
        }
        state.write().unwrap().data_log_limit = 1;
        AppState::push_data(&state, "last".into());
        AppState::push_tx(&state, "last".into());
        assert_eq!(state.read().unwrap().data_log.len(), 1);
        assert_eq!(state.read().unwrap().tx_log.len(), 1);
        state.write().unwrap().data_log_limit = 0;
        AppState::push_data(&state, "ignored".into());
        AppState::push_tx(&state, "ignored".into());
        assert!(state.read().unwrap().data_log.is_empty());
        assert!(state.read().unwrap().tx_log.is_empty());
    }
}
