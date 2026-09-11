# NightRun project memory

This file records the requested product direction, the current implementation,
and the constraints that future enhancements should preserve. Update it when a
feature changes the architecture, supported platforms, security boundary, model
catalog, or installer workflow.

## What was requested

1. Add a network stack to NightRun.
2. Include MCP support for inbound and outbound traffic.
3. Make networking and MCP optional through checkbox-style installer controls.
4. Select or offer LLMs according to the target machine's available memory.
5. Determine how NightRun can work on Apple Silicon computers.
6. Maintain the enhanced project at `https://github.com/rvanengen/NIGHTRUN`.
7. Keep the README synchronized with the implemented project state and define
   a safe process for adding, disabling, and removing catalog models.

## Current implemented state

### Networking

- `nr-net` is an allocation-free, `no_std` Ethernet/ARP/IPv4/ICMP/UDP stack.
- The UEFI adapter uses Simple Network Protocol and the first compatible NIC.
- Networking is disabled in default builds.
- Current firmware configuration uses static QEMU addresses. DHCP, DNS, TCP,
  and TLS are not implemented in the boot application.

### MCP

- `nr-mcp` provides bounded UDP fragmentation and reassembly with CRC-32.
- NightRun can receive MCP requests for status and local-model prompting.
- NightRun can send explicit outbound `tools/list` and `tools/call` requests.
- `nrmcp-gateway` translates between the device protocol and MCP Streamable
  HTTP, performs upstream initialization, and propagates MCP session IDs.
- The HTTP endpoint requires a bearer token and defaults to loopback.
- The device UDP link is not encrypted or cryptographically authenticated; it
  must remain on QEMU's private network or a trusted LAN.

### Build modes and installer controls

- Offline: no network or MCP application code.
- Network only: `--network`, or Cargo feature `network`.
- Network plus MCP: `--mcp`, or Cargo feature `mcp`; MCP implies networking.
- The Linux installer displays checkbox-style controls for both optional
  stacks. Both default to off.

### Memory-aware models

- The installer asks for RAM in the machine that will boot NightRun.
- Catalog entries exceeding the selected RAM are hidden with an explanation.
- The largest compatible catalog image is marked as the memory-tier choice.
- Future supported models automatically participate through the manifest's
  `min_ram_gb`, `targets`, and size fields.
- Each manifest entry supports `enabled = yes|no`; disabling preserves its
  pins and metadata while removing it from installer choices.
- Supported architecture families remain Llama, Qwen3, and dense Granite.
  Adding an unrelated GGUF architecture requires inference-engine work and
  reference validation, not merely another manifest entry.

### Apple Silicon

- `nrhost` builds and runs the shared inference engine as a native ARM64 macOS
  process using NEON.
- Host CPU-feature detection uses the operating system's supported mechanism,
  preventing unsupported ARM dot-product instructions from being selected.
- The destructive-media installer remains Linux-only.
- Native boot on Apple Silicon Macs is not implemented. It would be a separate
  platform port involving the m1n1/Asahi boot chain and Apple-specific display,
  input, storage, interrupt, timer, multicore, and network support.
- The existing ARM64 UEFI target supports Raspberry Pi 5 and generic QEMU UEFI;
  it must not be described as a native Mac boot image.

## Architectural rules to preserve

- Offline remains the default and must require no network hardware or gateway.
- MCP must remain a separate build choice from the lower-level packet stack.
- MCP-enabled builds may imply networking; network-only builds must not expose
  MCP commands or JSON-RPC services.
- Keep TCP, TLS, remote credentials, and standard HTTP session handling in the
  host gateway unless a reviewed firmware implementation is intentionally added.
- Treat remote MCP metadata, arguments, results, and prompts as untrusted input.
- Keep firmware buffers and message sizes bounded; inference must not acquire
  unbounded allocation or I/O paths.
- Preserve pinned model revisions, checksums, conversion validation, and the
  installer's system-disk protections.
- Do not claim arbitrary GGUF support or native Apple boot support.

## Future enhancement backlog

1. Add DHCP and a boot-time network configuration file so real hardware does
   not require editing source constants.
2. Add authenticated encryption to the device-to-gateway protocol, including
   replay protection and key provisioning.
3. Add gateway/device capability negotiation and protocol-version handling.
4. Add explicit user confirmation policies for sensitive outbound MCP tools.
5. Add request cancellation, retries, duplicate suppression, and clearer MCP
   connection state in the framebuffer UI.
6. Expand the pinned model catalog within already-supported architecture
   families and benchmark RAM requirements on each target.
7. Improve memory estimation using model weights, context length, KV-cache
   requirements, scratch space, and reserved firmware/host memory rather than a
   single catalog threshold.
8. Add an optional macOS build-only workflow while keeping destructive flashing
   separate from the Linux installer.
9. Evaluate Metal acceleration for `nrhost`; keep CPU/NEON as the reference and
   fallback path.
10. Treat native Apple Silicon boot as a dedicated milestone with hardware and
    boot-chain research, not as an extension of the UEFI image target.

## Validation baseline

At the time this memory was created:

- `cargo test --workspace` passed on an Apple Silicon host.
- Offline, network-only, and MCP-enabled `nr-boot` checks passed for both the
  x86_64 custom UEFI target and `aarch64-unknown-uefi`.
- `nr-net` and `nr-mcp` passed `no_std` checks.
- MCP gateway inbound/outbound integration tests passed.
- Installer scripts passed Bash syntax validation. The complete installer test
  harness still needs execution on a Linux host with Bash 4 or newer.

## Repository state

- Primary repository: `https://github.com/rvanengen/NIGHTRUN`
- Primary branch: `main`
- Original upstream: `https://github.com/hardrave/NIGHTRUN`
