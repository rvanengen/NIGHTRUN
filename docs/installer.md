# The NightRun installer (`./install.sh`)

An interactive tool that takes you from "which machine?" to verified,
bootable NightRun media: target choice -> model catalog -> verified
download (or local GGUF) -> conversion -> a real target-specific image ->
safe device selection -> two-stage confirmed flash -> readback
verification -> boot instructions.

```sh
./install.sh
```

## Supported hosts and dependencies

Linux with Bash ≥ 4. Required (checked at preflight): `lsblk findmnt
mountpoint umount sync dd sha256sum stat realpath df` plus `curl` (or
wget) and `cargo` (rustup). Optional, used when present: `udisksctl`
(polite desktop unmount/eject). Missing packages are never installed
silently: the installer shows the exact package-manager command and asks
first; declining exits with manual instructions. On non-Linux hosts it
exits with an explanation (block-device and mount tooling are
Linux-specific); build images manually per README.md instead.

Layout: `install.sh` (launcher) + `scripts/installer/lib/*.sh` (one
module per concern: ui, preflight, targets, models, downloads, build,
media, flash, verify, safety) + `config/models.manifest` +
`scripts/installer/tests/`.

## Targets

The first choice selects genuinely different images. The next screen records
the target machine's RAM; models that cannot fit are hidden with an explicit
reason, and the largest compatible catalog model is marked as the recommended
best fit:

- **x86_64 UEFI** -> `cargo xtask image` -> GPT disk with a FAT32 ESP,
  `EFI/BOOT/BOOTX64.EFI`, `model.nrm`. Boots from USB on UEFI PCs.
- **Raspberry Pi 5** -> `cargo xtask pi-image` -> MBR disk with a FAT32
  partition carrying the Pi UEFI firmware (`RPI_EFI.fd`, built from
  pinned source: see docs/rpi5-uefi.md), device trees, `config.txt`,
  `EFI/BOOT/BOOTAA64.EFI`, `model.nrm`. Boots from microSD.

One image is never silently used for the other target. Selecting Pi 5
without the firmware payload present offers to run
`scripts/build-rpi5-firmware.sh` (and explains its dependencies).

## Optional stacks

Before building, a checkbox-style screen controls two independent layers:

- `[ ] Network stack`: Ethernet, ARP, IPv4, ICMP and UDP.
- `[ ] MCP bridge`: bidirectional MCP/JSON-RPC through the authenticated host
  gateway. MCP depends on networking, so checking it also checks networking;
  unchecking networking disables both.

Both default off. The installer passes `--network` for the packet stack alone
or `--mcp` for network plus MCP. The IMAGE READY card records the selected
mode, so an offline image is never presented as a network-enabled one.

## The model manifest (`config/models.manifest`)

The single source of truth for the catalog. Format: `model.<id>.<key> =
value`, one per line, `#` comments; the file is parsed as data and never
executed. Keys per model: `name family quant repo file revision sha256
size_bytes license gated min_ram_gb targets nrm_bytes min_media_gb blurb`.
The optional `enabled` key accepts `yes` or `no` and defaults to `yes` for
backward compatibility.

- `revision` is a Hugging Face **repo commit**; downloads resolve
  `/resolve/<revision>/<file>`, so a moved branch cannot change what
  users receive.
- `sha256` is the artifact digest; current pins were computed locally
  and cross-checked against HF's `X-Linked-ETag` LFS hash.
- `targets` filters the catalog per selected target; `min_ram_gb` then filters
  against the RAM entered for the target. A qualifier like `rpi5-8gb` is also
  surfaced in the model description.

**Adding a model:** append a block with a new id, pin the revision
(`curl -sI .../resolve/main/file.gguf | grep -i x-repo-commit`), record
the sha256 (`x-linked-etag`, or sha256sum after a manual download), and
run `scripts/installer/tests/run.sh` (it validates required fields).
The family must be one NightRun supports (llama, qwen3, dense granite);
`nrconvert --inspect` is the gatekeeper at run time regardless.

**Disabling or removing a model:** set `model.<id>.enabled = no` to keep its
verified metadata in version control while removing it from installer choices.
Delete every `model.<id>.*` line only when the entry should be forgotten
permanently. Cached GGUF and NRM files are separate local artifacts and are not
deleted automatically.

Each additional compatible catalog entry automatically participates in the
RAM filter and best-fit recommendation through its `min_ram_gb` value. This
allows the catalog to grow without pretending arbitrary GGUF architectures are
supported.

## Downloads and verification

Downloads go to `models/<file>.part` (resumable, `curl -C -`), then must
pass, in order: exact size, SHA-256 against the manifest, GGUF magic
bytes, and `nrconvert --inspect` (real architecture/tensor metadata, not
the filename). Only then is the file atomically renamed into the cache.
A cached file is re-verified by checksum before reuse.

**Gated repositories:** a 401/403 is explained in plain language. If the
model requires accepting terms, you must do that on the official model
page first; the installer never tries to bypass access controls. A
token is taken from `HF_TOKEN` or prompted with echo disabled; it is held
in memory only, passed to curl via a private config stream (never on the
command line), and never logged or persisted. None of the current
catalog models are gated.

