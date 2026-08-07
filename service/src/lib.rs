use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ledger_base::ports::{AccountPort, IdempotencyPort, PendingPort, RaftPort};
use ledger_base::{
    channel, Ack, Consumer, LedgerError, LogStream, Producer, Request, ThreadPolicy,
};
use ledger_sequencer::{LogKind, PressureView, Reactor, ReactorConfig, Transport};

#[derive(Debug, Clone, Copy)]
pub struct ServiceConfig {
    /// Everything the state machine itself is tuned by.
    pub reactor: ReactorConfig,
    /// Depth of the two queues between a client and the reactor. Together with the client's own
    /// in-flight limit this decides how much can be waiting to be admitted.
    pub client_queue: usize,
    /// Core to bind the reactor to. Honoured on Linux; elsewhere the thread only gets a
    /// performance-class hint, and the service reports which it got.
    pub pin: Option<usize>,
    /// Print log events to stderr. Off means they are drained and discarded, which costs the
    /// reactor nothing either way.
    pub log_to_stderr: bool,
    /// How long a shutdown waits for work in flight to finish. Uncommitted work is safe to lose —
    /// clients retry — but a batch consensus has already committed still owes an apply, so this
    /// should outlast a consensus round trip by a good margin.
    pub drain_timeout: Duration,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            reactor: ReactorConfig::default(),
            client_queue: 1 << 16,
            pin: None,
            log_to_stderr: false,
            drain_timeout: Duration::from_secs(5),
        }
    }
}

/// What a client is given when it connects: submit here, read acks there. Today both are
/// in-process queues; a network listener would sit on this seam and own one endpoint per
/// connection.
pub struct ClientEndpoint {
    pub requests: Producer<Request>,
    pub acks: Consumer<Ack>,
    /// Why the sequencer stopped admitting, when it has. A full request queue is all a refused submission
    /// can see by itself, and that says nothing about which backlog caused it — this is where the reason
    /// crosses the thread boundary.
    pub pressure: PressureView,
}

/// Asks the service to stop, from anywhere — a signal handler, an admin endpoint, another thread.
///
/// The service installs no signal handler of its own on purpose: which signals mean "stop" is the
/// process owner's decision, so the binary catches them and calls [`StopToken::request`].
#[derive(Clone)]
pub struct StopToken {
    stop: Arc<AtomicBool>,
}

impl StopToken {
    pub fn request(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The running ledger. Starting it starts the reactor loop; the caller never drives ticks.
pub struct LedgerService<A: AccountPort, P: PendingPort, I: IdempotencyPort, R: RaftPort> {
    stop: Arc<AtomicBool>,
    reactor: Option<JoinHandle<Stopped<A, P, I, R>>>,
    log: Option<JoinHandle<()>>,
}

/// The ledger after it has stopped, so final state can be read.
pub struct Stopped<A: AccountPort, P: PendingPort, I: IdempotencyPort, R: RaftPort> {
    pub reactor: Reactor<A, P, I, R>,
    /// The thread placement the reactor actually got.
    pub placement: &'static str,
    /// Whether everything in flight finished before the drain timeout ran out. False means work was
    /// abandoned, which is worth an operator's attention.
    pub drained: bool,
}

impl<A, P, I, R> LedgerService<A, P, I, R>
where
    A: AccountPort + Send + 'static,
    P: PendingPort + Send + 'static,
    I: IdempotencyPort + Send + 'static,
    R: RaftPort + Send + 'static,
{
    pub fn start(
        config: ServiceConfig,
        accounts: A,
        pending: P,
        idem: I,
        raft: R,
    ) -> Result<(Self, ClientEndpoint), LedgerError> {
        let (request_tx, request_rx) = channel(config.client_queue);
        let (ack_tx, ack_rx) = channel(config.client_queue);
        let (reactor, log) = Reactor::new(
            config.reactor,
            Transport {
                requests: request_rx,
                acks: ack_tx,
            },
            accounts,
            pending,
            idem,
            raft,
        )?;

        let pressure = reactor.pressure();
        let stop = Arc::new(AtomicBool::new(false));
        let log_thread = Self::spawn_log_drain(log, Arc::clone(&stop), config.log_to_stderr);
        let reactor_stop = Arc::clone(&stop);
        let reactor_thread = thread::Builder::new()
            .name("reactor".to_owned())
            .spawn(move || Self::serve_then_drain(reactor, reactor_stop, config))
            .map_err(|_| LedgerError::Overloaded)?;

        let service = Self {
            stop,
            reactor: Some(reactor_thread),
            log: log_thread,
        };
        Ok((
            service,
            ClientEndpoint {
                requests: request_tx,
                acks: ack_rx,
                pressure,
            },
        ))
    }

    /// Hand this to whoever decides when the process should stop.
    pub fn stop_token(&self) -> StopToken {
        StopToken {
            stop: Arc::clone(&self.stop),
        }
    }

    /// Requests a stop and waits for the reactor to drain.
    pub fn shutdown(mut self) -> Option<Stopped<A, P, I, R>> {
        self.stop.store(true, Ordering::Relaxed);
        let stopped = self.reactor.take()?.join().ok();
        if let Some(log) = self.log.take() {
            let _ = log.join();
        }
        stopped
    }

    /// Two phases: serve until a stop is requested, then stop admitting and keep ticking until
    /// nothing is in flight. Uncommitted work is safe to abandon — clients retry — but a batch
    /// consensus has already committed still owes an apply, so the drain is what makes shutdown
    /// clean rather than lossy.
    fn serve_then_drain(
        mut reactor: Reactor<A, P, I, R>,
        stop: Arc<AtomicBool>,
        config: ServiceConfig,
    ) -> Stopped<A, P, I, R> {
        let placement = ThreadPolicy::apply(config.pin);
        while !stop.load(Ordering::Relaxed) {
            if !reactor.tick() {
                std::hint::spin_loop();
            }
        }
        reactor.close_intake();
        let deadline = Instant::now() + config.drain_timeout;
        while !reactor.is_quiescent() && Instant::now() < deadline {
            if !reactor.tick() {
                std::hint::spin_loop();
            }
        }
        let drained = reactor.is_quiescent();
        Stopped {
            reactor,
            placement,
            drained,
        }
    }

    /// Formatting a line happens here, never on the reactor.
    fn spawn_log_drain(
        log: LogStream,
        stop: Arc<AtomicBool>,
        to_stderr: bool,
    ) -> Option<JoinHandle<()>> {
        thread::Builder::new()
            .name("log".to_owned())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match log.poll() {
                        Some(event) if to_stderr => eprintln!("{}", LogKind::describe(&event)),
                        Some(_) => {}
                        None => thread::sleep(Duration::from_millis(1)),
                    }
                }
                // The reactor is still draining, so keep reading until it has nothing left to say.
                while let Some(event) = log.poll() {
                    if to_stderr {
                        eprintln!("{}", LogKind::describe(&event));
                    }
                }
            })
            .ok()
    }
}

/// Forgetting to call [`LedgerService::shutdown`] still stops the reactor and still drains it; the
/// final state is simply not handed back.
impl<A, P, I, R> Drop for LedgerService<A, P, I, R>
where
    A: AccountPort,
    P: PendingPort,
    I: IdempotencyPort,
    R: RaftPort,
{
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(reactor) = self.reactor.take() {
            let _ = reactor.join();
        }
        if let Some(log) = self.log.take() {
            let _ = log.join();
        }
    }
}
