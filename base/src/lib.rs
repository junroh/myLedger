#![deny(unsafe_code)]

/// The interfaces the components implement, kept a namespace because every component reaches for
/// exactly one of them.
pub mod ports;

#[allow(unsafe_code)]
mod affinity;
mod effect;
mod error;
mod footprint;
mod hash;
mod ids;
mod layout;
mod logging;
mod prng;
mod pool;
mod protocol;
mod signals;
#[allow(unsafe_code)]
mod spsc;
mod time;
mod transfer;

pub use affinity::ThreadPolicy;
pub use effect::{ColumnDelta, Effect, EffectKind};
pub use error::LedgerError;
pub use footprint::{Footprint, MapGauge, Part, Peak};
pub use hash::{FxBuildHasher, FxHashMap};
pub use ids::{AccountId, AcctHandle, Amount, BudgetGroup, LinkedChainId, Seq, TxId, MAX_AMOUNT};
pub use prng::Prng;
pub use layout::{layouts_are_sound, LineFit, TypeLayout, CACHE_LINE, HOT_TYPES, SUPPORTED_LINES};
pub use spsc::{channel, Consumer, Producer, StagedProducer};
pub use logging::{log_channel, LogEvent, LogSink, LogStream};
pub use pool::BufferPool;
pub use signals::Signals;
pub use protocol::{Ack, AckOutcome, Request};
pub use time::{Clock, ManualClock, SystemClock};
pub use transfer::{Transfer, TransferFlags, TransferKind};
