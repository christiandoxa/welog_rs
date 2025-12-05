//! Counterpart of `pkg/infrastructure/logger/log.go`
//!
//! In Go, the logger uses logrus + a hook to Elasticsearch with an async hook and
//! a `logs.txt` fallback file if sending to ES fails.
//!
//! In Rust:
//! - We use a worker thread with a mpsc channel for non-blocking logging.
//! - Send to Elasticsearch with `ureq` (blocking, but inside the worker thread).
//! - On failure, write one-line JSON to `logs.txt` (max 1GB, trimmed from the top).

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose};
use chrono::Utc;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::envkey;
use crate::util::LogFields;

/// Max fallback file size: 1GB (same as the Go version)
const FALLBACK_MAX_BYTES: u64 = 1 << 30;
/// Path to fallback log file
const FALLBACK_LOG_PATH: &str = "logs.txt";

/// Main logger that sends logs to Elasticsearch (if configured) and to the fallback file.
#[derive(Clone)]
pub struct Logger {
    inner: Arc<LoggerInner>,
}

struct LoggerInner {
    agent: ureq::Agent,
    elastic_url: Option<String>,
    index_prefix: Option<String>,
    username: Option<String>,
    password: Option<String>,
    fallback_path: PathBuf,
    fallback_lock: Mutex<()>,
    sender: Sender<LogFields>,
}

static LOGGER: OnceCell<Logger> = OnceCell::new();

impl Logger {
    /// Return the global (singleton) logger instance.
    pub fn global() -> &'static Logger {
        LOGGER.get_or_init(Logger::new_from_env)
    }

    fn new_from_env() -> Logger {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .build()
            .into();

        let elastic_url = std::env::var(envkey::ELASTIC_URL).ok();
        let index_prefix = std::env::var(envkey::ELASTIC_INDEX).ok();
        let username = std::env::var(envkey::ELASTIC_USERNAME).ok();
        let password = std::env::var(envkey::ELASTIC_PASSWORD).ok();

        let (sender, receiver) = channel::<LogFields>();

        let inner = Arc::new(LoggerInner {
            agent,
            elastic_url,
            index_prefix,
            username,
            password,
            fallback_path: PathBuf::from(FALLBACK_LOG_PATH),
            fallback_lock: Mutex::new(()),
            sender,
        });

        let worker_inner = inner.clone();
        thread::spawn(move || worker_loop(worker_inner, receiver));

        Logger { inner }
    }

    /// Send structured log to the worker.
    ///
    /// Similar to `logrus.Entry.WithFields(...).Info()` in Go: `fields` is the log field map.
    pub fn log(&self, fields: LogFields) {
        match serde_json::to_string(&fields) {
            Ok(json) => println!("{json}"),
            Err(err) => eprintln!("welog_rs: failed to serialize log for stdout: {err}"),
        }

        if let Err(err) = self.inner.sender.send(fields) {
            eprintln!("welog_rs: failed to enqueue log to worker: {err}");
        }
    }
}

/// Helper so the API resembles `logger.Logger()` in Go.
pub fn logger() -> &'static Logger {
    Logger::global()
}

#[cfg(test)]
pub(crate) fn test_logger_with_sender(sender: Sender<LogFields>) -> Logger {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(5)))
        .build()
        .into();

    Logger {
        inner: Arc::new(LoggerInner {
            agent,
            elastic_url: None,
            index_prefix: None,
            username: None,
            password: None,
            fallback_path: PathBuf::from("test_logs.txt"),
            fallback_lock: Mutex::new(()),
            sender,
        }),
    }
}

#[cfg(test)]
pub(crate) fn test_build_fallback_bytes(fields: &LogFields, hook_err: Option<&str>) -> Vec<u8> {
    test_inner(PathBuf::from("test_logs.txt")).build_fallback_bytes(fields, hook_err)
}

#[cfg(test)]
pub(crate) fn test_trim_oldest_lines(path: &Path, bytes_to_free: u64) -> io::Result<()> {
    test_inner(path.to_path_buf()).trim_oldest_lines(bytes_to_free)
}

#[cfg(test)]
pub(crate) fn test_send_to_elastic_without_config(fields: &LogFields) -> Result<(), String> {
    test_inner(PathBuf::from("unused.txt")).send_to_elastic(fields)
}

#[cfg(test)]
pub(crate) fn test_write_fallback(
    path: &Path,
    fields: &LogFields,
    hook_err: Option<&str>,
) -> io::Result<()> {
    test_inner(path.to_path_buf()).write_fallback(fields, hook_err)
}

#[cfg(test)]
fn test_inner(path: PathBuf) -> LoggerInner {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(50)))
        .build()
        .into();
    let (sender, _receiver) = channel();

    LoggerInner {
        agent,
        elastic_url: None,
        index_prefix: None,
        username: None,
        password: None,
        fallback_path: path,
        fallback_lock: Mutex::new(()),
        sender,
    }
}

#[cfg(test)]
pub(crate) fn run_worker_with_path(path: &Path, receiver: Receiver<LogFields>) {
    let inner = test_inner(path.to_path_buf());
    worker_loop(Arc::new(inner), receiver);
}

