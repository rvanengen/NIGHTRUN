//! NEON kernel correctness (aarch64 only; run cross-compiled under
//! qemu-user). The NEON dots are bit-identical to the scalar reference
//! by construction (exact integer sub-dots + replicated f32 order), so
//! everything except the f16 helpers asserts exact equality.

#![cfg(target_arch = "aarch64")]

use nr_tensor::kquant::{self, quantize_q4k_ref, quantize_q6k_ref, quantize_q8k, BlockQ8K, QK_K};
use nr_tensor::neon;
use nr_tensor::q8::{self, BlockQ8_0, QK8_0};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn f32(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 23) as f32 * 2.0 - 1.0
    }

    fn vec(&mut self, n: usize) -> Vec<f32> {
        (0..n).map(|_| self.f32()).collect()
    }

    /// Huge dynamic range, negatives, zeros, spikes.
    fn adversarial(&mut self, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| match self.next() % 7 {
                0 => 0.0,
                1 => 100.0 * self.f32(),
                2 => 1e-4 * self.f32(),
                3 => -50.0 + self.f32(),
                _ => self.f32() * (1 + i % 13) as f32,
            })
            .collect()
    }
}

fn q8(v: &[f32]) -> Vec<BlockQ8_0> {
    let mut out = vec![
        BlockQ8_0 {
            d: 0,
            qs: [0; QK8_0]
        };
        v.len() / QK8_0
    ];
    q8::quantize(v, &mut out);
    out
}

fn q8k(v: &[f32]) -> Vec<BlockQ8K> {
    let mut out = vec![
        BlockQ8K {
            d: 0.0,
            qs: [0; QK_K],
            bsums: [0; QK_K / 16]
        };
        v.len() / QK_K
    ];
    quantize_q8k(v, &mut out);
    out
}

#[test]
fn neon_q8_bit_equals_scalar() {
    let mut rng = Rng(11);
    for blocks in [1usize, 5, 40] {
        let w = q8(&rng.adversarial(blocks * QK8_0));
        let x = q8(&rng.vec(blocks * QK8_0));
        let s = nr_tensor::kernels::dot_q8_scalar(&w, &x);
        let v = unsafe { neon::kernels::dot_q8(&w, &x) };
        assert_eq!(s, v, "blocks={blocks}");
        let xs4: Vec<_> = (0..4)
            .map(|i| {
                q8(&rng
                    .vec(blocks * QK8_0)
                    .iter()
                    .map(|f| f * (i + 1) as f32)
                    .collect::<Vec<_>>())
            })
            .collect();
        let lanes = [&xs4[0][..], &xs4[1][..], &xs4[2][..], &xs4[3][..]];
        let v4 = unsafe { neon::kernels::dot_q8_x4(&w, lanes) };
        for (i, lane) in lanes.iter().enumerate() {
            assert_eq!(
                v4[i],
                nr_tensor::kernels::dot_q8_scalar(&w, lane),
                "x4 lane {i}"
            );
        }
    }
}

#[test]
fn neon_q4k_bit_equals_scalar() {
    let mut rng = Rng(21);
    for blocks in [1usize, 3, 10] {
        let w: Vec<_> = rng
            .adversarial(blocks * QK_K)
            .chunks_exact(QK_K)
            .map(quantize_q4k_ref)
            .collect();
        let x = q8k(&rng.vec(blocks * QK_K));
        let s = kquant::dot_q4k_scalar(&w, &x);
        let v = unsafe { neon::kquant::dot_q4k(&w, &x) };
        assert_eq!(s, v, "blocks={blocks}");
        let xs4: Vec<_> = (0..4)
            .map(|_| q8k(&rng.adversarial(blocks * QK_K)))
            .collect();
        let lanes = [&xs4[0][..], &xs4[1][..], &xs4[2][..], &xs4[3][..]];
        let v4 = unsafe { neon::kquant::dot_q4k_x4(&w, lanes) };
        for (i, lane) in lanes.iter().enumerate() {
            assert_eq!(v4[i], kquant::dot_q4k_scalar(&w, lane), "x4 lane {i}");
        }
    }
}

