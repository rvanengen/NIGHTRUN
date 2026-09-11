# shellcheck shell=bash disable=SC2034
# (SC2034: globals here are set for sibling modules sourced by install.sh)
# models.sh — manifest parsing, target-filtered catalog, model cards, and
# local-GGUF validation.
#
# The manifest (config/models.manifest) is data: it is parsed line by line
# and never sourced. Values land in the associative array NR_MF keyed
# "id.key"; NR_MODEL_IDS holds catalog order.

declare -A NR_MF=()
NR_MODEL_IDS=()

# Outcome globals of nr_select_model:
#   NR_MODEL_ID     catalog id, or "local"
#   NR_MODEL_NAME   display name
#   NR_MODEL_*      quant/family/... (catalog models)
#   NR_GGUF_PATH    set later by acquisition (local: immediately)

nr_manifest_load() {
    local file="$1" line key val id
    NR_MODEL_IDS=()
    while IFS= read -r line || [[ -n "$line" ]]; do
        line="${line%%#*}"
        [[ "$line" =~ ^[[:space:]]*$ ]] && continue
        if [[ "$line" =~ ^[[:space:]]*([a-zA-Z0-9._-]+)[[:space:]]*=[[:space:]]*(.*)$ ]]; then
            key="${BASH_REMATCH[1]}"
            val="${BASH_REMATCH[2]}"
            val="${val%"${val##*[![:space:]]}"}" # rtrim
            if [[ "$key" == model.*.* ]]; then
                id="${key#model.}"
                id="${id%%.*}"
                NR_MF["$id.${key#"model.$id."}"]="$val"
                # First mention of an id defines catalog order.
                local seen=0 m
                for m in "${NR_MODEL_IDS[@]}"; do [[ "$m" == "$id" ]] && seen=1; done
                (( seen )) || NR_MODEL_IDS+=("$id")
            elif [[ "$key" == "manifest.version" ]]; then
                NR_MF["manifest.version"]="$val"
            fi
        else
            nr_warn "manifest: ignored malformed line: ${line}"
        fi
    done < "$file"

    [[ "${NR_MF[manifest.version]:-}" == "1" ]] || {
        nr_error "Unsupported manifest version '${NR_MF[manifest.version]:-none}' in $file"
        return 1
    }

    # Every model must carry the fields the flow depends on.
    local required=(name family quant repo file revision sha256 size_bytes license gated min_ram_gb targets nrm_bytes min_media_gb)
    local mid f
    for mid in "${NR_MODEL_IDS[@]}"; do
        for f in "${required[@]}"; do
            [[ -n "${NR_MF[$mid.$f]:-}" ]] || {
                nr_error "manifest: model '$mid' is missing '$f'"
                return 1
            }
        done
        case "${NR_MF[$mid.enabled]:-yes}" in
            yes | no) ;;
            *)
                nr_error "manifest: model '$mid' has invalid enabled='${NR_MF[$mid.enabled]}' (use yes or no)"
                return 1
                ;;
        esac
    done
    return 0
}

# Missing `enabled` preserves compatibility with older manifests. A disabled
# entry retains its pins and metadata but is absent from installer choices.
nr_model_enabled() {
    [[ "${NR_MF[$1.enabled]:-yes}" == "yes" ]]
}

# Does model $1 support target $2? Target tokens in the manifest may carry
# qualifiers (rpi5-8gb); the base target must match, and qualifiers are
# surfaced as notes rather than hidden filters.
nr_model_supports() {
    local mid="$1" target="$2" tok
    local IFS=,
    for tok in ${NR_MF[$mid.targets]}; do
        [[ "$tok" == "$target" || "$tok" == "$target"-* ]] && return 0
    done
    return 1
}

# Qualifier note for target (e.g. "8gb" from rpi5-8gb), empty if none.
nr_model_target_note() {
    local mid="$1" target="$2" tok
    local IFS=,
    for tok in ${NR_MF[$mid.targets]}; do
        if [[ "$tok" == "$target"-* ]]; then
            printf '%s' "${tok#"$target"-}"
            return 0
        fi
    done
    return 0
}

nr_model_fits_memory() {
    local mid="$1" ram_gb="$2" need
    [[ "$ram_gb" =~ ^[0-9]+$ ]] || return 1
    need="${NR_MF[$mid.min_ram_gb]}"
    (( need <= ram_gb ))
}

nr_detect_host_ram_gb() {
    local kb
    kb="$(sed -n 's/^MemTotal:[[:space:]]*\([0-9]*\).*/\1/p' /proc/meminfo 2>/dev/null)"
    [[ "$kb" =~ ^[0-9]+$ ]] || return 1
    printf '%s' "$(( (kb + 1048575) / 1048576 ))"
}

