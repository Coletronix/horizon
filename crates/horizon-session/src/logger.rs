//! Log lines on stderr: `horizon-session LEVEL message`. `HORIZON_SESSION_LOG=debug` shows more.
use log::{Level, LevelFilter, Log, Metadata, Record};

struct Stderr(LevelFilter);

impl Log for Stderr {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.0
    }

    fn log(&self, record: &Record) {
        // Libraries' own chatter only from errors up (smithay logs protocol details as warnings).
        let ours = record.target().starts_with("horizon_session");
        if self.enabled(record.metadata()) && (ours || record.level() <= Level::Error) {
            eprintln!("horizon-session {} {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

pub fn init() {
    let level = match std::env::var("HORIZON_SESSION_LOG").as_deref() {
        Ok("debug") => LevelFilter::Debug,
        Ok("trace") => LevelFilter::Trace,
        Ok("warn") => LevelFilter::Warn,
        _ => LevelFilter::Info,
    };
    if log::set_boxed_logger(Box::new(Stderr(level))).is_ok() {
        log::set_max_level(level);
    }
}