**Local GGUF:** the path is `~`-expanded and canonicalized; the file must
be a readable, non-empty regular file with GGUF magic, there must be
enough free space to convert and package it, and it must pass the same
`nrconvert --inspect` compatibility report.

## Build stage

`nrconvert` converts GGUF -> `.nrm` (skipped when a current `.nrm`
exists); `cargo xtask image|pi-image --model …` assembles the image. The
screen shows stage lines; full tool output lands in
`build/nightrun-installer-logs/<timestamp>/`. The image is hashed
immediately; that digest anchors both the pre-flash revalidation and the
post-flash verification. Failures name the stage, keep the logs, and
never delete verified cached downloads.

## Device safety rules

Only devices that pass **all** of these are ever listed:

- a whole disk (`TYPE=disk`), never a partition;
- USB transport, or an `mmcblk` SD/MMC device;
- not the disk backing `/`, `/boot`, `/home`, or active swap. Resolution
  walks the full device ancestry, so stacked storage (LVM, LUKS,
  LUKS-on-LVM) still resolves to the physical disk and excludes it;
- not a loop/zram/ram/optical/device-mapper/RAID device;
- at least as large as the image.

Internal disks (SATA/NVMe) are excluded even if they claim to be
removable: some enclosures and readers lie in both directions, which is
also why a USB disk *without* the removable flag is still offered (its
transport is the trustworthy signal). There is never a default device,
never an auto-pick, and an empty list is a normal state with a rescan
option.

Each selected device is fingerprinted (path + size + model + serial +
major:minor) and re-verified against a fresh scan at selection, before
unmounting, and again immediately before writing, so a device path that
silently changed meaning (drives re-enumerating) aborts instead of
flashing the wrong disk.

## Why the confirmation is the way it is

Two stages, both defaulting to "no": a menu confirmation, then typing
`FLASH /dev/sdX` **exactly**: current device path included, no `y`, no
case-folding, no partial credit. A destructive action should cost one
deliberate sentence; muscle-memory `y`-mashing has erased too many disks
in tooling history. After the confirmation, everything is revalidated
once more (existence, identity, mounts, capacity, not-a-system-disk,
image checksum) before the first byte is written.

## Flashing and verification

The write is `sudo dd` to the **whole device** in 4 MiB blocks, 128 MiB
slices (progress per slice), `oflag=direct`, `fsync` on the final slice,
then `sync` + `partprobe`. The exact command template and device are
printed. Ctrl+C mid-write prints a truthful "media is INCOMPLETE" message
and never claims success.

Verification (default on; skippable only past an explicit warning) reads
back **exactly the image-sized region** (not the device remainder) and
compares SHA-256 digests. A mismatch reports the media as unreliable,
does not call the drive bootable, and suggests re-flashing or a
different card. sudo is used only for the dd/umount/partprobe calls; the
script never stays root.

## Testing

- `scripts/installer/tests/run.sh`: no root, and no contact
  with real block devices: the media engine runs on fixtures
  (`tests/fixtures/lsblk-*.txt`) covering single/multiple USB, SD+USB,
  mounted targets, no-media, root-only hosts (SATA and NVMe),
  path-swap-after-rescan, vanished devices, too-small media, an internal
  disk falsely claiming removability, and an external disk without the
  removable flag; plus manifest parsing, confirmation matching, path
  validation, GGUF magic, rendering with and without color; live
  (read-only) verification that the running host's root disk resolves
  through whatever storage stack it uses; and the readback hashing math
  proven against odd-size images with slice and tail corruption, using
  regular files instead of devices.
- `sudo scripts/installer/tests/loopback.sh`: integration, the exact
  write/verify slice pattern against a loop device, including deliberate
  corruption detection. Root required; still never touches real media.
- Lint: ShellCheck-clean (`shellcheck -x install.sh
  scripts/installer/lib/*.sh scripts/installer/tests/run.sh`).

## Troubleshooting

| symptom | likely cause / fix |
|---|---|
| no drive found | not USB/SD, too small, or it backs the running system, all by design; insert a stick and Rescan |
| drive won't unmount | something is using it: close file managers/terminals in it, then retry; the blocking partition is named |
| insufficient capacity | media smaller than the image; the IMAGE READY card shows the minimum |
| download failure | network or HF hiccup; the `.part` file resumes on re-run |
| HF access denied | gated repo: accept terms on the model page; token via `HF_TOKEN` or the hidden prompt |
| GGUF incompatible | unsupported family (hybrid/SSM/MoE rejected by design); the inspect card explains |
| conversion failure | see `build/nightrun-installer-logs/<ts>/nrconvert.log` |
| image build failure | see `.../image.log`; Pi target additionally needs the firmware payload |
| verification mismatch | worn/faulty media or reader: re-flash, then try different media |
| boots to black screen (x86) | Secure Boot on, or wrong boot entry: pick the USB in the firmware boot menu |
| boots to black screen (Pi) | EEPROM older than 2025-06-09: see docs/rpi5-uefi.md |

## Debug mode

`NIGHTRUN_INSTALL_DEBUG=1 ./install.sh` runs `bash -x` style tracing into
the log directory. There is deliberately **no** non-interactive flashing
mode: destructive writes always require the interactive confirmations.

## Exit codes

`0` success · `2` preflight · `3` user abort · `4` build · `5` media ·
`6` flash · `7` verification.
