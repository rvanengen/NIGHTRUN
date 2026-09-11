<div align="center">

<img src="web/assets/logo.svg" alt="NIGHTRUN" height="56">

**A local LLM runtime that boots from USB and runs without a conventional operating system.**

<img src="web/assets/nightrun-demo.gif" alt="NightRun booting in QEMU: splash, model loading with inline CRC, then a prompt answered by Llama 3.2 on the framebuffer" width="720">

*Real boot, one cut: loading and prefill sped up, generation at actual speed (Llama 3.2 1B, QEMU/KVM, 8 cores).*

</div>

---

This is weird software. It boots straight into an LLM.

There is no Linux userspace hiding underneath. Your machine's firmware starts NightRun
directly, NightRun copies a quantized model into RAM, draws its own terminal on the
framebuffer, and you chat. No kernel, no browser, no host process. The default build is
offline. Experimental network-only and network-plus-MCP builds are available when the
machine is intentionally connected to a trusted gateway.

Written in Rust. `no_std` where it counts. Runs on ordinary x86_64 PCs from a USB stick
and on a Raspberry Pi 5 from an SD card.

## The 10-second version

1. Run the installer. It builds a bootable image and flashes it to a USB stick or SD card.
2. Boot a machine from that stick.
3. Firmware starts NightRun. No OS loads, because there is no OS on the media.
4. The model (1.3 to 2.4 GB) streams into RAM with checksums verified during the read.
5. Storage is sealed. Any later disk read is a hard fault, on purpose.
6. You get a chat prompt. The model answers locally on your CPU. Offline is the default;
   optional networking and MCP are explicit build choices.

## Quick start

```sh
git clone https://github.com/rvanengen/NIGHTRUN.git
cd NIGHTRUN
less install.sh      # read what you are about to run
./install.sh
```

The installer walks you through target choice (x86_64 USB or Pi 5 SD), target RAM,
memory-compatible model selection, optional network/MCP checkboxes, a verified download
(pinned revision, SHA-256), image build, and flashing.

> **Warning.** The installer flashes removable media. It refuses system disks, lists only
> removable whole-disk devices, and demands you type `FLASH /dev/sdX` verbatim before
> writing a byte. Read the device name it shows you anyway. Flashing the wrong disk is
> not a recoverable error.

You need Linux, Rust (stable + nightly), QEMU if you want to test without hardware, and
about 6 GB of free disk for a model plus the image.

### Choose a connectivity mode

The installer shows checkbox-style controls and records the selected mode in the image
build. Both optional stacks default to off.

| Mode | Installer state | Manual flag | Contents |
|---|---|---|---|
| Offline | Network `[ ]`, MCP `[ ]` | no flag | No NIC, packet stack, or MCP application code |
| Network only | Network `[x]`, MCP `[ ]` | `--network` | Ethernet, ARP, IPv4, ICMP and UDP |
| Network + MCP | Network `[x]`, MCP `[x]` | `--mcp` | Packet stack plus bidirectional MCP through the host gateway |

MCP implies networking. Unchecking networking also unchecks MCP. The standard build
remains offline and does not require a gateway.

## Screenshots

| Loading | Generating |
|---|---|
| ![Model loading screen with streaming CRC stages](web/assets/screenshots/loading.png) | ![Chat mid-generation with live tok/s in the status bar](web/assets/screenshots/chat-generating.png) |

| Granite | Qwen |
|---|---|
| ![Granite 4.1 3B answering in the chat UI](web/assets/screenshots/chat-granite.png) | ![Qwen3 4B handling Polish and French text](web/assets/screenshots/chat-qwen.png) |

Captured from real boots in QEMU with the repository's own screenshot tooling
(`cargo xtask run --shot`). Nothing staged, nothing mocked up.

## What's inside

**Boot and runtime.** A single UEFI application: GOP framebuffer with a custom renderer
and bitmap fonts, USB keyboard input with caret editing and scrollback, multi-core
inference through firmware MP services, serial-port diagnostics. On the Pi 5 it also
drives the fan through the RP1, because nothing else is around to do it.

**Model lifecycle.** GGUF inspection and conversion (`nrconvert`), a purpose-built `.nrm`
container, full RAM residency before chat starts, CRC-32 sections verified while the file
streams from disk (there is no separate verify pass), and sealed storage afterwards.
Generation never touches the disk.

**Inference.** Hand-written quantized kernels: AVX2+FMA+F16C on x86_64, NEON on the Pi,
scalar reference implementations kept for both. Q8_0, Q4_K and Q6_K weights are used in
place, no dequantized copies. Prompt processing is batched (up to 64 tokens per pass) and
proven bit-identical to token-at-a-time decode. The generation loop allocates nothing.

