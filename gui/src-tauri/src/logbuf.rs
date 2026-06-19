//! Кольцевой буфер логов в памяти — чтобы кнопка «Скопировать лог» в
//! Диагностике отдавала последние события без чтения файлов. Параллельно пишем
//! в stderr через `env_logger` (обёрнут): разработчику видно в консоли, а
//! пользователю доступен снимок.

use std::collections::VecDeque;
use std::sync::Mutex;

use log::{Level, LevelFilter, Log, Metadata, Record};

const CAP: usize = 500;

static BUFFER: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

struct BufferLogger {
    inner: env_logger::Logger,
}

impl Log for BufferLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if !self.inner.enabled(record.metadata()) {
            return;
        }
        // Снимок в буфер (только info и выше — не засоряем trace-ом).
        if record.level() <= Level::Info {
            if let Ok(mut buf) = BUFFER.lock() {
                if buf.len() >= CAP {
                    buf.pop_front();
                }
                buf.push_back(format!("[{}] {}", record.level(), record.args()));
            }
        }
        self.inner.log(record);
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

/// Инициализировать логирование один раз на старте приложения.
pub fn init() {
    let inner = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .format_timestamp_millis()
    .build();
    let max = inner.filter();
    if log::set_boxed_logger(Box::new(BufferLogger { inner })).is_ok() {
        log::set_max_level(max.max(LevelFilter::Info));
    }
}

/// Снимок последних строк лога (для копирования).
#[must_use]
pub fn snapshot() -> String {
    BUFFER
        .lock()
        .map(|b| b.iter().cloned().collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}
