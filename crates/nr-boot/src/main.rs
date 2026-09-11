//! NightRun boot layer: UEFI entry, platform bring-up, panic screen.

#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
pub mod serial;
mod app;
#[cfg(target_arch = "aarch64")]
mod fan;
mod input;
mod modelload;
#[cfg(feature = "network")]
mod network;
mod smp;
mod video;

use core::fmt::Write as _;
use core::sync::atomic::{AtomicPtr, Ordering};

use uefi::prelude::*;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[entry]
fn main() -> Status {
    // FIRST: make FP/SIMD usable. QEMU's AAVMF enables it before running
    // apps, but firmware is not required to — with access trapped, the
    // first FP/NEON instruction (rustc emits them even in memcpy) is a
    // silent synchronous exception. Integer-registers-only asm.
    #[cfg(target_arch = "aarch64")]
    enable_fp_early();

    serial::init();
    serial_println!("[nightrun] v{} boot layer up", VERSION);
    uefi::helpers::init().expect("uefi helpers");
    serial::attach();
    // Early boot narrates through the firmware text console: on hardware
    // without a serial hookup, the last line frozen on screen names the
    // failing stage.
    con_print("NightRun: boot layer up\r\n");
    // The firmware watchdog would reset the machine mid-chat; disable it.
    let _ = uefi::boot::set_watchdog_timer(0, 0x1_0000, None);
    con_print("NightRun: watchdog off, probing CPU features\r\n");
    enable_simd();
    con_print("NightRun: bringing up framebuffer (GOP)\r\n");

    let display = video::init();
    install_panic_fb(&display);
    serial_println!("[boot] framebuffer up, entering app");

    app::run(display);
    Status::SUCCESS
}

/// Print through the firmware's text console (visible on screen before
/// our framebuffer takes over; no-op once it's unavailable).
pub fn con_print(msg: &str) {
    use core::fmt::Write as _;
    uefi::system::with_stdout(|out| {
        let _ = out.write_str(msg);
    });
}

/// Enable the SIMD state the kernels need and log what the CPU offers.
#[cfg(target_arch = "x86_64")]
fn enable_simd() {
    if !enable_simd_quiet() {
        serial_println!("[cpu] no AVX - scalar kernels");
        return;
    }
    let f = nr_tensor::cpu::features();
    serial_println!(
        "[cpu] avx enabled; avx2={} fma={} f16c={}",
        f.avx2,
        f.fma,
        f.f16c
    );
}

/// NEON is architecturally baseline on aarch64 UEFI (hard-float target);
/// access is unlocked by `enable_fp_early` at entry, so just log.
#[cfg(target_arch = "aarch64")]
fn enable_simd() {
    enable_simd_quiet();
    let f = nr_tensor::cpu::features();
    serial_println!(
        "[cpu] aarch64 neon baseline; dotprod={} fp16={}",
        f.dotprod,
        f.fp16
    );
}

/// Un-trap FP/SIMD at whichever EL the firmware runs us (the aarch64
/// analog of the x86 XCR0 enable). Uses only integer registers so it is
/// safe to run before FP access exists.
#[cfg(target_arch = "aarch64")]
#[inline(never)]
fn enable_fp_early() {
    // SAFETY: reads CurrentEL and flips only the FP-trap controls for
    // that EL, then isb. No memory access, integer registers only.
    unsafe {
        let el: u64;
        core::arch::asm!("mrs {}, CurrentEL", out(reg) el, options(nomem, nostack));
        match (el >> 2) & 3 {
            2 => {
                // EL2 (TF-A launches EDK2 here on the Pi): clear
                // CPTR_EL2.TFP (bit 10, traps FP/SIMD when set).
                let mut cptr: u64;
                core::arch::asm!("mrs {}, cptr_el2", out(reg) cptr, options(nomem, nostack));
                cptr &= !(1u64 << 10);
                core::arch::asm!("msr cptr_el2, {}", in(reg) cptr, options(nomem, nostack));
            }
            1 => {
                // EL1 (QEMU virt AAVMF): CPACR_EL1.FPEN = 0b11 (no traps).
                let mut cpacr: u64;
                core::arch::asm!("mrs {}, cpacr_el1", out(reg) cpacr, options(nomem, nostack));
                cpacr |= 0b11 << 20;
                core::arch::asm!("msr cpacr_el1, {}", in(reg) cpacr, options(nomem, nostack));
            }
            _ => {}
        }
        core::arch::asm!("isb", options(nomem, nostack));
    }
}

#[cfg(target_arch = "aarch64")]
pub fn enable_simd_quiet() -> bool {
    true
}

