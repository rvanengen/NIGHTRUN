# NightRun architecture

NightRun is one `no_std` Rust UEFI application that boots a machine straight into a
local LLM chat. This file is the engineering reference: what runs where, why the big
decisions went the way they did, and which mistakes already got caught by the test
suite so you don't repeat them.

Four models ship in the catalog across three model families: Llama 3.2 1B Instruct
(Q8_0), Llama 3.2 3B Instruct (Q4_K_M), Granite 4.1 3B (Q4_K_M, dense transformer)
and Qwen3-4B-Instruct-2507 (Q4_K_M). One model per image
(`cargo xtask image --model <file.nrm>`).

Scope note: NightRun supports the conventional dense transformer variant of Granite
only. Hybrid Granite architectures (Mamba-2/SSM layers, MoE) are intentionally out of
scope and rejected at conversion with a named, actionable error.

Background: most of the code was written with Claude Code using the Fable 5 model,
against a fixed methodology of reference kernels, token-level parity gates and
adversarial parser tests. The README's "Built with a coding agent" section has the
short version.

## The load-bearing decision: UEFI Boot Services stay on

NightRun never calls `ExitBootServices`. The firmware provides exactly four things at
runtime:

| Firmware service | Used for |
|---|---|
| `GRAPHICS_OUTPUT_PROTOCOL` | obtaining the linear framebuffer (once) |
| `SIMPLE_TEXT_INPUT` | keyboard (USB keyboards work through the firmware's own USB stack) |
| `SIMPLE_FILE_SYSTEM` | reading `model.nrm` off the boot volume (once) |
| `MP_SERVICES` + `AllocatePages`/`stall` | starting cores, memory, timing |

Everything else is NightRun's own code: rendering, fonts, memory management,
tokenization, tensor math, sampling, the UI.

An experimental exception is available behind the disabled-by-default
`nr-boot/network` feature. It uses firmware's Simple Network Protocol only as an
Ethernet device adapter; Ethernet, ARP, IPv4, ICMP, and UDP are implemented by the
allocation-free `nr-net` crate. See [network.md](network.md). The normal build does not
open a NIC and retains the offline behavior described here.

The separate `nr-boot/mcp` feature implies `network` and adds bounded NRMP framing plus
the MCP application layer. HTTP, TLS, authentication and remote MCP sessions remain in
the host gateway rather than the firmware binary; see [mcp.md](mcp.md).

Why not exit boot services? After `ExitBootServices` a USB keyboard requires a full
XHCI host-controller driver, and PS/2 emulation is unreliable on modern firmware.
Staying resident trades a purist badge for something that actually boots and takes
keystrokes on real machines. The application processors we start never touch firmware
services (pure compute plus atomics), which keeps us inside UEFI's rules.

The same binary architecture runs on two targets: `BOOTX64.EFI` for x86_64 PCs and
`BOOTAA64.EFI` for the Raspberry Pi 5 (on a TF-A + EDK2 firmware port built from
pinned source; docs/rpi5-uefi.md records the pins, the diff review and the bring-up
war stories). Platform differences live in small adapter modules (serial: port I/O vs
PL011; clock: TSC vs CNTVCT; fan: nothing vs the RP1 PWM driver, because on the Pi
nobody else is around to spin the fan).

## Boot flow

```
efi_main
├─ serial up (COM1 port I/O on x86, PL011 on the Pi)   [debug channel]
├─ disable firmware watchdog (or it reboots us after 5 min)
├─ enable vector state (x86: CR4.OSXSAVE + XCR0 for AVX; the Pi boots with NEON on)
├─ GOP: pick 1280x720 BGRX (fallback chain), grab framebuffer pointer
├─ install panic screen (heap-free direct framebuffer drawing)
├─ splash (procedural synthwave scene + logo)
├─ boot sequence (loading screen with real progress):
│  ├─ clock calibration against firmware stall
│  ├─ MP services: start all APs into the spin-worker pool
│  ├─ memory map scan (conventional RAM tally)
│  ├─ read \model.nrm in 16 MB chunks into AllocatePages memory,
│  │  CRC32 running over each chunk as it lands (no post-load pass)
│  ├─ parse + validate header, tensor table, tokenizer payload
│  ├─ seal storage (further model reads from disk are a hard fault)
│  └─ arena sized from InferCtx::required_bytes (KV cache + scratch)
└─ chat loop (poll keys, template, batched prefill, sample/stream)
```

Every failure on this path is terminal and named: a corrupt file reports the CRC
mismatch and how to rebuild, an undersized machine reports the allocation failure.
The chat screen cannot appear unless the whole chain succeeded.

## Memory model

- **Model blob**: firmware pages (`LOADER_DATA`), loaded once, resident forever;
  tensors are zero-copy views (`&[BlockQ4K]`/`&[f32]` straight into the blob,
  64-byte aligned by the converter, alignment re-checked by the parser). The blob
  needs one contiguous allocation, which in practice sets the RAM floor at roughly
  2.5x the model size on fragmented firmware memory maps.
- **Arena** (bump allocator, sized per model at boot: ~140 MB for Llama 1B, ~650 MB
  for Qwen3 4B): f16 KV cache, activation and scratch buffers, logits. Allocated
  during boot; generation performs zero allocations.
- **Heap** (UEFI pool via the `uefi` crate allocator): UI strings, scrollback,
  tokenizer output. Never touched inside the token loop proper.

## .nrm model format

Produced by `tools/nrconvert` from a GGUF. Little-endian, fixed 192-byte header:
magic `NRUN`, version (3), arch id (llama3 / qwen3 / granite), dims (dim / layers /
heads / kv heads / head_dim / ffn / vocab / ctx), rope theta + Llama-3 scaling
params, flags (tied embeddings), four muP-style scalars, display name, then offsets
for the tokenizer blob, the tensor table (32-byte entries: kind, layer, dtype,
offset, size, rows, cols) and the 64-byte-aligned data section. CRC32 over metadata
and data, verified while the file streams in. Parsing on bare metal is header reads
plus pointer arithmetic; there is no GGUF parsing in the runtime.

The parser treats the file as untrusted input: all offset and size math is checked
arithmetic, tensor offsets must honor the alignment contract, every tensor's byte
size must match its dtype's block math exactly, and the tokenizer table is validated
entry by entry at parse time. An adversarial test suite (truncations, wrapped
offsets, overflowing dimensions, misaligned tables, flipped CRC bits) pins the
behavior: malformed files get named errors, never panics.

Tensors stay in their GGUF block layouts: Q8_0 (32 x i8 + f16 scale = 34 B), Q4_K
(256-value super-blocks, packed 6-bit scale/min pairs, 144 B) and Q6_K (4+2-bit
planes, 16 signed scales, 210 B); norms in f32. "Q4_K_M" is a per-tensor policy,
not one format, and NightRun preserves each tensor's exact source dtype.

Granite's four muP scalars (embedding x12, attention score 1/64 replacing
1/sqrt(head_dim), residual x0.22, logits /10 for granite-4.1-3b) are stored
explicitly, carry neutral values for the other families, and are applied branch-free
in the forward pass. The audited Granite Q4_K_M policy mirrors Qwen's: Q4_K
majority, Q6_K for attn_v + ffn_down in 20/40 layers and the (tied) token_embd;
pretokenizer id "dbrx" equals the cl100k pattern our Llama-3 style already
implements; template `<|start_of_role|>role<|end_of_role|>content<|end_of_text|>\n`
with `<|end_of_text|>` (100257) tripling as bos/eos/eot.

