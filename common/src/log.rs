//! ログレベル付きの最小限のロガー。
//!
//! `Logger` は任意の `core::fmt::Write` 実装（シリアルポート等）に対して
//! レベルフィルタリングとフォーマットを行う、ハードウェアに依存しない
//! 純粋ロジックである。ホスト上で `cargo test` により検証する。

use core::fmt::{self, Write};

/// ログレベル。`Trace` が最も詳細（重大度が低い）、`Error` が最も重大。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

/// `writer` へレベルフィルタリング付きでログ行を書き出す。
///
/// `min_level` 未満のメッセージは破棄される。書き込み自体の失敗
/// （フォーマットエラー等）はログ出力を落とすためだけの理由でパニック
/// させたくないため、無視する。
pub struct Logger<W> {
    writer: W,
    min_level: LogLevel,
}

impl<W: Write> Logger<W> {
    pub fn new(writer: W, min_level: LogLevel) -> Self {
        Self { writer, min_level }
    }

    pub fn log(&mut self, level: LogLevel, args: fmt::Arguments<'_>) {
        if level < self.min_level {
            return;
        }
        let _ = writeln!(self.writer, "[{}] {args}", level.label());
    }

    pub fn trace(&mut self, args: fmt::Arguments<'_>) {
        self.log(LogLevel::Trace, args);
    }

    pub fn debug(&mut self, args: fmt::Arguments<'_>) {
        self.log(LogLevel::Debug, args);
    }

    pub fn info(&mut self, args: fmt::Arguments<'_>) {
        self.log(LogLevel::Info, args);
    }

    pub fn warn(&mut self, args: fmt::Arguments<'_>) {
        self.log(LogLevel::Warn, args);
    }

    pub fn error(&mut self, args: fmt::Arguments<'_>) {
        self.log(LogLevel::Error, args);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_ordering_trace_lowest_error_highest() {
        assert!(LogLevel::Trace < LogLevel::Debug);
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Error);
    }

    #[test]
    fn messages_below_min_level_are_dropped() {
        let mut logger = Logger::new(String::new(), LogLevel::Info);
        logger.debug(format_args!("hidden"));
        logger.info(format_args!("visible"));
        assert_eq!(logger.writer, "[INFO] visible\n");
    }

    #[test]
    fn messages_at_or_above_min_level_are_kept() {
        let mut logger = Logger::new(String::new(), LogLevel::Warn);
        logger.warn(format_args!("careful"));
        logger.error(format_args!("boom"));
        assert_eq!(logger.writer, "[WARN] careful\n[ERROR] boom\n");
    }

    #[test]
    fn formats_with_level_label_prefix() {
        let mut logger = Logger::new(String::new(), LogLevel::Trace);
        logger.trace(format_args!("value = {}", 42));
        assert_eq!(logger.writer, "[TRACE] value = 42\n");
    }
}
