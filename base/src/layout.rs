/// The line size of the machine being built for: aligning to anything else pays memory for nothing.
/// Padding a 64-byte-line machine to 128 would buy one thing — x86 pulls adjacent lines in pairs, so a
/// pair of padded atomics can still share a fetch — and cost double the padding for a benefit nobody
/// here can measure. Claims are checked against every size in `SUPPORTED_LINES`, so portability is a
/// build-time fact rather than a padding choice.
#[cfg(target_arch = "aarch64")]
pub const CACHE_LINE: usize = 128;
#[cfg(not(target_arch = "aarch64"))]
pub const CACHE_LINE: usize = 64;

/// Line sizes the ledger targets. Claims are checked against all of them, so a claim that holds
/// here holds on the other targets too.
pub const SUPPORTED_LINES: [usize; 2] = [64, 128];

/// Declares a struct that starts on a cache line and occupies whole lines. Use it for state
/// that is reached at random and is too big to fit inside one line, and for values that must not
/// share a line across threads. State that *does* fit in a line should instead be sized to divide
/// it — see `never_straddles` — which is cheaper in memory and just as free of straddling.
///
/// `repr(align(..))` only accepts literals, so this macro is the single place the alignment is
/// written; the emitted const assertions turn a layout regression into a build failure.
#[macro_export]
macro_rules! cache_aligned {
    (
        $(#[$attr:meta])*
        $vis:vis struct $name:ident {
            $($(#[$field_attr:meta])* $fvis:vis $field:ident : $ty:ty),* $(,)?
        }
    ) => {
        $(#[$attr])*
        #[repr(align(128))]
        $vis struct $name { $($(#[$field_attr])* $fvis $field : $ty),* }

        const _: () = {
            assert!(
                ::core::mem::size_of::<$name>() % $crate::CACHE_LINE == 0,
                concat!(stringify!($name), " no longer occupies whole cache lines")
            );
            assert!(::core::mem::align_of::<$name>() == $crate::CACHE_LINE);
        };
    };
}

/// Keeps a value written by one thread off the lines its neighbours read.
#[repr(align(128))]
#[derive(Debug, Default)]
pub struct CachePadded<T>(T);

impl<T> CachePadded<T> {
    pub const fn new(value: T) -> Self {
        Self(value)
    }
}

impl<T> core::ops::Deref for CachePadded<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

/// How a type is expected to sit against the cache line. This states a property, it does not create
/// one: size and alignment come from the struct, and the build checks that the claim holds.
#[derive(Debug, Clone, Copy)]
pub enum LineFit {
    /// Fits in one line and never crosses a boundary: `CACHE_LINE % size == 0` and
    /// `align >= size`. Nothing is padded. Use it for small state reached at random.
    Inside,
    /// Starts on a line and fills whole ones: `size % CACHE_LINE == 0` and `align >= CACHE_LINE`.
    /// This pads. Use it for random-access state too big for one line; the declared size is what
    /// then catches silent growth.
    WholeLines,
    /// May cross a boundary, deliberately. The reason is part of the declaration.
    Straddles(&'static str),
}

/// Streamed in order: consecutive access touches every line anyway, so only the size matters.
pub const STREAMED: &str = "streamed in order, never reached at random";
pub const ON_PURPOSE: &str = "packed on purpose; footprint beats straddling, measured";

#[derive(Debug, Clone, Copy)]
pub struct TypeLayout {
    pub name: &'static str,
    pub size: usize,
    pub align: usize,
    /// What the declaration says; equal to `size` or the build fails.
    pub expected: usize,
    pub fit: LineFit,
}

impl TypeLayout {
    pub const fn of<T>(name: &'static str, expected: usize, fit: LineFit) -> Self {
        Self {
            name,
            size: core::mem::size_of::<T>(),
            align: core::mem::align_of::<T>(),
            expected,
            fit,
        }
    }

    pub const fn matches_expected_size(&self) -> bool {
        self.size == self.expected
    }

    /// Whether the type actually sits the way it claims to, on every line size the ledger targets.
    pub const fn honours_fit(&self) -> bool {
        let mut index = 0;
        while index < SUPPORTED_LINES.len() {
            if !self.honours_fit_on(SUPPORTED_LINES[index]) {
                return false;
            }
            index += 1;
        }
        true
    }

    const fn honours_fit_on(&self, line: usize) -> bool {
        match self.fit {
            LineFit::Inside => {
                self.size > 0 && self.size <= line && line.is_multiple_of(self.size) && self.align >= self.size
            }
            LineFit::WholeLines => self.size.is_multiple_of(line) && self.align >= line,
            LineFit::Straddles(_) => true,
        }
    }

    /// How many share a line, or zero when one does not fit. Line size is a property of the machine, so
    /// packing is too: a 40-byte record is three to a 128-byte line and one to a 64-byte line, and that
    /// difference is misses per operation on hardware nobody here can measure.
    pub const fn per_line_on(&self, line: usize) -> usize {
        if self.size == 0 || self.size > line {
            0
        } else {
            line / self.size
        }
    }

    pub const fn per_line(&self) -> usize {
        self.per_line_on(CACHE_LINE)
    }

    pub const fn fit_name(&self) -> &'static str {
        match self.fit {
            LineFit::Inside => "inside",
            LineFit::WholeLines => "whole-lines",
            LineFit::Straddles(_) => "straddles",
        }
    }
}

/// Declares a struct's expected size and line fit next to the struct, and fails the build there if
/// either stops holding. `size_of` supplies the real size; the declared one must match, so a field
/// added without thought does not slip through.
#[macro_export]
macro_rules! layout_claim {
    ($name:ident: $type:ty, size = $size:expr, $fit:expr) => {
        pub const $name: $crate::TypeLayout =
            $crate::TypeLayout::of::<$type>(stringify!($type), $size, $fit);

        const _: () = assert!(
            $name.matches_expected_size(),
            concat!(stringify!($type), " changed size; update the declaration deliberately")
        );
        const _: () = assert!(
            $name.honours_fit(),
            concat!(stringify!($type), " no longer sits against the cache line as it claims")
        );
    };
}

/// Checked where a crate gathers its claims.
pub const fn layouts_are_sound(types: &[TypeLayout]) -> bool {
    let mut index = 0;
    while index < types.len() {
        if !types[index].matches_expected_size() || !types[index].honours_fit() {
            return false;
        }
        index += 1;
    }
    true
}

/// Gathered for reporting; each claim is declared where its struct is.
pub const HOT_TYPES: &[TypeLayout] = &[
    crate::transfer::LAYOUT,
    crate::effect::LAYOUT,
    crate::protocol::REQUEST_LAYOUT,
    crate::protocol::ACK_LAYOUT,
    crate::ports::account::LAYOUT,
];

const _: () = assert!(layouts_are_sound(HOT_TYPES), "a watched type broke its layout contract");

#[cfg(test)]
mod tests {
    use super::*;

    /// A claim is checked against every line size the ledger targets, so a size that fits one and
    /// straddles another is not `Inside`. This is the check the build-time assertion runs.
    #[test]
    fn a_fit_holds_only_when_it_holds_on_every_target_line_size() {
        // The fields are the point: they are the bytes being measured.
        #[repr(align(32))]
        #[allow(dead_code)]
        struct Snug([u8; 32]);
        let snug = TypeLayout::of::<Snug>("Snug", 32, LineFit::Inside);
        assert!(snug.honours_fit(), "32 divides 64 and 128, and the alignment says so");

        // 24 bytes divides neither line size: an array of them straddles.
        #[allow(dead_code)]
        struct Odd([u8; 24]);
        let odd = TypeLayout::of::<Odd>("Odd", 24, LineFit::Inside);
        assert!(!odd.honours_fit());
        assert!(TypeLayout::of::<Odd>("Odd", 24, LineFit::Straddles("measured")).honours_fit());

        // Unaligned, so where an array puts it is up to the allocator.
        #[allow(dead_code)]
        struct Loose([u32; 8]);
        assert!(!TypeLayout::of::<Loose>("Loose", 32, LineFit::Inside).honours_fit());
    }

    /// `WholeLines` has to hold on the 64-byte target too, which means whole 128-byte units.
    #[test]
    fn whole_lines_means_whole_lines_on_both_targets() {
        cache_aligned! {
            #[allow(dead_code)]
            struct Padded {
                value: u64,
            }
        }
        let padded = TypeLayout::of::<Padded>("Padded", 128, LineFit::WholeLines);
        assert!(padded.honours_fit());
        assert_eq!(padded.size, 128, "the macro pads to a whole line");
    }

    /// The declared size is a budget: a type that grew must be looked at rather than accepted.
    #[test]
    fn a_declared_size_that_no_longer_matches_is_caught() {
        #[allow(dead_code)]
        struct Grown([u8; 40]);
        let claim = TypeLayout::of::<Grown>("Grown", 32, LineFit::Straddles("measured"));
        assert!(!claim.matches_expected_size());
        assert!(!layouts_are_sound(&[claim]));
        assert!(layouts_are_sound(HOT_TYPES), "the real claims hold");
    }
}