nr_select_target_memory() {
    local suggested detected
    if [[ "$NR_TARGET" == "rpi5" ]]; then
        suggested=8
    else
        detected="$(nr_detect_host_ram_gb || true)"
        suggested="${detected:-8}"
    fi
    while :; do
        nr_section "TARGET MEMORY"
        nr_note "This is the RAM in the machine that will boot NightRun, not disk space."
        [[ -n "${detected:-}" ]] && nr_kv "Detected here" "${detected} GB"
        nr_ask "Target RAM in GB [${suggested}] (B to go back):"
        [[ -z "$REPLY" ]] && REPLY="$suggested"
        case "$REPLY" in
            [bB]) return 1 ;;
            *)
                if [[ "$REPLY" =~ ^[0-9]+$ ]] && (( REPLY >= 2 && REPLY <= 1024 )); then
                    NR_TARGET_RAM_GB="$REPLY"
                    return 0
                fi
                nr_warn "Enter a whole number from 2 to 1024."
                ;;
        esac
    done
}

nr_recommended_model() {
    local target="$1" ram_gb="$2" mid need bytes best="" best_ram=-1 best_bytes=-1
    for mid in "${NR_MODEL_IDS[@]}"; do
        nr_model_enabled "$mid" || continue
        nr_model_supports "$mid" "$target" || continue
        nr_model_fits_memory "$mid" "$ram_gb" || continue
        need="${NR_MF[$mid.min_ram_gb]}"
        bytes="${NR_MF[$mid.nrm_bytes]}"
        if (( need > best_ram )) || (( need == best_ram && bytes > best_bytes )); then
            best="$mid"
            best_ram="$need"
            best_bytes="$bytes"
        fi
    done
    printf '%s' "$best"
}