The audited dtype policy of the Qwen3-4B Q4_K_M artifact: everything Q4_K except
`attn_v` (18/36 layers), `ffn_down` (18/36 layers) and `token_embd` in Q6_K; norms
and the per-head Q/K norms F32. The model is tied (no `output.weight`): the Q6_K
embedding matrix doubles as the classifier, validated by a dedicated test plus
llama.cpp parity. The GGUF `rope_freqs` tensor (Llama-3 frequency divisors) is
carried through and preferred at runtime when present; Qwen3 uses plain theta=5e6.

## Tokenizer

Byte-level BPE (tiktoken-style, 100k-152k vocab depending on family). The converter
decodes GGUF's byte-unicode token strings to raw bytes, resolves merge strings to id
pairs `(left, right) -> (result, rank)` sorted for binary search, and emits a flat
blob (token table + string pool + byte-to-id table + special ids). The runtime
pretokenizer is a hand-rolled implementation of the family split regexes
(contractions, letter runs with optional prefix, digit runs, punctuation, whitespace
lookahead) using core's Unicode tables. All 57 reference cases per family (emoji,
CJK, Arabic, Polish, code, JSON, URLs, special-token literals, more) match the
official HF tokenizers exactly; exotic Unicode-category edge cases may deviate, a
documented limitation. Control tokens are never encoded from user text.

