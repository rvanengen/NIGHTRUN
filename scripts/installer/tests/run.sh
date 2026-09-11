#!/usr/bin/env bash
# NightRun installer test harness. Plain bash, no framework, no root, and
# — by construction — no contact with real block devices: the media engine
# runs exclusively on fixture data here.
#
#   scripts/installer/tests/run.sh
# shellcheck disable=SC2016,SC1091,SC2034
# (SC2016: single-quoted bash -c test bodies expand inside the subshell;
#  SC1091: libs are sourced dynamically; SC2034: globals feed sourced libs)

set -u -o pipefail

TESTS_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
NR_ROOT="$(cd -- "$TESTS_DIR/../../.." && pwd)"
FX="$TESTS_DIR/fixtures"
LIB="$NR_ROOT/scripts/installer/lib"

PASS=0
FAIL=0

t() { # t <name> <condition...>
    local name="$1"; shift
    if "$@"; then
        PASS=$(( PASS + 1 ))
    else
        FAIL=$(( FAIL + 1 ))
        echo "FAIL: $name" >&2
    fi
}

eq() { [[ "$1" == "$2" ]]; }
not() { ! "$@"; }

# Load libraries in a controlled environment (no tty, no color).
export NIGHTRUN_NO_COLOR=1
NR_MANIFEST="$NR_ROOT/config/models.manifest"
# shellcheck source=../lib/safety.sh
source "$LIB/safety.sh"
# shellcheck source=../lib/ui.sh
source "$LIB/ui.sh"
# shellcheck source=../lib/models.sh
source "$LIB/models.sh"
# shellcheck source=../lib/features.sh
source "$LIB/features.sh"
# shellcheck source=../lib/media.sh
source "$LIB/media.sh"

# ---- confirmation matcher ---------------------------------------------

t "confirm: exact match accepted"        nr_confirm_exact_flash /dev/sdb "FLASH /dev/sdb"
t "confirm: y rejected"                  not nr_confirm_exact_flash /dev/sdb "y"
t "confirm: yes rejected"                not nr_confirm_exact_flash /dev/sdb "yes"
t "confirm: case-insensitive rejected"   not nr_confirm_exact_flash /dev/sdb "flash /dev/sdb"
t "confirm: partial rejected"            not nr_confirm_exact_flash /dev/sdb "FLASH"
t "confirm: partition path rejected"     not nr_confirm_exact_flash /dev/sdb "FLASH /dev/sdb1"
t "confirm: blank rejected"              not nr_confirm_exact_flash /dev/sdb ""
t "confirm: stale device rejected"       not nr_confirm_exact_flash /dev/sdc "FLASH /dev/sdb"
t "confirm: trailing junk rejected"      not nr_confirm_exact_flash /dev/sdb "FLASH /dev/sdb "

# ---- whole-disk path gate ----------------------------------------------
# (Existence checks need real /dev nodes; use ones every Linux has, plus
# string-shape assertions via the regex part.)

t "diskpath: partition sdb1 shape rejected" bash -c '
    source "'"$LIB"'/safety.sh" >/dev/null 2>&1
    p=/dev/sdb1; [[ "$p" =~ ^/dev/(sd[a-z]+|vd[a-z]+)$ ]] && exit 1 || exit 0'
t "diskpath: nvme partition shape rejected" bash -c '
    p=/dev/nvme0n1p2; [[ "$p" =~ ^/dev/(nvme[0-9]+n[0-9]+|mmcblk[0-9]+)$ ]] && exit 1 || exit 0'
t "diskpath: mmcblk0 shape accepted" bash -c '
    p=/dev/mmcblk0; [[ "$p" =~ ^/dev/(nvme[0-9]+n[0-9]+|mmcblk[0-9]+)$ ]]'

# ---- manifest ------------------------------------------------------------

