# Apple Silicon

NightRun has two distinct Apple Silicon stories.

## Native macOS host process: supported

The `nrhost` tool runs the same model parser, tokenizer and inference engine as
the boot application as a normal ARM64 macOS process. It uses NEON kernels and
platform CPU-feature detection, including safe fallback on Apple chips that do
not expose the optional ARM dot-product instruction.

```sh
# Convert a supported GGUF once.
cargo run --release -p nrconvert -- model.gguf models/model.nrm

# Run locally on macOS. Adjust threads to suit the machine.
cargo run --release -p nrhost -- models/model.nrm \
  --threads 8 --prompt "Explain why the sky is blue." -n 128
```

Model memory is not just the file size: weights, the KV cache, scratch space,
and macOS all need room. As a conservative starting point, use the same RAM
thresholds shown by the installer (4 GB for Llama 1B, 6 GB for the 3B choices,
8 GB for Qwen 4B) and leave several GB for macOS. Unified memory works normally;
NightRun's host engine is CPU/NEON-based and does not yet use Metal.

The destructive-media installer remains Linux-only because its disk discovery,
system-disk protection and flash verification are built around Linux block
device APIs. A model can still be converted and run with the commands above on
macOS.

## Native boot on a Mac: not yet supported

The existing ARM64 boot target is a UEFI application for Raspberry Pi 5 and
generic QEMU `virt`; it is not an Apple boot image. Native Apple Silicon boot
would be a substantial new platform port, likely using the m1n1/Asahi boot
chain and adding Apple-specific display, keyboard, storage, timer, interrupt,
multicore and network drivers. UEFI firmware protocols used by NightRun are not
provided by a Mac's normal boot environment.

In short: running the engine on Apple Silicon is supported now; booting a Mac
directly into NightRun is feasible as a separate engineering project, not an
image-format switch.
