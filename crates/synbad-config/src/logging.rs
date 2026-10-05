//! Size-bounded logs shared by the daemon, Core readers and GUI launcher.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const LOG_BYTES: u64 = 2 * 1024 * 1024;
const BACKUPS: usize = 3;

#[derive(Clone)]
pub struct RotatingLog(Arc<Mutex<LogFile>>);

struct LogFile {
    path: PathBuf,
    file: Option<File>,
    bytes: u64,
    limit: u64,
}

impl RotatingLog {
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::with_limit(path, LOG_BYTES)
    }

    fn with_limit(path: &Path, limit: u64) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Migrate logs left by older unbounded writers as well as their
        // backups, retaining the newest diagnostics without reading GBs.
        for index in 0..=BACKUPS {
            let old = if index == 0 {
                path.to_path_buf()
            } else {
                backup_path(path, index)
            };
            trim_existing(&old, limit)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let bytes = file.metadata()?.len();
        Ok(Self(Arc::new(Mutex::new(LogFile {
            path: path.into(),
            file: Some(file),
            bytes,
            limit,
        }))))
    }
}

fn backup_path(path: &Path, index: usize) -> PathBuf {
    path.with_file_name(format!(
        "{}.{}",
        path.file_name().unwrap().to_string_lossy(),
        index
    ))
}

fn trim_existing(path: &Path, limit: u64) -> io::Result<()> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > limit {
        file.seek(SeekFrom::End(-(limit as i64)))?;
        let mut tail = vec![0; limit as usize];
        file.read_exact(&mut tail)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&tail)?;
        file.set_len(limit)?;
    }
    Ok(())
}

impl LogFile {
    fn backup(&self, index: usize) -> PathBuf {
        backup_path(&self.path, index)
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.take(); // close before renaming, including on Windows
        let _ = fs::remove_file(self.backup(BACKUPS));
        for index in (1..BACKUPS).rev() {
            let old = self.backup(index);
            if old.exists() {
                fs::rename(old, self.backup(index + 1))?;
            }
        }
        if self.path.exists() {
            fs::rename(&self.path, self.backup(1))?;
        }
        self.bytes = 0;
        Ok(())
    }
}

impl Write for RotatingLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut log = self
            .0
            .lock()
            .map_err(|_| io::Error::other("log lock poisoned"))?;
        // Split even one enormous write so neither the current log nor a
        // backup exceeds the cap. Old data is discarded after three rotations.
        let mut remaining = bytes;
        while !remaining.is_empty() {
            if log.bytes >= log.limit {
                log.rotate()?;
            }
            if log.file.is_none() {
                log.file = Some(
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log.path)?,
                );
            }
            let size = remaining.len().min((log.limit - log.bytes) as usize);
            log.file.as_mut().unwrap().write_all(&remaining[..size])?;
            log.bytes += size as u64;
            remaining = &remaining[size..];
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut log = self
            .0
            .lock()
            .map_err(|_| io::Error::other("log lock poisoned"))?;
        match log.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_writes_and_reopens_remain_bounded() {
        let root = std::env::temp_dir().join(format!(
            "synbad-logs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("core.log");
        fs::create_dir_all(&root).unwrap();
        fs::write(&path, [b'o'; 200]).unwrap();
        fs::write(backup_path(&path, 1), [b'b'; 200]).unwrap();
        let mut log = RotatingLog::with_limit(&path, 32).unwrap();
        assert_eq!(fs::read(&path).unwrap(), [b'o'; 32]);
        assert_eq!(fs::metadata(backup_path(&path, 1)).unwrap().len(), 32);
        log.write_all(&[b'x'; 300]).unwrap();
        drop(log);
        let mut log = RotatingLog::with_limit(&path, 32).unwrap();
        log.write_all(b"last crash reason\n").unwrap();
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("last crash reason"));
        for file in fs::read_dir(&root).unwrap() {
            assert!(file.unwrap().metadata().unwrap().len() <= 32);
        }
        assert_eq!(fs::read_dir(&root).unwrap().count(), 4);
        drop(log);
        fs::remove_dir_all(root).unwrap();
    }
}
