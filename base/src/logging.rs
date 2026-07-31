use crate::spsc::{self, Consumer, Producer};

/// One log line before it becomes text: the hot path writes numbers only, and formatting,
/// allocation and output all happen on whoever drains the stream.
///
/// This is diagnostics, not the ledger's durable record — that is the consensus log. A
/// `LogEvent` may be dropped.
#[derive(Debug, Clone, Copy)]
pub struct LogEvent {
    pub kind: u16,
    pub at_nanos: u64,
    pub a: u64,
    pub b: u64,
}

pub struct LogSink {
    events: Producer<LogEvent>,
    dropped: u64,
}

impl LogSink {
    /// Never blocks and never grows: if nobody is draining, the event is counted and dropped.
    /// Losing a log line must not slow the ledger down.
    pub fn record(&mut self, kind: u16, at_nanos: u64, a: u64, b: u64) {
        if self
            .events
            .push(LogEvent {
                kind,
                at_nanos,
                a,
                b,
            })
            .is_err()
        {
            self.dropped += 1;
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

pub struct LogStream {
    events: Consumer<LogEvent>,
}

impl LogStream {
    pub fn poll(&self) -> Option<LogEvent> {
        self.events.pop()
    }
}

pub fn log_channel(capacity: usize) -> (LogSink, LogStream) {
    let (sink, stream) = spsc::channel(capacity);
    (
        LogSink {
            events: sink,
            dropped: 0,
        },
        LogStream { events: stream },
    )
}
