use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{Mutex, OnceLock},
};

static TRAFFIC: OnceLock<Mutex<BoundedLog>> = OnceLock::new();

const MAX_LOG_BYTES: usize = 10 * 1024 * 1024;

fn overwrite(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(path)
}

/// FIFO by complete lines. Compact in batches to avoid rewriting on every record.
struct BoundedLog {
    file: File,
    size: usize,
    limit: usize,
}

impl BoundedLog {
    fn new(path: &Path, limit: usize) -> io::Result<Self> {
        Ok(Self {
            file: overwrite(path)?,
            size: 0,
            limit,
        })
    }
}

impl Write for BoundedLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.size.saturating_add(bytes.len()) <= self.limit {
            self.file.write_all(bytes)?;
            self.size += bytes.len();
        } else {
            self.file.seek(SeekFrom::Start(0))?;
            let mut retained = Vec::new();
            self.file.read_to_end(&mut retained)?;
            retained.extend_from_slice(bytes);
            let reserve = (self.limit / 10).min(self.limit.saturating_sub(bytes.len()));
            let target = self.limit - reserve;
            let cut = retained.len().saturating_sub(target);
            let start = if cut == 0 || retained[cut - 1] == b'\n' {
                cut
            } else {
                retained[cut..]
                    .iter()
                    .position(|b| *b == b'\n')
                    .map(|i| cut + i + 1)
                    .unwrap_or(retained.len())
            };
            let tail = &retained[start..];
            self.file.seek(SeekFrom::Start(0))?;
            self.file.write_all(tail)?;
            self.file.set_len(tail.len() as u64)?;
            self.size = tail.len();
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

pub fn init() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let directory = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("无法确定日志目录"))?
        .join("logs");
    std::fs::create_dir_all(&directory)?;
    let runtime = BoundedLog::new(&directory.join("runtime.log"), MAX_LOG_BYTES)?;
    let traffic = BoundedLog::new(&directory.join("traffic.log"), MAX_LOG_BYTES)?;
    TRAFFIC
        .set(Mutex::new(traffic))
        .map_err(|_| anyhow::anyhow!("日志已初始化"))?;
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("ble_server=info,warn"),
    )
    .target(env_logger::Target::Pipe(Box::new(runtime)))
    .write_style(env_logger::WriteStyle::Never)
    .try_init()?;
    std::panic::set_hook(Box::new(|info| {
        log::error!("程序异常: {info}");
    }));
    log::info!("程序启动，日志目录: {}", directory.display());
    Ok(())
}

pub fn traffic(direction: &str, line: &str) {
    if let Some(file) = TRAFFIC.get() {
        let result = file
            .lock()
            .map_err(|_| io::Error::other("日志锁损坏"))
            .and_then(|mut file| {
                writeln!(
                    file,
                    "[{}] {direction} {line}",
                    crate::state::AppState::local_hms()
                )?;
                file.flush()
            });
        if let Err(error) = result {
            log::error!("保存收发日志失败: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn new_session_overwrites_previous_log() {
        let path = std::env::temp_dir().join(format!("ble-log-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&path, "old content").unwrap();
        {
            let mut file = overwrite(&path).unwrap();
            writeln!(file, "new").unwrap();
            file.flush().unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod fifo_tests {
    use super::*;
    #[test]
    fn fifo_preserves_latest_complete_utf8_lines_and_size() {
        let path = std::env::temp_dir().join(format!("ble-fifo-{}", uuid::Uuid::new_v4()));
        {
            let mut log = BoundedLog::new(&path, 64).unwrap();
            for i in 0..100 {
                writeln!(log, "{i:03} 中文消息").unwrap();
                log.flush().unwrap();
                let bytes = std::fs::read(&path).unwrap();
                assert!(bytes.len() <= 64);
                let text = String::from_utf8(bytes).unwrap();
                assert!(text.ends_with(&format!("{i:03} 中文消息\n")));
                for line in text.lines() {
                    assert!(line.ends_with(" 中文消息"));
                }
            }
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("000"));
        assert!(text.contains("099"));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn exact_limit_and_oversized_line_remain_bounded() {
        let path = std::env::temp_dir().join(format!("ble-fifo-{}", uuid::Uuid::new_v4()));
        {
            let mut log = BoundedLog::new(&path, 8).unwrap();
            log.write_all(b"1234567\n").unwrap();
            assert_eq!(log.size, 8);
            log.write_all(b"too long a line\n").unwrap();
            assert!(log.size <= 8);
            log.write_all(b"new\n").unwrap();
            log.flush().unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
        }
        std::fs::remove_file(path).unwrap();
    }
}
