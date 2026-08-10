// The DWA perceptual transfer curve and its inverse, plus the two 64K-entry
// half-float lookup tables the encoder and decoder use to apply them: linear
// values are stored nonlinearly before the DCT and converted back afterwards.

use std::{cell::UnsafeCell, mem::MaybeUninit, sync::OnceLock};

use half::f16;

const TABLE_LEN: usize = u16::MAX as usize + 1;
type TransferTable = [u16; TABLE_LEN];

/// Loader-zeroed storage kept separate from the initialized flag. Combining
/// the 128 KiB array and `OnceLock` in one static makes MSVC emit the whole
/// object into the PE's file-backed `.data` section.
struct TransferTableStorage {
    table: UnsafeCell<MaybeUninit<TransferTable>>,
}

impl TransferTableStorage {
    const fn new() -> Self {
        Self {
            table: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    fn get_or_init(
        &'static self,
        initialized: &'static OnceLock<()>,
        convert: impl FnMut(f16) -> f16,
    ) -> &'static TransferTable {
        initialized.get_or_init(|| self.initialize(convert));
        self.assume_init_ref()
    }

    /// Write every element before the paired `OnceLock` publishes completion.
    ///
    /// The pointer writes avoid creating a 128 KiB stack temporary. This method
    /// is called only by `get_or_init`, whose lock excludes concurrent writers.
    #[allow(unsafe_code, reason = "write-only access is serialized by the paired OnceLock")]
    fn initialize(&self, mut convert: impl FnMut(f16) -> f16) {
        let destination = self.table.get().cast::<u16>();

        for bits in 0..=u16::MAX {
            let converted = convert(f16::from_bits(bits)).to_bits();

            // SAFETY: `destination` points at TABLE_LEN contiguous u16 slots,
            // `bits` visits each valid index exactly once, and the paired
            // OnceLock permits only one initializer at a time.
            unsafe {
                destination.add(bits as usize).write(converted);
            }
        }
    }

    /// Borrow the table only after the paired lock's acquire operation has
    /// observed successful initialization.
    #[allow(unsafe_code, reason = "the paired OnceLock publishes a fully initialized table")]
    fn assume_init_ref(&'static self) -> &'static TransferTable {
        // SAFETY: `get_or_init` calls this only after all TABLE_LEN elements
        // were written and the OnceLock synchronized that write with readers.
        // Initialization never mutates the table again.
        unsafe { (&*self.table.get()).assume_init_ref() }
    }
}

// SAFETY: all mutation is private to `initialize`, which runs under the paired
// OnceLock exactly once. Readers are handed a shared reference only after that
// lock publishes completion, and the table is immutable thereafter.
#[allow(
    unsafe_code,
    reason = "interior mutation is serialized and published by the paired OnceLock"
)]
unsafe impl Sync for TransferTableStorage {}

pub(super) fn to_nonlinear_table() -> &'static TransferTable {
    static TABLE: TransferTableStorage = TransferTableStorage::new();
    static INITIALIZED: OnceLock<()> = OnceLock::new();

    TABLE.get_or_init(&INITIALIZED, dwa_convert_to_nonlinear)
}

fn dwa_convert_to_nonlinear(x: f16) -> f16 {
    // Inverse of the decoder's nonlinear -> linear transfer.
    // Values <= 1 use a power curve; values above 1 follow the exponential tail.
    let value = x.to_f32();
    if !value.is_finite() {
        return f16::ZERO;
    }

    let sign = if value < 0.0 {
        -1.0
    } else {
        1.0
    };
    let value = value.abs();

    let nonlinear = if value <= 1.0 {
        value.powf(1.0 / 2.2)
    } else {
        1.0 + value.ln() / 9.02501329156_f32.ln()
    };

    f16::from_f32(sign * nonlinear)
}

/// The stored nonlinear --> linear lookup table for all half bit patterns
pub(super) fn to_linear_table() -> &'static TransferTable {
    static TABLE: TransferTableStorage = TransferTableStorage::new();
    static INITIALIZED: OnceLock<()> = OnceLock::new();

    TABLE.get_or_init(&INITIALIZED, dwa_convert_to_linear)
}

fn dwa_convert_to_linear(x: f16) -> f16 {
    // Inverse of the encoder's nonlinear transfer.
    let value = x.to_f32();
    if !value.is_finite() {
        return f16::ZERO;
    }

    let sign = if value < 0.0 {
        -1.0
    } else {
        1.0
    };
    let value = value.abs();

    let linear = if value <= 1.0 {
        value.powf(2.2)
    } else {
        // exp(2.2) ^ (value - 1) == exp(2.2 * (value - 1))
        (9.02501329156_f32).powf(value - 1.0)
    };

    f16::from_f32(sign * linear)
}