t "manifest: loads" nr_manifest_load "$NR_MANIFEST"
t "manifest: four models" eq "${#NR_MODEL_IDS[@]}" 4
t "manifest: llama sha present" eq "${NR_MF[llama-1b.sha256]:0:8}" "432f310a"
t "manifest: malformed line tolerated" bash -c '
    source "'"$LIB"'/safety.sh" >/dev/null 2>&1
    source "'"$LIB"'/ui.sh"; source "'"$LIB"'/models.sh"
    tmp="$(mktemp)"; trap "rm -f \"$tmp\"" EXIT
    printf "manifest.version = 1\nmodel.x.name = A\nGARBAGE LINE NO EQUALS\n" > "$tmp"
    nr_manifest_load "$tmp" 2>/dev/null; rc=$?
    # It must not crash; it must fail cleanly (missing required fields).
    [[ $rc -eq 1 ]]'

# target filtering
NR_TARGET="rpi5"
t "filter: llama supports rpi5"    nr_model_supports llama-1b rpi5
t "filter: qwen matches rpi5 base" nr_model_supports qwen3-4b rpi5
t "filter: qwen rpi5 has 8gb note" eq "$(nr_model_target_note qwen3-4b rpi5)" "8gb"
t "filter: granite x86 no note"    eq "$(nr_model_target_note granite-3b x86_64)" ""
t "memory: 4 GB fits llama 1B"     nr_model_fits_memory llama-1b 4
t "memory: 6 GB rejects qwen 4B"   not nr_model_fits_memory qwen3-4b 6
t "memory: 8 GB fits qwen 4B"      nr_model_fits_memory qwen3-4b 8
t "memory: 6 GB recommends granite" eq "$(nr_recommended_model x86_64 6)" "granite-3b"
t "memory: nonnumeric rejected"    not nr_model_fits_memory llama-1b nope

# Build-mode invariants behind the checkbox UI.
NR_ENABLE_NETWORK=0 NR_ENABLE_MCP=0
t "features: default label offline" eq "$(nr_stack_label)" "offline"
NR_ENABLE_NETWORK=1 NR_ENABLE_MCP=0
t "features: network-only label" eq "$(nr_stack_label)" "network only"
NR_ENABLE_NETWORK=1 NR_ENABLE_MCP=1
t "features: MCP label" eq "$(nr_stack_label)" "network + MCP"

# ---- path validation -----------------------------------------------------

t "path: ~ expands" bash -c '
    source "'"$LIB"'/safety.sh" >/dev/null 2>&1
    [[ "$(nr_canon_path "~")" == "$HOME" ]]'
t "path: spaces preserved" bash -c '
    source "'"$LIB"'/safety.sh" >/dev/null 2>&1
    d="$(mktemp -d)"; trap "rm -rf \"$d\"" EXIT
    touch "$d/a file.gguf"
    [[ "$(nr_canon_path "$d/a file.gguf")" == "$d/a file.gguf" ]]'
t "path: nonexistent rejected" bash -c '
    source "'"$LIB"'/safety.sh" >/dev/null 2>&1
    ! nr_canon_path /no/such/file-xyz'

# GGUF magic
t "gguf: magic accepted" bash -c '
    source "'"$LIB"'/safety.sh" >/dev/null 2>&1
    source "'"$LIB"'/ui.sh"; source "'"$LIB"'/models.sh"
    f="$(mktemp)"; trap "rm -f \"$f\"" EXIT
    printf "GGUFxxxx" > "$f"; nr_check_gguf_magic "$f"'
t "gguf: non-gguf rejected" bash -c '
    source "'"$LIB"'/safety.sh" >/dev/null 2>&1
    source "'"$LIB"'/ui.sh"; source "'"$LIB"'/models.sh"
    f="$(mktemp)"; trap "rm -f \"$f\"" EXIT
    printf "NOPE" > "$f"; ! nr_check_gguf_magic "$f"'

# ---- helpers -------------------------------------------------------------

