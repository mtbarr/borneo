use anyhow::Result;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::cli::OutputFormat;
use crate::types::ArtifactCoordinates;

pub(crate) enum Status {
    Begin { key: String, msg: String },
    Update { key: String, msg: String },
    End { key: String, msg: Option<String> },
    Clear,
    Fatal(String),
    Log(String),
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

pub(crate) static STATUS: std::sync::OnceLock<StatusHandle> = std::sync::OnceLock::new();

pub struct StatusHandle {
    tx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<Status>>>,
    format: OutputFormat,
}

impl StatusHandle {
    pub(crate) fn init(format: OutputFormat) -> tokio::sync::mpsc::UnboundedReceiver<Status> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        STATUS
            .set(Self {
                tx: std::sync::Mutex::new(Some(tx)),
                format,
            })
            .ok()
            .expect("StatusHandle already initialized");
        rx
    }

    pub fn get() -> &'static Self {
        STATUS.get().expect("StatusHandle not initialized")
    }

    pub(crate) fn send(&self, status: Status) {
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(status);
        }
    }

    pub(crate) fn shutdown(&self) {
        self.tx.lock().unwrap().take();
    }

    pub fn format(&self) -> OutputFormat {
        self.format
    }

    pub fn begin(&self, key: impl Into<String>, msg: impl Into<String>) {
        self.send(Status::Begin {
            key: key.into(),
            msg: msg.into(),
        });
    }

    pub fn update(&self, key: impl Into<String>, msg: impl Into<String>) {
        self.send(Status::Update {
            key: key.into(),
            msg: msg.into(),
        });
    }

    pub fn end(&self, key: impl Into<String>) {
        self.send(Status::End {
            key: key.into(),
            msg: None,
        });
    }

    pub fn end_log(&self, key: impl Into<String>, msg: impl Into<String>) {
        self.send(Status::End {
            key: key.into(),
            msg: Some(msg.into()),
        });
    }

    pub fn resolving(&self, coord: &ArtifactCoordinates) {
        self.begin(format!("resolve:{coord}"), format!("resolving {coord}"));
    }

    pub fn resolved(&self, coord: &ArtifactCoordinates) {
        self.end(format!("resolve:{coord}"));
    }

    pub fn downloading(&self, coord: &ArtifactCoordinates) {
        self.begin(format!("dl:{coord}"), format!("downloading {coord}"));
    }

    pub fn downloaded(&self, coord: &ArtifactCoordinates) {
        self.end(format!("dl:{coord}"));
    }

    pub fn clear(&self) {
        self.send(Status::Clear);
    }

    pub fn log(&self, msg: impl Into<String>) {
        self.send(Status::Log(msg.into()));
    }

    pub fn fatal(&self, msg: impl Into<String>) {
        self.send(Status::Fatal(msg.into()));
    }

    pub fn task<T>(
        &self,
        key: &str,
        spinner_msg: impl Into<String>,
        done_msg: impl Into<String>,
        f: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.begin(key, spinner_msg);
        let result = f();
        let msg = result.is_ok().then(|| done_msg.into());
        self.send(Status::End {
            key: key.to_string(),
            msg,
        });
        result
    }

    pub fn stdout(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.send(Status::Stdout(bytes));
        }
    }

    pub fn stderr(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.send(Status::Stderr(bytes));
        }
    }
}

const MAX_VISIBLE: usize = 4;

struct ProgressDisplay {
    multi: MultiProgress,
    slots: Vec<ProgressBar>,
    overflow_bar: ProgressBar,
    active_style: ProgressStyle,
    empty_style: ProgressStyle,
    queue: Vec<(String, String)>,
    fatal: Option<anyhow::Error>,
}

impl ProgressDisplay {
    fn new() -> Self {
        let multi = MultiProgress::new();
        let active_style = ProgressStyle::with_template("{spinner:.green} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", " "]);
        let empty_style = ProgressStyle::with_template("").unwrap();

        let slots: Vec<_> = (0..MAX_VISIBLE)
            .map(|_| {
                let pb = multi.add(ProgressBar::new_spinner());
                pb.set_style(empty_style.clone());
                pb
            })
            .collect();
        let overflow_bar = multi.add(ProgressBar::new_spinner());
        overflow_bar.set_style(empty_style.clone());