nr_select_model() {
    local choices=() excluded=() mid
    for mid in "${NR_MODEL_IDS[@]}"; do
        nr_model_enabled "$mid" || continue
        if nr_model_supports "$mid" "$NR_TARGET"; then
            if nr_model_fits_memory "$mid" "$NR_TARGET_RAM_GB"; then
                choices+=("$mid")
            else
                excluded+=("$mid")
            fi
        fi
    done
    local recommended
    recommended="$(nr_recommended_model "$NR_TARGET" "$NR_TARGET_RAM_GB")"

    while :; do
        nr_section "SELECT MODEL"
        nr_kv "Target RAM" "${NR_TARGET_RAM_GB} GB"
        local i=1 note extra
        for mid in "${choices[@]}"; do
            note="$(nr_model_target_note "$mid" "$NR_TARGET")"
            extra="requires ~${NR_MF[$mid.min_ram_gb]} GB RAM"
            [[ "$note" == "8gb" ]] && extra+=" · needs an 8 GB Raspberry Pi 5"
            [[ "$mid" == "$recommended" ]] && extra+=" · recommended best fit"
            nr_item "$i" "${NR_MF[$mid.name]} — ${NR_MF[$mid.quant]}" \
                    "${NR_MF[$mid.blurb]:-} · $extra"
            (( i++ ))
        done
        if (( ${#excluded[@]} )); then
            local hidden=""
            for mid in "${excluded[@]}"; do
                hidden+="${NR_MF[$mid.name]} (${NR_MF[$mid.min_ram_gb]} GB), "
            done
            nr_note "Hidden for insufficient RAM: ${hidden%, }"
        fi
        nr_item "$i" "Use a local GGUF file" "Bring your own model — it will be inspected before use."
        nr_item "B" "Back"
        nr_item "Q" "Quit"
        nr_ask "Choose a model:"
        case "$REPLY" in
            [bB]) return 1 ;;
            [qQ]) nr_note "Nothing was changed."; exit "$EXIT_USER_ABORT" ;;
            '') nr_warn "Please pick an option." ;;
            *)
                if [[ "$REPLY" =~ ^[0-9]+$ ]] && (( REPLY >= 1 && REPLY <= ${#choices[@]} )); then
                    NR_MODEL_ID="${choices[REPLY-1]}"
                    nr_show_model_card "$NR_MODEL_ID"
                    return 0
                elif [[ "$REPLY" =~ ^[0-9]+$ ]] && (( REPLY == ${#choices[@]} + 1 )); then
                    nr_pick_local_gguf && return 0
                else
                    nr_warn "Please pick an option from the list."
                fi
                ;;
        esac
    done
}

nr_show_model_card() {
    local mid="$1"
    NR_MODEL_NAME="${NR_MF[$mid.name]}"
    nr_section "MODEL SELECTED"
    nr_kv "Name"          "${NR_MF[$mid.name]}"
    nr_kv "Architecture"  "${NR_MF[$mid.family]} (dense transformer)"
    nr_kv "Quantization"  "${NR_MF[$mid.quant]}"
    nr_kv "Provider"      "${NR_MF[$mid.repo]} (${NR_MF[$mid.license]})"
    nr_kv "Required RAM"  "${NR_MF[$mid.min_ram_gb]} GB on the target machine"
    nr_kv "Available RAM" "${NR_TARGET_RAM_GB} GB selected"
    nr_kv "Disk required" "$(nr_human_bytes "${NR_MF[$mid.size_bytes]}") download + $(nr_human_bytes "${NR_MF[$mid.nrm_bytes]}") converted"
    nr_kv "Target"        "$NR_TARGET_LABEL"
}

# Local GGUF path: expand, canonicalize, verify shape, then inspect with
# the real converter. Sets NR_MODEL_ID=local, NR_GGUF_PATH, NR_MODEL_NAME.
nr_pick_local_gguf() {
    nr_section "LOCAL GGUF"
    nr_ask "Path to the .gguf file:"
    local raw="$REPLY" path
    [[ -n "$raw" ]] || { nr_warn "No path given."; return 1; }
    path="$(nr_canon_path "$raw")" || { nr_error "Path not found: $raw"; return 1; }
    [[ -f "$path" ]] || { nr_error "Not a regular file: $path"; return 1; }
    [[ -r "$path" ]] || { nr_error "Not readable: $path"; return 1; }
    [[ -s "$path" ]] || { nr_error "File is empty: $path"; return 1; }
    nr_check_gguf_magic "$path" || {
        nr_error "This is not a GGUF file (magic bytes mismatch): $path"
        return 1
    }

    # Space: conversion needs roughly the GGUF size again, image the same.
    local sz free
    sz="$(stat -c %s -- "$path")"
    free="$(nr_free_bytes "$NR_ROOT")"
    if [[ -n "$free" ]] && (( free < sz * 2 )); then
        nr_error "Not enough free space to convert and package this model"
        nr_note  "(need ~$(nr_human_bytes "$(( sz * 2 ))"), have $(nr_human_bytes "$free"))."
        return 1
    fi

    nr_inspect_gguf "$path" "LOCAL MODEL CHECK" || return 1
    NR_MODEL_ID="local"
    NR_GGUF_PATH="$path"
    return 0
}

nr_check_gguf_magic() {
    local magic
    magic="$(head -c 4 -- "$1" 2>/dev/null)"
    [[ "$magic" == "GGUF" ]]
}

# Run `nrconvert --inspect` and render a compatibility card. Also derives
# NR_MODEL_NAME for local files. Returns 1 when the family is unsupported.
nr_inspect_gguf() {
    local path="$1" title="$2" out
    nr_note "Inspecting model metadata (nrconvert --inspect)..."
    if ! out="$(cargo run --release -q -p nrconvert -- --inspect "$path" 2>&1)"; then
        nr_error "The converter rejected this file:"
        printf '%s\n' "$out" | tail -5 | sed 's/^/    /'
        return 1
    fi
    # First line: arch=<fam> name="<display>" tensors=N; second: verdict.
    local head1 arch name verdict
    head1="$(printf '%s\n' "$out" | head -1)"
    arch="$(printf '%s\n' "$head1" | sed -n 's/^arch=\([a-z0-9_-]*\).*/\1/p')"
    name="$(printf '%s\n' "$head1" | sed -n 's/.*name="\([^"]*\)".*/\1/p')"
    verdict="$(printf '%s\n' "$out" | sed -n 's/^verdict: *//p' | head -1)"

    nr_section "$title"
    nr_kv "File"          "$path"
    nr_kv "Architecture"  "${arch:-unknown}"
    nr_kv "Model"         "${name:-unknown}"
    nr_kv "Verdict"       "${verdict:-unknown}"
    nr_kv "Target"        "$NR_TARGET_LABEL"
    case "$arch:$verdict" in
        llama:* | qwen3:* | granite*:*supported*)
            nr_kv "Compatibility" "READY"
            NR_MODEL_NAME="${name:-$(basename "$path")}"
            return 0
            ;;
        *)
            nr_kv "Compatibility" "NOT SUPPORTED"
            nr_error "NightRun supports the llama, qwen3 and dense-granite families."
            nr_note  "Hybrid/SSM and MoE artifacts are rejected by design."
            return 1
            ;;
    esac
}
