#!/usr/bin/env bash
# NightRun — interactive installer, image builder and safe USB/SD flasher.
#
#   ./install.sh
#
# Guides you from target + model choice through building a real NightRun
# boot image to safely flashing and verifying removable media. See
# docs/installer.md for the full documentation and safety rationale.

NR_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly NR_ROOT
readonly NR_MANIFEST="$NR_ROOT/config/models.manifest"
readonly NR_LIB="$NR_ROOT/scripts/installer/lib"
NR_LOG_DIR="$NR_ROOT/build/nightrun-installer-logs/$(date +%Y%m%d-%H%M%S)"
readonly NR_LOG_DIR

# shellcheck source=scripts/installer/lib/safety.sh
source "$NR_LIB/safety.sh"
# shellcheck source=scripts/installer/lib/ui.sh
source "$NR_LIB/ui.sh"
# shellcheck source=scripts/installer/lib/preflight.sh
source "$NR_LIB/preflight.sh"
# shellcheck source=scripts/installer/lib/targets.sh
source "$NR_LIB/targets.sh"
# shellcheck source=scripts/installer/lib/models.sh
source "$NR_LIB/models.sh"
# shellcheck source=scripts/installer/lib/features.sh
source "$NR_LIB/features.sh"
# shellcheck source=scripts/installer/lib/downloads.sh
source "$NR_LIB/downloads.sh"
# shellcheck source=scripts/installer/lib/build.sh
source "$NR_LIB/build.sh"
# shellcheck source=scripts/installer/lib/media.sh
source "$NR_LIB/media.sh"
# shellcheck source=scripts/installer/lib/flash.sh
source "$NR_LIB/flash.sh"
# shellcheck source=scripts/installer/lib/verify.sh
source "$NR_LIB/verify.sh"

# Debug mode: full bash tracing into the log dir (screen stays clean).
# Token-handling code paths disable tracing locally so secrets never
# reach the trace (see downloads.sh).
if [[ -n "${NIGHTRUN_INSTALL_DEBUG:-}" ]]; then
    mkdir -p -- "$NR_LOG_DIR"
    exec 9>>"$NR_LOG_DIR/trace.log"
    export BASH_XTRACEFD=9
    set -x
fi

main() {
    nr_banner
    nr_note "Builds a bootable NightRun image and writes it to a USB drive or SD card."
    nr_note "Nothing is written to any disk without an exact typed confirmation."

    nr_preflight
    nr_manifest_load "$NR_MANIFEST" || exit "$EXIT_PREFLIGHT"

    # Target → target RAM → model → optional stacks.
    while :; do
        nr_select_target
        nr_select_target_memory || continue
        nr_select_model || continue
        nr_select_features && break
    done

    # Acquire the GGUF (catalog models only; local files arrive validated).
    if [[ "$NR_MODEL_ID" != "local" ]]; then
        nr_acquire_model || exit "$EXIT_BUILD"
    fi

    # Convert + assemble the target-specific image.
    nr_build_image || exit "$EXIT_BUILD"

    # Pick media, confirm, flash, verify.
    nr_select_media || exit "$EXIT_MEDIA"
    nr_flash_flow   || exit "$EXIT_FLASH"
    nr_verify_flow  || exit "$EXIT_VERIFY"

    nr_completion_screen
}

main "$@"