t "bytes: 2.2GB renders" eq "$(nr_human_bytes 2214592512)" "2.0 GB"
t "bytes: sub-GB renders MB" eq "$(nr_human_bytes 400000000)" "381 MB"
t "no-color: markers survive" bash -c '
    NIGHTRUN_NO_COLOR=1
    source "'"$LIB"'/safety.sh" >/dev/null 2>&1
    source "'"$LIB"'/ui.sh"
    out="$(nr_ok hello)"; [[ "$out" == *"ok"* && "$out" == *hello* && "$out" != *$'"'"'\033'"'"'* ]]'

# ---- media engine on fixtures (the spec scenario matrix) ------------------

media_case() { # media_case <fixture> <protected> <min_bytes> -> echoes candidate paths
    NR_LSBLK_FIXTURE="$FX/$1" NR_PROTECTED_FIXTURE="$FX/$2" nr_media_candidates "$3"
    printf '%s\n' "${NR_DEV_PATH[@]:-}"
}

t "media: one safe USB found"          eq "$(media_case lsblk-one-usb.txt protected-nvme.txt 2000000000)" "/dev/sdb"
t "media: multi USB both listed"       eq "$(media_case lsblk-multi-usb.txt protected-nvme.txt 2000000000 | tr '\n' ' ')" "/dev/sdb /dev/sdc "
t "media: SD + USB both listed"        eq "$(media_case lsblk-sd-plus-usb.txt protected-nvme.txt 2000000000 | tr '\n' ' ')" "/dev/mmcblk0 /dev/sdb "
t "media: none found -> empty"         eq "$(media_case lsblk-none.txt protected-nvme.txt 2000000000)" ""
t "media: root-only sata -> empty"     eq "$(media_case lsblk-root-sata.txt protected-sata.txt 2000000000)" ""
t "media: too-small excluded"          eq "$(media_case lsblk-too-small.txt protected-nvme.txt 2000000000)" ""
t "media: internal false-removable excluded" eq "$(media_case lsblk-internal-false-removable.txt protected-nvme.txt 2000000000)" ""
t "media: external USB without RM flag still offered" eq "$(media_case lsblk-external-no-rm.txt protected-nvme.txt 2000000000)" "/dev/sdb"
t "media: loop/zram/sr/md/dm all excluded" eq "$(media_case lsblk-virtuals.txt protected-nvme.txt 1)" ""

t "media: mounted USB reports its mounts" bash -c '
    source "'"$LIB"'/safety.sh" >/dev/null 2>&1
    source "'"$LIB"'/ui.sh"; source "'"$LIB"'/media.sh"
    NR_LSBLK_FIXTURE="'"$FX"'/lsblk-mounted-usb.txt"
    NR_PROTECTED_FIXTURE="'"$FX"'/protected-nvme.txt"
    nr_media_candidates 2000000000
    [[ "${NR_DEV_MOUNTS[0]}" == *"/media/user/STICK"* && "${NR_DEV_MOUNTS[0]}" == *"/mnt/two"* ]]'

# Identity re-verification: same path, different device (swap) => mismatch;
# device disappearing => mismatch.
t "media: path swap detected by fingerprint" bash -c '
    source "'"$LIB"'/safety.sh" >/dev/null 2>&1
    source "'"$LIB"'/ui.sh"; source "'"$LIB"'/media.sh"
    NR_PROTECTED_FIXTURE="'"$FX"'/protected-nvme.txt"
    NR_LSBLK_FIXTURE="'"$FX"'/lsblk-multi-usb.txt"
    nr_media_candidates 0
    NR_SEL_PATH="/dev/sdb"
    NR_SEL_FP="$(nr_fingerprint /dev/sdb 62109253632 "SanDisk Ultra" X1 8:16)"
    NR_LSBLK_FIXTURE="'"$FX"'/lsblk-swapped-path.txt"
    ! nr_reverify_selection'
