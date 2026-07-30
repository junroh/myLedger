use ledger_base::LogEvent;

/// The kinds of log event the sequencer emits. The hot path records numbers only; naming and
/// formatting happen wherever the stream is drained.
pub struct LogKind;

impl LogKind {
    pub const STARTED: u16 = 1;
    pub const SEQ_GAP: u16 = 2;
    pub const LANE_QUARANTINED: u16 = 3;
    pub const LANE_RELEASED: u16 = 4;
    pub const FAIL_STOP: u16 = 5;
    pub const COMMIT_FAILED: u16 = 6;
    pub const INTAKE_PAUSED: u16 = 7;
    pub const INTAKE_RESUMED: u16 = 8;
    pub const CHAIN_ABORTED: u16 = 9;
    pub const HOLDS_EVICTED: u16 = 10;
    pub const COMMIT_OUT_OF_ORDER: u16 = 11;
    pub const APPLY_FAILED: u16 = 12;
    pub const INVARIANT_BROKEN: u16 = 13;

    pub fn describe(event: &LogEvent) -> String {
        let LogEvent { kind, at_nanos, a, b } = *event;
        let micros = at_nanos / 1_000;
        match kind {
            Self::STARTED => format!("[{micros}us] sequencer started: slots={a} batch_size={b}"),
            Self::SEQ_GAP => {
                format!("[{micros}us] contract-1 violated: lane={a} expected seq={b}")
            }
            Self::LANE_QUARANTINED => format!("[{micros}us] lane quarantined: lane={a}"),
            Self::LANE_RELEASED => format!("[{micros}us] lane released: lane={a}"),
            Self::FAIL_STOP => format!("[{micros}us] fail-stop: quarantined lanes={a}"),
            Self::COMMIT_FAILED => {
                format!("[{micros}us] consensus refused batch={a} effects={b}")
            }
            Self::INTAKE_PAUSED => {
                format!("[{micros}us] intake paused: acks={a} pending writes={b}")
            }
            Self::INTAKE_RESUMED => format!("[{micros}us] intake resumed"),
            Self::CHAIN_ABORTED => {
                format!("[{micros}us] linked chain aborted at batch boundary: chain={a} legs={b}")
            }
            Self::HOLDS_EVICTED => format!("[{micros}us] holds evicted: {a} (live={b})"),
            Self::COMMIT_OUT_OF_ORDER => {
                format!("[{micros}us] consensus commit out of order: expected={a} got={b}")
            }
            Self::INVARIANT_BROKEN => {
                format!("[{micros}us] the sequencer's own bookkeeping broke: reason={a} tick={b}")
            }
            Self::APPLY_FAILED => {
                format!("[{micros}us] committed effect could not be applied: batch={a} error={b}")
            }
            other => format!("[{micros}us] unknown event {other}: a={a} b={b}"),
        }
    }
}