#[cfg(test)]
mod test {
    use rand::{Rng, SeedableRng};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Barrier,
    };

    use super::*;
    use crate::image::validate_results::ValidateResult;

    const SEED: [u8; 32] = [
        44, 201, 17, 88, 6, 255, 61, 30, 11, 2, 121, 99, 1, 250, 77, 33, 7, 42, 13, 200, 176, 22,
        5, 66, 100, 19, 240, 8, 91, 3, 128, 9,
    ];

    fn assert_curve_roundtrips(value: f32) {
        let x = f16::from_f32(value);
        let restored = dwa_convert_to_linear(dwa_convert_to_nonlinear(x));
        x.assert_approx_equals_result(&restored);
    }

    /// Applying the forward transfer curve and then its inverse must recover
    /// the original value (approximately; the curves round-trip through f16).
    /// Restricted to a moderate magnitude range to avoid the f16-coarseness of
    /// the exponential tail at extreme values.
    #[test]
    fn transfer_curve_roundtrip_scalar() {
        for &value in &[0.0f32, 0.1, 0.25, 0.5, 0.75, 1.0, 2.0, 3.5] {
            assert_curve_roundtrips(value);
            assert_curve_roundtrips(-value);
        }

        let mut random = rand::rngs::StdRng::from_seed(SEED);
        for _ in 0..512 {
            assert_curve_roundtrips(random.gen_range(-4.0f32..4.0));
        }
    }

    /// The two 64K lookup tables are the tabulated forward/inverse curves, so
    /// composing them must be approximately the identity over every finite,
    /// moderate-magnitude half-float bit pattern.
    #[test]
    fn transfer_curve_roundtrip_tables() {
        let to_nonlinear = to_nonlinear_table();
        let to_linear = to_linear_table();

        for bits in 0..=u16::MAX {
            let value = f16::from_bits(bits);
            let magnitude = value.to_f32().abs();
            if !value.to_f32().is_finite() || magnitude > 4.0 {
                continue;
            }

            let restored = f16::from_bits(to_linear[to_nonlinear[bits as usize] as usize]);
            value.assert_approx_equals_result(&restored);
        }
    }

    /// BSS storage must not change a single generated lookup value. Check all
    /// 65,536 half-float bit patterns in both directions against the scalar
    /// conversion functions that populated the original inline tables.
    #[test]
    fn transfer_tables_match_scalar_conversions_exactly() {
        let to_nonlinear = to_nonlinear_table();
        let to_linear = to_linear_table();

        for bits in 0..=u16::MAX {
            let value = f16::from_bits(bits);
            assert_eq!(to_nonlinear[bits as usize], dwa_convert_to_nonlinear(value).to_bits());
            assert_eq!(to_linear[bits as usize], dwa_convert_to_linear(value).to_bits());
        }
    }

    /// Contending readers must publish one fully initialized table and execute
    /// the conversion exactly once per possible half-float bit pattern.
    #[test]
    fn concurrent_first_access_initializes_once() {
        const THREAD_COUNT: usize = 16;

        static TABLE: TransferTableStorage = TransferTableStorage::new();
        static INITIALIZED: OnceLock<()> = OnceLock::new();
        static CONVERSIONS: AtomicUsize = AtomicUsize::new(0);

        let barrier = Barrier::new(THREAD_COUNT);
        let pointers = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(THREAD_COUNT);

            for _ in 0..THREAD_COUNT {
                handles.push(scope.spawn(|| {
                    barrier.wait();
                    let table = TABLE.get_or_init(&INITIALIZED, |value| {
                        CONVERSIONS.fetch_add(1, Ordering::Relaxed);
                        value
                    });

                    table as *const TransferTable as usize
                }));
            }

            handles
                .into_iter()
                .map(|handle| handle.join().expect("table initializer thread must not panic"))
                .collect::<Vec<_>>()
        });

        assert_eq!(CONVERSIONS.load(Ordering::Relaxed), TABLE_LEN);
        assert!(pointers.windows(2).all(|pair| pair[0] == pair[1]));

        let table = TABLE.get_or_init(&INITIALIZED, |value| value);
        for bits in 0..=u16::MAX {
            assert_eq!(table[bits as usize], bits);
        }
    }
}