**Correctness.** Greedy output is pinned token-for-token against llama.cpp for every
supported model family, on every change. Tokenizers are tested against fixtures generated
from the official Hugging Face tokenizers, chat templates against `apply_chat_template`.
If a kernel change breaks parity, the kernel is wrong. That rule has caught real bugs.

**Networking (experimental, opt-in).** `nr-net` provides an allocation-free
Ethernet/ARP/IPv4/ICMP/UDP stack with a fixed neighbor cache and checked wire parsing.
The `nr-boot` `network` feature connects it to UEFI's Simple Network Protocol; see the
[network-stack notes](docs/network.md). The separate `mcp` feature and a
bearer-authenticated host gateway add bidirectional MCP Streamable HTTP for inbound
prompts/status and outbound tool calls; see [the MCP guide](docs/mcp.md). Normal image
builds do not enable either stack.

## Supported targets

| Target | Status | Boot media | Notes |
|---|---|---|---|
| x86_64 UEFI | supported | USB | any 64-bit UEFI machine, Secure Boot off; validated in QEMU/OVMF; real-machine firmware quirks vary, boot reports welcome |
| Raspberry Pi 5 | supported | microSD | validated on a real D0-stepping 8 GB board; UEFI firmware built from pinned source |
| Apple Silicon macOS | host mode | none | `nrhost` runs the same ARM64/NEON inference engine; native Mac boot is a separate future port; see [Apple Silicon](docs/apple-silicon.md) |
| Legacy BIOS | not supported | | UEFI only |

## Supported models

| Model | Quant | Size | RAM needed | Runs on |
|---|---|---|---|---|
| Llama 3.2 1B Instruct | Q8_0 | 1.3 GB | 4 GB | x86_64, Pi 5 |
| Llama 3.2 3B Instruct | Q4_K_M | 1.9 GB | 6 GB | x86_64, Pi 5 |
| Granite 4.1 3B | Q4_K_M | 2.0 GB | 6 GB | x86_64, Pi 5 |
| Qwen3 4B Instruct 2507 | Q4_K_M | 2.3 GB | 8 GB | x86_64, Pi 5 (8 GB) |

Three model families are implemented, each with its real quirks handled faithfully:

- **Llama 3.2**: GQA, adjacent-pair RoPE, tied embeddings, the Llama 3 chat template.
- **Qwen3**: NEOX-style rope (half-split pairs), per-head Q/K RMSNorm before rope, no BOS
  token, attention width (4096) wider than the hidden size (2560), tied output head.
- **Granite 4.1**: dense transformer only. GQA, SwiGLU, four muP-style scalars from the
  header (embedding, attention, residual, logit). Hybrid SSM/MoE Granite variants are
  rejected at conversion with a named reason, not mangled at runtime.

Any GGUF whose tensors use Q8_0, Q4_K, Q6_K or F32 converts with each tensor's exact
dtype preserved, so Q8_0, Q4_K_M, Q4_K_S and Q6_K builds of these families all work.
This is not "arbitrary GGUF support": a new architecture family needs engine work and
reference validation, and the converter will tell you so.

### Add, disable, or remove catalog models

[`config/models.manifest`](config/models.manifest) is the single source of truth for the
installer's available models. The installer does not contain a second hard-coded list.
Every enabled entry is filtered by target and `min_ram_gb`, so a newly added compatible
model automatically appears only when the selected machine has enough RAM.

To add a model, first confirm that `nrconvert --inspect` accepts its GGUF architecture
and tensor types. Then append a uniquely named block to the manifest:

```ini
model.example-3b.name           = Example 3B Instruct
model.example-3b.enabled        = yes
model.example-3b.family         = llama3
model.example-3b.quant          = Q4_K_M
model.example-3b.repo           = owner/repository
model.example-3b.file           = exact-file-name.gguf
model.example-3b.revision       = full-pinned-repository-commit
model.example-3b.sha256         = full-64-character-artifact-sha256
model.example-3b.size_bytes     = exact-download-size
model.example-3b.license        = exact model license
model.example-3b.gated          = no
model.example-3b.min_ram_gb     = 6
model.example-3b.targets        = x86_64,rpi5
model.example-3b.nrm_bytes      = measured-converted-size
model.example-3b.min_media_gb   = 4
model.example-3b.blurb          = Short installer description
```

Never use `main` or another moving branch name as `revision`; pin the repository commit
and verify the downloaded artifact's SHA-256 and exact byte size. A catalog entry is a
supply-chain promise, not just a download shortcut.

There are two removal levels:

- Set `model.<id>.enabled = no` to hide a model from the installer while preserving its
  pinned metadata for easy restoration and audit history.
- Delete every `model.<id>.*` line to remove it permanently from the catalog. Cached
  `.gguf` and `.nrm` files are local build artifacts and are deliberately not deleted.