/// AVX (YMM state) enable via CR4.OSXSAVE + XCR0 — UEFI guarantees SSE
/// only. Quiet variant is also used by AP worker bring-up, where two
/// cores sharing the serial port would interleave garbage.
#[cfg(target_arch = "x86_64")]
pub fn enable_simd_quiet() -> bool {
    use core::arch::x86_64::__cpuid;
    let leaf1 = __cpuid(1);
    let xsave = leaf1.ecx & (1 << 26) != 0;
    let avx = leaf1.ecx & (1 << 28) != 0;
    if !(xsave && avx) {
        return false;
    }
    // SAFETY: CPL0 under UEFI; setting CR4.OSXSAVE then XCR0 x87|SSE|AVX.
    unsafe {
        core::arch::asm!(
            "mov rax, cr4",
            "or rax, 1 << 18", // CR4.OSXSAVE
            "mov cr4, rax",
            out("rax") _,
            options(nostack)
        );
        core::arch::asm!(
            "xor ecx, ecx",
            "xgetbv",
            "or eax, 7", // x87 | SSE | AVX state
            "xsetbv",
            out("eax") _, out("ecx") _, out("edx") _,
            options(nostack)
        );
    }
    true
}

// ---- Panic screen ----------------------------------------------------------

static PANIC_FB: AtomicPtr<nr_gfx::direct::DirectFb> = AtomicPtr::new(core::ptr::null_mut());

fn install_panic_fb(display: &video::Display) {
    let fb = alloc::boxed::Box::leak(alloc::boxed::Box::new(display.direct()));
    PANIC_FB.store(fb, Ordering::Release);
}

/// Fixed-size formatting buffer usable during panic (no heap).
struct PanicBuf {
    buf: [u8; 512],
    len: usize,
}

impl core::fmt::Write for PanicBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    serial_println!("[panic] {}", info);

    // Best effort: surface the panic on the firmware console too (covers
    // failures before our framebuffer exists, e.g. GOP bring-up).
    {
        use core::fmt::Write as _;
        uefi::system::with_stdout(|out| {
            let _ = write!(out, "\r\nNightRun PANIC: {info}\r\n");
        });
    }

    let fb_ptr = PANIC_FB.load(Ordering::Acquire);
    if !fb_ptr.is_null() {
        // SAFETY: set once from a leaked box; framebuffer stays mapped.
        let fb = unsafe { &*fb_ptr };
        let mut msg = PanicBuf {
            buf: [0; 512],
            len: 0,
        };
        let _ = write!(msg, "{}", info);
        draw_panic_screen(
            fb,
            core::str::from_utf8(&msg.buf[..msg.len]).unwrap_or("panic"),
        );
    }

    loop {
        halt();
    }
}

/// Park the CPU (x86 hlt / aarch64 wfi).
#[inline]
pub fn halt() {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: privileged halt; we run at the firmware's privilege level.
    unsafe {
        core::arch::asm!("hlt")
    };
    #[cfg(target_arch = "aarch64")]
    // SAFETY: wfi is always safe to execute.
    unsafe {
        core::arch::asm!("wfi")
    };
}

fn draw_panic_screen(fb: &nr_gfx::direct::DirectFb, msg: &str) {
    use nr_gfx::theme;
    static SMALL: &[u8] = include_bytes!("../../../assets/fonts/spleen-8x16.psfu");
    let Some(font) = nr_gfx::PsfFont::parse(SMALL) else {
        return;
    };

    fb.fill_rect(0, 0, fb.width, fb.height, 0x12021c);
    let band_y = fb.height / 4;
    fb.fill_rect(0, band_y, fb.width, 4, theme::NEON_MAGENTA);
    fb.fill_rect(0, band_y + 90, fb.width, 4, theme::NEON_MAGENTA);
    fb.text(
        &font,
        48,
        band_y + 28,
        "NIGHTRUN // SYSTEM FAULT",
        theme::NEON_MAGENTA,
    );
    fb.text(
        &font,
        48,
        band_y + 56,
        "the runtime hit an unrecoverable error - power cycle to restart",
        theme::TEXT_DIM,
    );

    // Wrapped panic message.
    let cols = (fb.width - 96) / font.width;
    let mut y = band_y + 130;
    let bytes = msg.as_bytes();
    let mut i = 0;
    while i < bytes.len() && y < fb.height - 32 {
        let end = (i + cols).min(bytes.len());
        if let Ok(line) = core::str::from_utf8(&bytes[i..end]) {
            fb.text(&font, 48, y, line, theme::TEXT_PRIMARY);
        }
        i = end;
        y += font.height + 4;
    }
}
