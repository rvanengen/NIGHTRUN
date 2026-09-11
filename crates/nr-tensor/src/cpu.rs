//! CPU feature detection, per architecture, with the same cached shape.
//!
//! x86_64: AVX2/FMA/F16C via CPUID + XCR0 (the boot layer enables YMM
//! state). aarch64: NEON is baseline; DotProd/FP16 probed from the ID
//! registers (readable at the EL where UEFI runs).

use core::sync::atomic::{AtomicU8, Ordering};

static CACHED: AtomicU8 = AtomicU8::new(0);

const PROBED: u8 = 1;
const F_A: u8 = 2; // x86: AVX2   | aarch64: dotprod
const F_B: u8 = 4; // x86: FMA    | aarch64: fp16 arith
#[cfg(target_arch = "x86_64")]
const F_C: u8 = 8; // x86: F16C

fn cached_bits() -> u8 {
    let mut bits = CACHED.load(Ordering::Relaxed);
    if bits & PROBED == 0 {
        bits = probe();
        CACHED.store(bits, Ordering::Relaxed);
    }
    bits
}

#[cfg(target_arch = "x86_64")]
mod imp {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Features {
        pub avx2: bool,
        pub fma: bool,
        pub f16c: bool,
    }

    pub fn features() -> Features {
        let bits = cached_bits();
        Features {
            avx2: bits & F_A != 0,
            fma: bits & F_B != 0,
            f16c: bits & F_C != 0,
        }
    }

    /// True when the AVX2+FMA kernel paths can be used.
    pub fn fast_path() -> bool {
        let f = features();
        f.avx2 && f.fma
    }

    /// True when the fast f16<->f32 conversion path can be used.
    pub fn fast_f16() -> bool {
        let f = features();
        f.f16c && f.avx2 && f.fma
    }

    pub fn simd_label() -> &'static str {
        if fast_path() {
            "AVX2+FMA kernels"
        } else {
            "scalar kernels (no AVX2)"
        }
    }

    pub(super) fn probe() -> u8 {
        use core::arch::x86_64::{__cpuid, __cpuid_count};

        let mut bits = PROBED;
        let leaf1 = __cpuid(1);
        let osxsave = leaf1.ecx & (1 << 27) != 0;
        let avx = leaf1.ecx & (1 << 28) != 0;
        if !(osxsave && avx) {
            return bits;
        }
        // Check the OS/firmware enabled YMM state (XCR0 bits 1|2).
        let xcr0: u64 = unsafe {
            let lo: u32;
            let hi: u32;
            core::arch::asm!("xgetbv", in("ecx") 0u32, out("eax") lo, out("edx") hi, options(nostack, nomem));
            ((hi as u64) << 32) | lo as u64
        };
        if xcr0 & 0b110 != 0b110 {
            return bits;
        }
        if leaf1.ecx & (1 << 12) != 0 {
            bits |= F_B;
        }
        if leaf1.ecx & (1 << 29) != 0 {
            bits |= F_C;
        }
        let leaf7 = __cpuid_count(7, 0);
        if leaf7.ebx & (1 << 5) != 0 {
            bits |= F_A;
        }
        bits
    }
}

#[cfg(target_arch = "aarch64")]
mod imp {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Features {
        /// FEAT_DotProd (sdot/udot) — present on Cortex-A76 (Pi 5).
        pub dotprod: bool,
        /// FEAT_FP16 arithmetic (conversions are ARMv8.0 baseline).
        pub fp16: bool,
    }

    pub fn features() -> Features {
        let bits = cached_bits();
        Features {
            dotprod: bits & F_A != 0,
            fp16: bits & F_B != 0,
        }
    }

    /// NEON is architecturally baseline on aarch64 (and the UEFI target
    /// is hard-float): the NEON kernels are always usable.
    pub fn fast_path() -> bool {
        true
    }

    /// f16<->f32 conversion instructions (FCVTL) are ARMv8.0 baseline.
    pub fn fast_f16() -> bool {
        true
    }

    pub fn simd_label() -> &'static str {
        if features().dotprod {
            "NEON+DOTPROD kernels"
        } else {
            "NEON kernels"
        }
    }

    pub(super) fn probe() -> u8 {
        let mut bits = PROBED;
        // Host OSes virtualize or restrict ID registers differently. Rust's
        // standard-library detector uses the platform-supported mechanism
        // (including Darwin's sysctl path), so use it whenever std exists.
        #[cfg(feature = "std")]
        {
            if std::arch::is_aarch64_feature_detected!("dotprod") {
                bits |= F_A;
            }
            if std::arch::is_aarch64_feature_detected!("fp16") {
                bits |= F_B;
            }
            return bits;
        }

        #[cfg(not(feature = "std"))]
        {
            // ID_AA64ISAR0_EL1.DP (bits 47:44) => FEAT_DotProd.
            let isar0: u64;
            unsafe {
                core::arch::asm!("mrs {}, ID_AA64ISAR0_EL1", out(reg) isar0, options(nostack, nomem))
            };
            if (isar0 >> 44) & 0xf >= 1 {
                bits |= F_A;
            }
            // ID_AA64PFR0_EL1.FP (bits 19:16) == 1 => FEAT_FP16 arithmetic.
            let pfr0: u64;
            unsafe {
                core::arch::asm!("mrs {}, ID_AA64PFR0_EL1", out(reg) pfr0, options(nostack, nomem))
            };
            if (pfr0 >> 16) & 0xf == 1 {
                bits |= F_B;
            }
            bits
        }
    }
}

use imp::probe;
pub use imp::{fast_f16, fast_path, features, simd_label, Features};