fn worker_loop(inner: Arc<LoggerInner>, receiver: Receiver<LogFields>) {
    for fields in receiver {
        if let Err(err) = inner.send_to_elastic(&fields)
            && let Err(fallback_err) = inner.write_fallback(&fields, Some(&err))
        {
            eprintln!(
                "welog_rs: failed to write fallback log to file {:?}: {fallback_err}",
                inner.fallback_path
            );
        }
    }
}

impl LoggerInner {
    fn send_to_elastic(&self, fields: &LogFields) -> Result<(), String> {
        let elastic_url = match &self.elastic_url {
            Some(url) if !url.is_empty() => url,
            _ => return Err("ELASTIC_URL__ not configured".to_string()),
        };

        let index_prefix = self
            .index_prefix
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("welog");

        // indexNameFunc in Go: `<ElasticIndex>-YYYY-MM-DD`
        let index_name = format!("{index_prefix}-{}", Utc::now().format("%Y-%m-%d"));

        let url = format!("{}/{}/_doc", elastic_url.trim_end_matches('/'), index_name);

        let mut req = self.agent.post(&url);

        if let (Some(user), Some(pass)) = (&self.username, &self.password) {
            let credentials = format!("{user}:{pass}");
            let encoded = general_purpose::STANDARD.encode(credentials);
            req = req.header("Authorization", format!("Basic {encoded}"));
        }

        let body: Value = Value::Object(fields.clone());

        let resp = match req.send_json(body) {
            Ok(resp) => resp,
            Err(ureq::Error::StatusCode(code)) => {
                return Err(format!("Elasticsearch HTTP error: {code}"));
            }
            Err(err) => return Err(format!("Elasticsearch request error: {err}")),
        };

        if !resp.status().is_success() {
            return Err(format!("Elasticsearch HTTP error: {}", resp.status()));
        }

        Ok(())
    }

    fn write_fallback(&self, fields: &LogFields, hook_err: Option<&str>) -> io::Result<()> {
        let log_bytes = self.build_fallback_bytes(fields, hook_err);
        if log_bytes.is_empty() || log_bytes.len() as u64 > FALLBACK_MAX_BYTES {
            return Ok(());
        }

        let _guard = self.fallback_lock.lock();

        self.ensure_fallback_file()?;
        self.ensure_fallback_capacity(log_bytes.len() as u64)?;
        self.append_fallback(&log_bytes)?;

        Ok(())
    }

    fn build_fallback_bytes(&self, fields: &LogFields, hook_err: Option<&str>) -> Vec<u8> {
        let mut map = Map::new();
        for (k, v) in fields.iter() {
            map.insert(k.clone(), v.clone());
        }

        if let Some(err) = hook_err {
            map.insert("hook_error".to_string(), Value::String(err.to_string()));
        }

        let mut data = serde_json::to_vec(&Value::Object(map)).unwrap_or_else(|e| {
            eprintln!("welog_rs: failed to serialize fallback log: {e}");
            Vec::new()
        });

        if !data.ends_with(b"\n") {
            data.push(b'\n');
        }

        data
    }

    fn ensure_fallback_file(&self) -> io::Result<()> {
        if !self.fallback_path.exists() {
            File::create(&self.fallback_path)?;
        }
        Ok(())
    }

    fn ensure_fallback_capacity(&self, additional: u64) -> io::Result<()> {
        let size = self.file_size(&self.fallback_path)?;
        let required = size + additional;

        if required <= FALLBACK_MAX_BYTES {
            return Ok(());
        }

        let bytes_to_free = required - FALLBACK_MAX_BYTES;
        self.trim_oldest_lines(bytes_to_free)
    }

    fn append_fallback(&self, log_bytes: &[u8]) -> io::Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.fallback_path)?;
        f.write_all(log_bytes)?;
        Ok(())
    }

    fn file_size(&self, path: &Path) -> io::Result<u64> {
        Ok(fs::metadata(path)?.len())
    }

    /// Similar to `trimOldestLines` in Go:
    /// drop the oldest lines until `bytes_to_free` is freed.
    fn trim_oldest_lines(&self, bytes_to_free: u64) -> io::Result<()> {
        let src = File::open(&self.fallback_path)?;
        let mut reader = BufReader::new(src);

        let tmp_path = self.fallback_path.with_extension("tmp");

        let mut tmp = File::create(&tmp_path)?;

        let mut removed: u64 = 0;
        let mut buf = String::new();

        // Drop lines until bytes_to_free is reached, then write from the next line onward.
        while reader.read_line(&mut buf)? != 0 {
            let len_with_newline = buf.len() as u64;
            removed += len_with_newline;
            if removed >= bytes_to_free {
                // Write this line as the first line in the new file.
                tmp.write_all(buf.as_bytes())?;
                buf.clear();
                break;
            }
            buf.clear();
        }

        // Write remaining lines.
        while reader.read_line(&mut buf)? != 0 {
            tmp.write_all(buf.as_bytes())?;
            buf.clear();
        }

        tmp.flush()?;
        drop(tmp);

        fs::rename(tmp_path, &self.fallback_path)?;

        Ok(())
    }
}
