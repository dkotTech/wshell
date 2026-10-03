//! App logs and the audit log.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tokio::sync::broadcast;

use crate::util::now_utc;

const RING: usize = 1000;
const MAX_FILE: u64 = 4 << 20;

/// Log of a single app: a ring buffer, a file and a subscription for `logs -f`.
pub struct AppLog {
    inner: Mutex<Inner>,
    tx: broadcast::Sender<String>,
}

struct Inner {
    ring: VecDeque<String>,
    path: PathBuf,
    file: Option<File>,
}

impl AppLog {
    pub fn new(path: PathBuf) -> Self {
        let ring = std::fs::read_to_string(&path)
            .map(|s| {
                let lines: Vec<&str> = s.lines().collect();
                lines[lines.len().saturating_sub(RING)..].iter().map(|l| l.to_string()).collect()
            })
            .unwrap_or_default();
        let (tx, _) = broadcast::channel(256);
        AppLog { inner: Mutex::new(Inner { ring, path, file: None }), tx }
    }

    pub fn write(&self, source: &str, message: &str) {
        let mut inner = self.inner.lock().unwrap();
        for text in message.lines() {
            let line = format!("{} {source} {text}", now_utc());
            if inner.ring.len() == RING {
                inner.ring.pop_front();
            }
            inner.ring.push_back(line.clone());
            inner.append(&line);
            let _ = self.tx.send(line);
        }
    }

    /// The last `n` lines and a subscription to the following ones, atomically: `write` writes and
    /// broadcasts under the same lock, so lines are neither lost nor duplicated.
    pub fn tail_and_subscribe(&self, n: usize) -> (Vec<String>, broadcast::Receiver<String>) {
        let inner = self.inner.lock().unwrap();
        let tail = inner.ring.iter().skip(inner.ring.len().saturating_sub(n)).cloned().collect();
        (tail, self.tx.subscribe())
    }
}

impl Inner {
    fn append(&mut self, line: &str) {
        if self.file.is_none() {
            self.file = open_private(&self.path).ok();
        }
        let Some(f) = &mut self.file else { return };
        if f.metadata().map(|m| m.len() > MAX_FILE).unwrap_or(false) {
            // Simple rotation: one previous file.
            let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
            self.file = open_private(&self.path).ok();
        }
        if let Some(f) = &mut self.file {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn open_private(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).mode(0o600).open(path)
}

/// Permission audit log: grant, denial, revocation. Without message contents.
pub struct Audit {
    file: Mutex<Option<File>>,
}

impl Audit {
    pub fn new(path: &Path) -> Self {
        Audit { file: Mutex::new(open_private(path).ok()) }
    }

    pub fn record(&self, app: &str, event: &str, detail: &str) {
        tracing::info!(target: "audit", app, event, detail);
        if let Some(f) = self.file.lock().unwrap().as_mut() {
            let _ = writeln!(f, "{} {app} {event} {detail}", now_utc());
        }
    }
}
