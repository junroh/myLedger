use std::hint::black_box;
use std::time::Instant;

use ledger_base::{channel, Request, Transfer, TransferFlags, TxId};
use ledger_benchkit::{BenchOptions, Samples};

const OPS: u64 = 20_000_000;

/// Cost per item of publishing into the ring, one item at a time versus a whole batch with a
/// single release store. The consumer is drained in the same loop so the ring never fills.
struct PushBench {
    batch: usize,
}

impl PushBench {
    fn run(&self) -> std::time::Duration {
        let (producer, consumer) = channel(1 << 14);
        let tx = Transfer {
            id: TxId(1),
            pending_ref: TxId::ABSENT,
            debit_account: ledger_base::AccountId(1),
            credit_account: ledger_base::AccountId(2),
            amount: 1,
            ledger: 1,
            flags: TransferFlags::NONE,
        };
        let started = Instant::now();
        let mut pushed = 0u64;
        while pushed < OPS {
            let taken = if self.batch == 1 {
                producer.push(Request::single(tx, pushed)).is_ok() as usize
            } else {
                producer.push_from(self.batch, |offset| Request {
                    tx,
                    submitted_at_nanos: pushed + offset as u64,
                    end_of_batch: offset + 1 == self.batch,
                })
            };
            pushed += taken as u64;
            for _ in 0..taken {
                black_box(consumer.pop());
            }
        }
        started.elapsed()
    }
}

/// The same work through `rtrb`, to answer whether the hand-rolled ring earns its `unsafe`.
/// `write_chunk_uninit` is its equivalent of publishing a batch with one release store.
struct RtrbBench {
    batch: usize,
}

impl RtrbBench {
    fn run(&self) -> std::time::Duration {
        let (mut producer, mut consumer) = rtrb::RingBuffer::<Request>::new(1 << 14);
        let tx = Transfer {
            id: TxId(1),
            pending_ref: TxId::ABSENT,
            debit_account: ledger_base::AccountId(1),
            credit_account: ledger_base::AccountId(2),
            amount: 1,
            ledger: 1,
            flags: TransferFlags::NONE,
        };
        let started = Instant::now();
        let mut pushed = 0u64;
        while pushed < OPS {
            let taken = if self.batch == 1 {
                producer.push(Request::single(tx, pushed)).is_ok() as usize
            } else {
                match producer.write_chunk_uninit(self.batch) {
                    Ok(chunk) => chunk.fill_from_iter((0..self.batch).map(|offset| Request {
                        tx,
                        submitted_at_nanos: pushed + offset as u64,
                        end_of_batch: offset + 1 == self.batch,
                    })),
                    Err(_) => 0,
                }
            };
            pushed += taken as u64;
            for _ in 0..taken {
                black_box(consumer.pop().ok());
            }
        }
        started.elapsed()
    }
}

fn main() {
    let options = BenchOptions::from_args();
    options.announce();
    for batch in [1usize, 8, 64, 512, 4096] {
        let mut ours = Samples::new(format!("ours   publish (batch {batch})"), OPS);
        let mut theirs = Samples::new(format!("rtrb   publish (batch {batch})"), OPS);
        for _ in 0..options.repeat {
            ours.add(PushBench { batch }.run());
            theirs.add(RtrbBench { batch }.run());
        }
        ours.report();
        theirs.report();
    }
}
