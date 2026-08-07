mod store;

pub use store::MemoryAccounts;

use ledger_base::{parts_are_sound, AccountId, AcctHandle, SizedPart, Unit};

/// What the account component charges per unit. Both parts follow the **working set** and nothing
/// else: an account never leaves, so no rate and no lifetime enters here — which is why this is the
/// one component whose size a deployment already knows.
pub const SIZING: &[SizedPart] = &[
    SizedPart::new(
        "account records",
        Unit::Account,
        store::ACCOUNT_BYTES,
        "one account's four durable columns and the id kept beside them",
    ),
    SizedPart::table::<AccountId, AcctHandle>(
        "account index",
        "an account id against its dense handle, resolved once at intake so no later stage re-hashes",
    ),
];

const _: () = assert!(
    parts_are_sound(SIZING),
    "two parts share a name, or one is free"
);