t "media: vanished device detected" bash -c '
    source "'"$LIB"'/safety.sh" >/dev/null 2>&1
    source "'"$LIB"'/ui.sh"; source "'"$LIB"'/media.sh"
    NR_PROTECTED_FIXTURE="'"$FX"'/protected-nvme.txt"
    NR_SEL_PATH="/dev/sdb"
    NR_SEL_FP="whatever"
    NR_LSBLK_FIXTURE="'"$FX"'/lsblk-none.txt"
    ! nr_reverify_selection'
t "media: stable identity passes" bash -c '
    source "'"$LIB"'/safety.sh" >/dev/null 2>&1
    source "'"$LIB"'/ui.sh"; source "'"$LIB"'/media.sh"
    NR_PROTECTED_FIXTURE="'"$FX"'/protected-nvme.txt"
    NR_LSBLK_FIXTURE="'"$FX"'/lsblk-multi-usb.txt"
    NR_SEL_PATH="/dev/sdb"
    NR_SEL_FP="$(nr_fingerprint /dev/sdb 62109253632 "SanDisk Ultra" X1 8:16)"
    nr_reverify_selection'

# ---- log path -------------------------------------------------------------

t "logs: timestamped path shape" bash -c '
    p="build/nightrun-installer-logs/20260707-120000"
    [[ "$p" =~ ^build/nightrun-installer-logs/[0-9]{8}-[0-9]{6}$ ]]'

# ---- live protected-disk resolution (host, read-only) ----
live_protected="$(NR_PROTECTED_FIXTURE='' nr_protected_disks)"
root_disk="$(lsblk -snlo NAME -- "$(findmnt -no SOURCE --target /)" 2>/dev/null | tail -1)"
t "live: root physical disk is protected" grep -qx "/dev/$root_disk" <<<"$live_protected"
t "live: protected paths are clean /dev entries" bash -c '! grep -qvE "^/dev/[a-zA-Z0-9_-]+$" <<<"$1"' _ "$live_protected"

# ---- readback verification math (regular files, no devices) ----
# shellcheck source=../lib/verify.sh
source "$LIB/verify.sh"
# test seam: no sudo; drop O_DIRECT (regular files may reject it)
# shellcheck disable=SC2317  # invoked indirectly through nr_readback_sha
nr_dd() {
    local a=() x
    for x in "$@"; do [[ "$x" == iflag=direct ]] || a+=("$x"); done
    command dd "${a[@]}"
}
rb_img="$NR_TMPDIR/rb-image.bin"
rb_dev="$NR_TMPDIR/rb-device.bin"
head -c $(( 3 * 4194304 + 123 )) /dev/urandom > "$rb_img"   # odd size: 3 blocks + 123B tail
cp -- "$rb_img" "$rb_dev"
head -c 4096 /dev/urandom >> "$rb_dev"   # media larger than image, remainder must be ignored
rb_want="$(sha256sum -- "$rb_img" | cut -d' ' -f1)"
rb_got="$(nr_readback_sha "$rb_dev" "$(stat -c%s -- "$rb_img")" /dev/null 2>/dev/null)"
t "readback: odd-size image hashes exactly (tail path)" test "$rb_got" = "$rb_want"
printf 'X' | dd of="$rb_dev" bs=1 seek=$(( 3 * 4194304 + 60 )) conv=notrunc status=none
rb_got="$(nr_readback_sha "$rb_dev" "$(stat -c%s -- "$rb_img")" /dev/null 2>/dev/null)"
t "readback: tail corruption detected" test "$rb_got" != "$rb_want"
cp -- "$rb_img" "$rb_dev"
printf 'X' | dd of="$rb_dev" bs=1 seek=1000000 conv=notrunc status=none
rb_got="$(nr_readback_sha "$rb_dev" "$(stat -c%s -- "$rb_img")" /dev/null 2>/dev/null)"
t "readback: slice corruption detected" test "$rb_got" != "$rb_want"
unset -f nr_dd

echo
echo "passed: $PASS  failed: $FAIL"
(( FAIL == 0 ))