        Self {
            multi,
            slots,
            overflow_bar,
            active_style,
            empty_style,
            queue: Vec::new(),
            fatal: None,
        }
    }

    fn handle(&mut self, status: Status) {
        if self.fatal.is_some() {
            return;
        }
        match status {
            Status::Begin { key, msg } => self.push(key, msg),
            Status::Update { key, msg } => {
                if let Some((_, existing)) = self.queue.iter_mut().find(|(k, _)| k == &key) {
                    *existing = msg;
                    self.refresh();
                }
            }
            Status::End { key, msg } => {
                self.remove(&key);
                if let Some(m) = msg {
                    self.multi.println(m).ok();
                }
            }
            Status::Clear => {
                self.queue.clear();
                self.refresh();
            }
            Status::Fatal(msg) => {
                self.multi.println(msg).ok();
                self.fatal = Some(anyhow::anyhow!(""));
            }
            Status::Log(msg) => {
                self.multi.println(msg).ok();
            }
            Status::Stdout(bytes) | Status::Stderr(bytes) => {
                if let Ok(s) = String::from_utf8(bytes) {
                    for line in s.lines() {
                        self.multi.println(line).ok();
                    }
                }
            }
        }
    }

    fn push(&mut self, key: String, msg: String) {
        self.queue.push((key, msg));
        self.refresh();
    }

    fn remove(&mut self, key: &str) {
        self.queue.retain(|(k, _)| k != key);
        self.refresh();
    }

    fn refresh(&self) {
        for (i, slot) in self.slots.iter().enumerate() {
            if let Some((_, msg)) = self.queue.get(i) {
                slot.set_style(self.active_style.clone());
                slot.enable_steady_tick(std::time::Duration::from_millis(80));
                slot.set_message(msg.clone());
            } else {
                slot.set_style(self.empty_style.clone());
                slot.set_message("");
                slot.disable_steady_tick();
            }
        }
        let overflow = self.queue.len().saturating_sub(MAX_VISIBLE);
        if overflow > 0 {
            self.overflow_bar
                .set_style(ProgressStyle::with_template("  {msg}").unwrap());
            self.overflow_bar
                .set_message(format!("and {overflow} more..."));
        } else {
            self.overflow_bar.set_style(self.empty_style.clone());
            self.overflow_bar.set_message("");
        }
    }

    fn finish(self) -> Result<()> {
        for slot in &self.slots {
            slot.finish_and_clear();
        }
        self.overflow_bar.finish_and_clear();
        match self.fatal {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

struct JsonDisplay {
    fatal: bool,
}

impl JsonDisplay {
    fn new() -> Self {
        Self { fatal: false }
    }

    fn handle(&mut self, status: Status) {
        if self.fatal {
            return;
        }
        match status {
            Status::Begin { key, msg } => {
                println!(
                    "{}",
                    serde_json::json!({"event": "begin", "key": key, "msg": msg})
                );
            }
            Status::Update { key, msg } => {
                println!(
                    "{}",
                    serde_json::json!({"event": "update", "key": key, "msg": msg})
                );
            }
            Status::End { key, msg } => match msg {
                Some(msg) => println!(
                    "{}",
                    serde_json::json!({"event": "end", "key": key, "msg": msg})
                ),
                None => println!("{}", serde_json::json!({"event": "end", "key": key})),
            },
            Status::Clear => {}
            Status::Fatal(msg) => {
                println!("{}", serde_json::json!({"event": "fatal", "msg": msg}));
                self.fatal = true;
            }
            Status::Log(msg) => {
                if !msg.is_empty() {
                    println!("{}", serde_json::json!({"event": "log", "msg": msg}));
                }
            }
            Status::Stdout(bytes) => {
                let data = String::from_utf8_lossy(&bytes);
                println!(
                    "{}",
                    serde_json::json!({"event": "stdout", "data": data.as_ref()})
                );
            }
            Status::Stderr(bytes) => {
                let data = String::from_utf8_lossy(&bytes);
                println!(
                    "{}",
                    serde_json::json!({"event": "stderr", "data": data.as_ref()})
                );
            }
        }
    }

    fn finish(self) -> Result<()> {
        if self.fatal {
            Err(anyhow::anyhow!(""))
        } else {
            Ok(())
        }
    }
}

pub fn spawn_progress(mut rx: UnboundedReceiver<Status>) -> tokio::task::JoinHandle<Result<()>> {
    let format = StatusHandle::get().format();
    tokio::spawn(async move {
        match format {
            OutputFormat::Text => {
                let mut display = ProgressDisplay::new();
                while let Some(status) = rx.recv().await {
                    display.handle(status);
                }
                display.finish()
            }
            OutputFormat::Json => {
                let mut display = JsonDisplay::new();
                while let Some(status) = rx.recv().await {
                    display.handle(status);
                }
                display.finish()
            }
        }
    })
}
