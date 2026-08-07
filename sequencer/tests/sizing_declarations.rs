//! Every structure a running node reports is one a sizing model can price.
//!
//! The unit costs in `SIZING` are `size_of`, so they cannot be stale. **What can go stale is the list.**
//! A structure added to a component and not declared reads as zero bytes to whatever is summing, and a
//! sizing answer that is short by one structure looks exactly like one that is right — which is the
//! failure the declaration exists to prevent, and the only one a test can catch.
//!
//! So this drives a real node, takes every part every component reports, and demands a declaration for
//! each. Not a unit test: the parts only exist once the components are built, and half of them are on
//! another thread.

mod harness;

use harness::{Harness, ALICE, BOB, FUNDING};
use ledger_base::SizedPart;

fn declared() -> Vec<&'static SizedPart> {
    [
        ledger_sequencer::SIZING,
        ledger_account::SIZING,
        ledger_idempotency::SIZING,
        ledger_pending::SIZING,
        ledger_raft::SIZING,
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Both directions, because they fail differently. An undeclared part makes a sizing answer silently
/// small; a declared part nothing reports is a name a model matches against and never finds, which
/// reads as a structure that costs nothing.
#[test]
fn every_reported_structure_has_a_declared_unit_cost() {
    let mut harness = Harness::new();
    // Enough traffic that the parts which only appear under load are there to be found: an empty
    // reactor reports no overlay entries and no blocks.
    harness.fund(ALICE, FUNDING * 100);
    for _ in 0..16 {
        harness.hold(ALICE, BOB, 1);
    }

    let reported: Vec<&'static str> = [
        harness.reactor.footprint(),
        harness.reactor.accounts().footprint(),
        harness.reactor.idem().footprint(),
        harness.reactor.pending().footprint(),
        harness.reactor.raft().footprint(),
    ]
    .iter()
    .flat_map(|footprint| footprint.parts().iter().map(|part| part.name))
    .collect();

    let declared = declared();
    for name in &reported {
        assert!(
            declared.iter().any(|part| part.name == *name),
            "`{name}` is reported but not declared in any crate's SIZING, \
             so a sizing model would price it at nothing"
        );
    }

    // Three parts a memory-backed harness cannot produce, each for a stated reason rather than because
    // the check was in the way. Everything else has to arrive, which is what catches a rename: a
    // declared name nothing reports is a term a model looks for and never finds.
    const ONLY_ELSEWHERE: [&str; 3] = [
        // Disk, so no footprint reports it at all.
        "pending record",
        // Both exist only when the volume is a directory: a memory backing has neither a lane thread
        // nor a read pool to hold blocks for.
        "volume write lane",
        "volume read pool",
    ];
    for part in &declared {
        assert!(
            ONLY_ELSEWHERE.contains(&part.name) || reported.contains(&part.name),
            "`{}` is declared but nothing reports it, so a model would match a name that never arrives",
            part.name
        );
    }
}

/// The index is a cuckoo table, not a hash table, and pricing it as one read 138MB where it holds 17.
/// The unit is the slot the load factor is already measured against, which is what makes this checkable
/// against the number the run prints beside it.
#[test]
fn the_hold_index_is_priced_by_its_slots_and_not_as_a_hash_table() {
    let harness = Harness::new();
    let footprint = harness.reactor.pending().footprint();
    let index = footprint
        .parts()
        .iter()
        .find(|part| part.name == "engine index")
        .expect("the engine reports its index");

    assert_eq!(
        index.bytes,
        index.capacity * ledger_pending::SLOT_BYTES,
        "the index allocates the slots it was told to and nothing more"
    );
    assert!(
        index.exact,
        "a cuckoo table's allocation is exact; only a hash table's has to be derived"
    );
}