After any catalog edit, run:

```sh
scripts/installer/tests/run.sh
cargo run --release -p nrconvert -- --inspect path/to/model.gguf
```

The first command validates the manifest and installer behavior on Linux with Bash 4+;
the second validates the actual model. Adding a model from a new family still requires
engine, tokenizer/template, kernel, conversion, and llama.cpp parity work. See the
[installer documentation](docs/installer.md) for pinning and validation details.

## Architecture

```
firmware (UEFI)
  -> NightRun entry: framebuffer, keyboard, timers, MP services
  -> model loader: streaming read + CRC verification
  -> .nrm validation: header, tensor table, tokenizer payload
  -> arena allocation: KV cache + scratch, sized up front
  -> storage sealed (later disk reads = hard fault)
  -> tokenizer + chat template (per family)
  -> batched prefill -> decode loop -> sampling
  -> framebuffer chat UI with live stats
  -> optional SNP NIC -> nr-net -> optional nr-mcp -> trusted host gateway
```

Some choices worth explaining:

**Why UEFI-resident.** NightRun deliberately stays on UEFI Boot Services instead of
calling `ExitBootServices()`. That is what makes a USB keyboard, a display, and disk
reads work on effectively any machine without shipping half a kernel's worth of drivers.
Firmware is the platform layer; everything above it (loader, formats, tokenizer, kernels,
KV cache, sampling, UI) is NightRun code. We say "no conventional OS", not "pure bare
metal", because precision matters more than a cooler-sounding claim.

**Why the model lives in RAM.** One copy, made once, verified during the copy. After
sealing, the generation path cannot perform I/O even by accident. It also makes
performance predictable: decode speed is memory bandwidth, not disk luck.

**Why a custom format.** `.nrm` is a boot-friendly container: fixed header, 64-byte
aligned tensors used in place as zero-copy views, tokenizer and chat template embedded,
CRC-32 over both metadata and data. The converter re-parses and re-checksums its own
output before declaring success. Malformed files get named errors at parse time; the
parser is tested against truncations, wrapped offsets, and misaligned tables.

**Why tokenizer parity is treated as life-or-death.** A model that loads perfectly but
tokenizes almost-correctly produces subtly wrong output, which is the least debuggable
failure mode there is. So tokenization and templates are pinned to the official
implementations by generated fixtures, and user text can never encode into control tokens.

The long version, with the war stories, lives in [docs/architecture.md](docs/architecture.md)
and on the [project site](web/).

## Benchmarks

Measured numbers from [docs/benchmarks.md](docs/benchmarks.md), conditions attached.
QEMU rows: q35, KVM, 8 cores, AVX2 host; single scripted runs that vary about 20% with
host load. Pi row: real board.

| Model | Prompt | Decode | Boot to chat | Where |
|---|---|---|---|---|
| Llama 3.2 1B Q8_0 | 52-56 tok/s | ~20 tok/s | 5.6 s | QEMU/KVM, 8 cores |
| Granite 4.1 3B Q4_K_M | 23-27 tok/s | ~14 tok/s | 9.5 s | QEMU/KVM, 8 cores |
| Qwen3 4B Q4_K_M | ~23 tok/s | ~11 tok/s | 11.5 s | QEMU/KVM, 8 cores |
| Granite 4.1 3B Q4_K_M | 6.2 tok/s | 3.0 tok/s | ~30 s | Raspberry Pi 5 (8 GB, D0), pre-sdot kernels |

Against llama.cpp on the same machine, same GGUF, greedy: decode is at parity (both are
memory-bandwidth-bound), batched prefill lands within 1.15x to 1.4x. Decode slows as the
context fills, because attention reads the whole KV cache per token: Granite drops from
about 13 tok/s early to 11.6 tok/s averaged over a 384-token generation. Headline numbers
are always the short-context best case; now you know.

## Building manually

The installer does all of this for you. If you want the pieces:

```sh
# 1. Get a model (example: Llama 3.2 1B)
curl -L -o models/Llama-3.2-1B-Instruct-Q8_0.gguf \
  https://huggingface.co/bartowski/Llama-3.2-1B-Instruct-GGUF/resolve/main/Llama-3.2-1B-Instruct-Q8_0.gguf

# 2. Convert to .nrm (inspects, converts, re-validates its own output)
cargo run --release -p nrconvert -- \
  models/Llama-3.2-1B-Instruct-Q8_0.gguf models/model.nrm

# 3. Build a bootable image
cargo xtask image --model models/model.nrm        # x86_64 -> nightrun.img
cargo xtask pi-image --model models/model.nrm     # Pi 5   -> nightrun-pi5.img
# Add --network for the packet stack alone, or --mcp for network + MCP.
#    (the Pi image needs firmware built once from pinned source:
#     scripts/build-rpi5-firmware.sh; see docs/rpi5-uefi.md)

# 4. Try it in QEMU before touching hardware
cargo xtask run --img --mem 4G --window

# 5. Flash (THIS ERASES THE TARGET DISK)
lsblk -d -o NAME,SIZE,MODEL,TRAN               # find your USB stick
sudo dd if=nightrun.img of=/dev/REPLACE_WITH_USB_DISK bs=4M status=progress conv=fsync
```