The blob's template field selects the chat format: Llama-3 headers
(`<|start_header_id|>` ... `<|eot_id|>`, BOS-prefixed), ChatML (`<|im_start|>role\n`
... `<|im_end|>\n`, no BOS; Qwen) or the Granite role format above. The Qwen
pretokenizer differs from Llama-3's in exactly one rule (single `\p{N}` instead of
`{1,3}`). All are fixture-tested against the official HF tokenizers and the
templated paths against `apply_chat_template` exactly. The UI labels the assistant
by family: `llama:`, `qwen:` or `granite:`.

## Inference

Per token: embedding row dequant -> n_layers x [RMSNorm -> QKV matvec -> (Qwen3:
per-head RMSNorm on Q and K) -> RoPE -> f16 KV append -> GQA attention -> output
matvec -> residual -> RMSNorm -> SwiGLU MLP -> residual] -> final norm -> classifier.

Prompt processing does not run token-at-a-time: `InferCtx::prefill_chunk` batches up
to 64 prompt tokens per pass through 4-wide register-tiled kernels, and a dedicated
test suite proves the result bit-identical to sequential decode, logits and KV cache
both, across lengths from 1 to 513 including every batch-boundary neighbor. Decode stays token-at-a-time, as it must.

Family differences handled by the same engine: attention width may differ from
hidden width (Qwen3: 32x128=4096 vs dim 2560, with dedicated q/attention-out
buffers), per-head Q/K RMSNorm before RoPE, and RoPE pairing style: adjacent pairs
for llama-family GGUFs (conversion permutes Q/K weights) vs NEOX half-split pairs
for Qwen3. Getting the RoPE style wrong produces coherent-but-divergent output;
parity tests catch it, and did.

- Weight matrices are dtype-tagged (`QMat`); dispatch happens once per matvec call.
  Activations are quantized to the matching format, Q8_0 (32-blocks) or Q8_K
  (256-blocks with group sums, llama.cpp numerics), once per activation vector even
  when several matrices consume it.
- x86_64: integer dots via AVX2 `sign/maddubs/madd` for q8xq8; the k-quant kernels
  follow ggml's maddubs structure with scalar 6-bit scale/min bookkeeping and
  bsums-based min correction (Q4_K) / bit-plane reassembly minus 32 (Q6_K). KV cache
  attention uses F16C (`vcvtph2ps`).
- aarch64 (Pi 5): NEON equivalents of the same kernels, with a runtime-detected
  sdot fast path for the q8 dot (emitted as a raw instruction so the baseline build
  never depends on the dotprod target feature). Bit-identical to the scalar
  reference by construction, asserted with `==` in a qemu-user test rig.
