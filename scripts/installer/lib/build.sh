# shellcheck shell=bash disable=SC2034
# (SC2034: globals here are set for sibling modules sourced by install.sh)
# build.sh — GGUF → .nrm conversion and target-specific image assembly.
#
# Bash orchestrates; the real work is done by the tested repository tools
# (tools/nrconvert and the xtask image builders). Verbose tool output goes
# to build/nightrun-installer-logs/<ts>/, the screen gets stage lines.

# Outcome globals:
#   NR_NRM_PATH     converted model
#   NR_IMAGE_PATH   final target-specific image
#   NR_IMAGE_BYTES  its size
#   NR_MIN_MEDIA_GB minimum media capacity

nr_build_image() {
    mkdir -p -- "$NR_LOG_DIR"

    # -- Convert (or reuse) the .nrm ------------------------------------
    local nrm
    if [[ "$NR_MODEL_ID" == "local" ]]; then
        nrm="$NR_ROOT/models/$(basename "${NR_GGUF_PATH%.gguf}").nrm"
    else
        # Catalog models keep the repo's canonical .nrm names, so images
        # built here are identical to hand-built ones.
        case "$NR_MODEL_ID" in
            llama-1b)   nrm="$NR_ROOT/models/model.nrm" ;;
            granite-3b) nrm="$NR_ROOT/models/granite-4.1-3b-q4km.nrm" ;;
            qwen3-4b)   nrm="$NR_ROOT/models/qwen3-4b-q4km.nrm" ;;
            *)          nrm="$NR_ROOT/models/${NR_MODEL_ID}.nrm" ;;
        esac
    fi

    nr_section "BUILDING"
    if [[ -f "$nrm" && "$nrm" -nt "$NR_GGUF_PATH" ]]; then
        nr_ok "Converted model is current: $(basename "$nrm") (reused)"
    else
        nr_note "Converting to NightRun format (nrconvert; validates its own output)..."
        local log="$NR_LOG_DIR/nrconvert.log"
        if ! cargo run --release -q -p nrconvert -- "$NR_GGUF_PATH" "$nrm" >"$log" 2>&1; then
            nr_error "Model conversion failed. Last lines:"
            tail -5 "$log" | sed 's/^/    /'
            nr_note "Full log: $log"
            rm -f -- "$nrm"
            return 1
        fi
        nr_ok "Converted: $(basename "$nrm") ($(nr_human_bytes "$(stat -c %s -- "$nrm")"))"
    fi
    NR_NRM_PATH="$nrm"

    # -- Assemble the target-specific image ------------------------------
    local log="$NR_LOG_DIR/image.log" xtask_cmd image
    if [[ "$NR_TARGET" == "rpi5" ]]; then
        xtask_cmd=(cargo xtask pi-image --model "$nrm")
        image="$NR_ROOT/nightrun-pi5.img"
        nr_note "Assembling the Raspberry Pi 5 SD image (firmware + DTBs + BOOTAA64.EFI + model)..."
    else
        xtask_cmd=(cargo xtask image --model "$nrm")
        image="$NR_ROOT/nightrun.img"
        nr_note "Assembling the x86_64 UEFI image (GPT/ESP + BOOTX64.EFI + model)..."
    fi
    if (( NR_ENABLE_MCP )); then
        xtask_cmd+=(--mcp)
    elif (( NR_ENABLE_NETWORK )); then
        xtask_cmd+=(--network)
    fi
    if ! (cd "$NR_ROOT" && "${xtask_cmd[@]}") >"$log" 2>&1; then
        nr_error "Image assembly failed at the '${xtask_cmd[*]}' stage. Last lines:"
        tail -5 "$log" | sed 's/^/    /'
        nr_note "Full log: $log"
        return 1
    fi
    [[ -f "$image" ]] || { nr_error "Builder reported success but $image is missing."; return 1; }

    NR_IMAGE_PATH="$image"
    NR_IMAGE_BYTES="$(stat -c %s -- "$image")"
    # Minimum media: image + safety margin, rounded up to a whole GB.
    NR_MIN_MEDIA_GB=$(( (NR_IMAGE_BYTES + NR_IMAGE_BYTES / 10) / 1073741824 + 1 ))

    # Integrity anchor for flashing AND verification later.
    nr_note "Hashing the image (used to verify the flashed media)..."
    NR_IMAGE_SHA="$(sha256sum -- "$image" | cut -d' ' -f1)"

    nr_section "IMAGE READY"
    nr_kv "Target"        "$NR_TARGET_LABEL"
    nr_kv "Model"         "$NR_MODEL_NAME"
    nr_kv "Stacks"        "$(nr_stack_label)"
    nr_kv "Image file"    "$image"
    nr_kv "Image size"    "$(nr_human_bytes "$NR_IMAGE_BYTES")"
    nr_kv "SHA-256"       "${NR_IMAGE_SHA:0:16}…"
    nr_kv "Minimum media" "${NR_MIN_MEDIA_GB} GB"
    return 0
}
