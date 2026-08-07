mod store;

pub use store::MemoryAccounts;

use ledger_base::{parts_are_sound, AccountId, AcctHandle, SizedPart, Unit};

/// What the account component charges per unit. Both parts follow the **working set** and nothing
/// else: an account never leaves, so no rate and no lifetime enters here — which is why this is the
/// one component whose size a deployment already knows.
pub const SIZING: &[SizedPart] = &[
    SizedPart::new("account records", Unit::Account, store::ACCOUNT_BYTES),
    SizedPart::table::<AccountId, AcctHandle>("account index"),
];

const _: () = assert!(
    parts_are_sound(SIZING),
    "two parts share a name, or one is free"
);