Flash the whole disk device, never a partition like `/dev/sdb1`. The EFI binary itself
builds with `cargo xtask build`; it needs nightly and a custom hard-float target, which
xtask handles.

## The installer

`./install.sh` is the guided path, and it is deliberately paranoid. It should be.

- Detects removable media conservatively: whole disks on USB/SD transports only. Anything
  backing `/`, `/boot`, `/home` or swap is excluded by walking the full storage stack
  (LVM and LUKS included), not by guessing from a "removable" flag.
- Never preselects a device. An empty list is a normal answer.
- Fingerprints the chosen device (size, model, serial) and re-verifies the identity
  immediately before writing, so a re-enumerated `/dev/sdX` aborts instead of hitting
  the wrong disk.
- Asks twice, and the second confirmation is typing `FLASH /dev/sdX` exactly. A stray
  "y" does nothing.
- Downloads are pinned to a Hugging Face revision and verified by SHA-256 before use.
  Tokens for gated models are read silently and never logged or persisted.
- After writing, it reads the media back and compares SHA-256 digests. "Verified" means
  the stick actually contains the image.
- Interrupting mid-write tells you the media is incomplete instead of pretending.

The test suite (`scripts/installer/tests/run.sh`) exercises the safety logic against
fixtures and never touches a real disk. Details in [docs/installer.md](docs/installer.md).

## Inside the chat

```
ENTER      send prompt
ESC        stop a running generation
LEFT/RIGHT, BACKSPACE, DELETE   edit the prompt at the caret
UP/DOWN, PGUP/PGDN              scroll conversation history
/clear     reset the conversation, model stays in RAM (instant)
/bye       power off through UEFI runtime services
```

The status bar shows core count, memory in use, context fill, prompt throughput,
first-token latency, and live generation speed. When the context window would overflow,
the conversation resets itself and says so on screen.

## Project status

NightRun is experimental systems software. It works, it is tested hard, and it still
assumes you are comfortable with firmware menus and boot media.

Solid: the inference engine (parity-pinned against llama.cpp), the `.nrm` toolchain,
batched prefill (bit-identical to decode, proven), the installer safety logic
(fixture-tested plus a repository-wide audit, see [docs/QUALITY_AUDIT.md](docs/QUALITY_AUDIT.md)),
QEMU boots for both architectures, and the Pi 5 bring-up on a real D0 board.

Still open: broad real-hardware coverage on x86 machines (firmware quirks vary), Pi 5
sustained-thermal measurements, faster NEON dot kernels on the Pi (implemented, awaiting
board re-benchmarks), C1-stepping Pi boards (tooling ready, untested on our hardware),
DHCP and boot-time network configuration, cryptographic protection for the device UDP
link, and native Apple Silicon boot. Native ARM64 macOS host inference is supported;
see [Apple Silicon](docs/apple-silicon.md). The durable feature history and enhancement
backlog live in [memory.md](memory.md).

## Built with a coding agent

Most of the code in this repository was written with Claude Code using the Fable 5 model.

That is part of the point. NightRun is also a test of how far a coding agent can be
pushed when the target is not a web app but a bootable systems project: firmware entry
points, a binary model format, quantized SIMD kernels, tokenizer parity, and an installer
that must never eat the wrong disk. The methodology stayed boring on purpose: reference
implementations for every kernel, token-for-token parity gates, adversarial parser tests,
and a paper trail in the docs.

Judge it like normal software: does it boot, does it run, does it validate, does it avoid
eating your USB drive, and does the code make sense?

## Contributing

Most useful right now: boot reports from real x86 machines (make, firmware version, what
happened), Pi 5 testing (especially C1 boards and thermals), tokenizer fixture cases in
more languages, kernel work, and benchmark reproductions. The contract for engine changes
is simple: the llama.cpp parity tests must stay green. If parity breaks, the code is
wrong, not the fixture.

## License

MIT. See [LICENSE](LICENSE).

Model weights are not included and carry their own terms: the Llama models under the
Llama Community License, Qwen and Granite builds under Apache-2.0. The Spleen bitmap
font is BSD-2-Clause (bundled, attribution in [assets/fonts](assets/fonts)).