- Scalar reference implementations of every kernel are kept permanently and used as
  the comparison oracle.
- The builtin `x86_64-unknown-uefi` target is soft-float, which breaks AVX
  intrinsics and software-floats every f32 op. NightRun builds against a custom
  hard-float target (`x86_64-nightrun-uefi.json`) with `-Zbuild-std=core,alloc` on
  nightly. The aarch64 build uses the stock `aarch64-unknown-uefi` target on stable.
- Multi-core: matvec rows fan out over a spin-worker pool (`nr-tensor::parallel`).
  APs are started once via MP services and never return; jobs are posted with
  atomics only. The same pool code drives std threads in `nrhost`, which runs the
  identical engine on Linux for debugging.
- Sampling: greedy, or temperature + top-k(64) prefilter + top-p nucleus.

Correctness: optimized kernels are tested against scalar (including adversarial and
saturated k-quant blocks); f16 against known bit patterns; the full forward pass is
pinned to token-for-token greedy parity with llama.cpp on the same artifacts, for
every family, including chat-templated replies and the tied-head audit. One nuance
is documented rather than papered over: Granite's /10 logit scaling compresses
greedy top-2 gaps, so near-ties can legitimately flip between numerically-equivalent
implementations; `nrhost --debug-gap` measures the gap so a real bug and a coin flip
are distinguishable.

## Performance

Measured numbers with conditions live in docs/benchmarks.md. The shape of the
performance story: decode is memory-bandwidth-bound and lands at parity with
llama.cpp on the same artifacts; batched prefill lands within 1.15-1.4x of
llama.cpp's; decode slows as the context fills because attention reads the whole KV
cache per token (measured: Granite ~13 tok/s early, 11.6 tok/s averaged over a
384-token generation).

## UI

`nr-gfx` renders into a RAM back buffer presented once per frame (row memcpy for
BGRX). Fonts are Spleen PSF1/PSF2 bitmaps (8x16 through 32x64). The splash/loading
backdrop (gradient sky, stars, striped sun, perspective grid) is fully procedural;
text supports shear, per-row gradients and a blended glow. The panic handler draws a
themed fault screen with direct, heap-free framebuffer writes.

The chat is a real terminal: instant echo of the sent prompt, a thinking indicator
until the first token, streaming output, caret editing, scrollback with a position
banner, `/clear` (reset conversation, model stays resident) and `/bye` (UEFI
power-off). The status bar reports core count, memory, context fill, prompt
throughput, first-token latency and live decode speed. The input line is
length-capped; UTF-8 sequences split across token boundaries are reassembled before
display.

## The installer

`./install.sh` is the guided path from a fresh clone to verified bootable media:
target choice, pinned-and-hashed model downloads, conversion, image build,
conservative removable-media detection, a typed `FLASH /dev/sdX` confirmation and
SHA-256 readback verification. It gets its own document (docs/installer.md) because
media safety deserves one.

## Limitations

- Context capped at 4096 tokens (KV memory: 128 MB for Llama 1B, 604 MB for Qwen3
  4B; the boot arena is sized from `InferCtx::required_bytes`); the conversation
  auto-resets when full. No KV eviction or sliding window.
- The model blob requires one contiguous firmware allocation; fragmented memory maps
  raise the practical RAM floor (a 2.0 GB model wants a 5 GB machine).
- The pretokenizer is an approximation outside common Unicode classes.
- Glyph coverage follows the Spleen fonts' Unicode tables (ASCII, Latin-1/Extended-A
  including Polish, typographic punctuation, Greek and Cyrillic where provided).
  Codepoints without a glyph (emoji, CJK) render as a clean gap, never as a
  substituted '?'; the tokenizer and history keep the exact text either way.
- Requires UEFI; no legacy BIOS path.
- Firmware keyboard repeat/rollover behavior varies between vendors.
- CPU only. Performance ceilings are memory-bandwidth ceilings.