#[test]
fn neon_q6k_bit_equals_scalar() {
    let mut rng = Rng(31);
    for blocks in [1usize, 3, 10] {
        let w: Vec<_> = rng
            .adversarial(blocks * QK_K)
            .chunks_exact(QK_K)
            .map(quantize_q6k_ref)
            .collect();
        let x = q8k(&rng.vec(blocks * QK_K));
        let s = kquant::dot_q6k_scalar(&w, &x);
        let v = unsafe { neon::kquant::dot_q6k(&w, &x) };
        assert_eq!(s, v, "blocks={blocks}");
        let xs4: Vec<_> = (0..4)
            .map(|_| q8k(&rng.adversarial(blocks * QK_K)))
            .collect();
        let lanes = [&xs4[0][..], &xs4[1][..], &xs4[2][..], &xs4[3][..]];
        let v4 = unsafe { neon::kquant::dot_q6k_x4(&w, lanes) };
        for (i, lane) in lanes.iter().enumerate() {
            assert_eq!(v4[i], kquant::dot_q6k_scalar(&w, lane), "x4 lane {i}");
        }
    }
}

/// Both integer-dot paths (sdot and the baseline multiply) must agree
/// exactly, regardless of which one the CPU probe would pick.
#[test]
fn neon_sdot_and_baseline_paths_identical() {
    if !nr_tensor::cpu::features().dotprod {
        return;
    }
    let mut rng = Rng(61);
    let blocks = 6;
    let w8 = q8(&rng.adversarial(blocks * QK8_0));
    let x8 = q8(&rng.vec(blocks * QK8_0));
    unsafe {
        assert_eq!(
            neon::kernels::dot_q8_impl::<true>(&w8, &x8),
            neon::kernels::dot_q8_impl::<false>(&w8, &x8)
        );
    }
    let w4: Vec<_> = rng
        .adversarial(blocks * QK_K)
        .chunks_exact(QK_K)
        .map(quantize_q4k_ref)
        .collect();
    let w6: Vec<_> = rng
        .adversarial(blocks * QK_K)
        .chunks_exact(QK_K)
        .map(quantize_q6k_ref)
        .collect();
    let xk = q8k(&rng.vec(blocks * QK_K));
    unsafe {
        assert_eq!(
            neon::kquant::dot_q4k_impl::<true>(&w4, &xk),
            neon::kquant::dot_q4k_impl::<false>(&w4, &xk)
        );
        assert_eq!(
            neon::kquant::dot_q6k_impl::<true>(&w6, &xk),
            neon::kquant::dot_q6k_impl::<false>(&w6, &xk)
        );
    }
}

/// Saturated metadata: max 6-bit scales/mins, extreme quants.
#[test]
fn neon_kquant_saturated_blocks() {
    let q4 = kquant::BlockQ4K {
        d: nr_tensor::f32_to_f16(0.9),
        dmin: nr_tensor::f32_to_f16(1.7),
        scales: [0xFF; 12],
        qs: [0xFF; 128],
    };
    let q6 = kquant::BlockQ6K {
        ql: [0xFF; 128],
        qh: [0xFF; 64],
        scales: [-128i8; 16],
        d: nr_tensor::f32_to_f16(1.3),
    };
    let mut rng = Rng(41);
    let x = q8k(&rng.adversarial(QK_K));
    assert_eq!(kquant::dot_q4k_scalar(&[q4], &x), unsafe {
        neon::kquant::dot_q4k(&[q4], &x)
    });
    assert_eq!(kquant::dot_q6k_scalar(&[q6], &x), unsafe {
        neon::kquant::dot_q6k(&[q6], &x)
    });
}

/// f16 helpers vectorize the summation, so tolerance (like x86 F16C).
#[test]
fn neon_f16_matches_scalar_within_tolerance() {
    let mut rng = Rng(51);
    for n in [7usize, 64, 257] {
        let k: Vec<u16> = rng
            .vec(n)
            .iter()
            .map(|&v| nr_tensor::f32_to_f16(v))
            .collect();
        let q = rng.vec(n);
        let scalar: f32 = k
            .iter()
            .zip(&q)
            .map(|(&kb, &qv)| nr_tensor::f16_to_f32(kb) * qv)
            .sum();
        let v = unsafe { neon::kernels::dot_f16(&k, &q) };
        assert!(
            (scalar - v).abs() <= 1e-4 * scalar.abs().max(1.0),
            "n={n}: {scalar} vs {v}"
        );

        let mut out_s = rng.vec(n);
        let mut out_v = out_s.clone();
        let a = 0.37f32;
        for (o, &vb) in out_s.iter_mut().zip(&k) {
            *o += a * nr_tensor::f16_to_f32(vb);
        }
        unsafe { neon::kernels::axpy_f16(&mut out_v, a, &k) };
        for i in 0..n {
            assert!(
                (out_s[i] - out_v[i]).abs() <= 1e-5 * out_s[i].abs().max(1.0),
                "elem {i}"
            );
        }
    }
}
