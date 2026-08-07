use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A test's hold on what a stand-in component answers with.
///
/// **This is what replaces a latency in a test that has to see a request still waiting.** A component
/// answers on a thread of its own, so its answers arrive whether or not the test ticks — and a test that
/// arranged an interleaving by giving that thread five milliseconds of work was arranging it with the
/// scheduler. On a busy machine the scheduler disagreed: three assertions in `lane_ordering` failed a few
/// times in every hundred concurrent runs, always because the answer landed earlier in the tick sequence
/// than the delay implied. The state the test wanted was never a duration.
///
/// **Permits rather than a switch**, and that half was found by measuring rather than by reasoning: two
/// answers released together are taken in whatever order the reactor's tick takes them, so a test that
/// means to send one *ahead* of another has to be able to send exactly one. Letting both go and hoping
/// reproduces the problem the gate exists to remove.
///
/// `waiting` is the other half: a test that needs two answers queued before it lets either go has to be
/// able to see that they are. Published by the component while the gate is closed, so an ordinary run
/// pays one comparison and no store.
///
/// It lives here because it is stand-in machinery and a real component brings none of it — the same
/// reason the lane ordering and the worker loop are here. Two components use it, which is also why it is
/// one type: "held until told" is the same thing whether what is held is a pending reply or a commit.
#[derive(Clone, Default)]
pub struct AnswerGate(Arc<GateState>);

struct GateState {
    /// Answers the component may still send. `usize::MAX` is an open gate, which is what every run but a
    /// test's has.
    permits: AtomicUsize,
    waiting: AtomicUsize,
}

impl Default for GateState {
    fn default() -> Self {
        Self {
            permits: AtomicUsize::new(usize::MAX),
            waiting: AtomicUsize::new(0),
        }
    }
}

impl AnswerGate {
    /// Nothing more leaves until it is let through. The component keeps working; only what would leave
    /// is kept.
    pub fn hold(&self) {
        self.0.permits.store(0, Ordering::Relaxed);
    }

    /// Lets exactly `answers` out and closes again.
    pub fn let_through(&self, answers: usize) {
        self.0.permits.store(answers, Ordering::Relaxed);
    }

    /// Open again, for good.
    pub fn release(&self) {
        self.0.permits.store(usize::MAX, Ordering::Relaxed);
    }

    /// Answers the component has ready and cannot send.
    pub fn waiting(&self) -> usize {
        self.0.waiting.load(Ordering::Relaxed)
    }

    /// Whether the gate is out of the way entirely, which is every run but a test's. The component asks
    /// this before publishing `waiting`, so an open gate costs a load and nothing else.
    pub fn is_open(&self) -> bool {
        self.0.permits.load(Ordering::Relaxed) == usize::MAX
    }

    /// Whether one more answer may leave. Asked before an answer is taken and `spend` after it is, so a
    /// round that finds nothing to send does not burn a permit.
    pub fn may_send(&self) -> bool {
        self.0.permits.load(Ordering::Relaxed) > 0
    }

    pub fn spend(&self) {
        let left = self.0.permits.load(Ordering::Relaxed);
        if left != usize::MAX {
            self.0
                .permits
                .store(left.saturating_sub(1), Ordering::Relaxed);
        }
    }

    pub fn note_waiting(&self, answers: usize) {
        self.0.waiting.store(answers, Ordering::Relaxed);
    }
}
