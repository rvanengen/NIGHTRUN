# shellcheck shell=bash disable=SC2034
# (SC2034: globals here are set for sibling modules sourced by install.sh)
# ui.sh — NightRun installer look & feel.
#
# The palette mirrors crates/nr-gfx/src/theme.rs (magenta/orange/cyan on a
# dark terminal), used as accents, never as the only signal: every status
# also carries a text marker (ok/warning/ERROR). Color is disabled when
# stdout is not a tty, when NO_COLOR is set, when TERM is dumb/absent, or
# with NIGHTRUN_NO_COLOR=1 — output stays clean monochrome.

NR_COLOR=0
if [[ -t 1 && -z "${NO_COLOR:-}" && -z "${NIGHTRUN_NO_COLOR:-}" ]]; then
    case "${TERM:-dumb}" in
        dumb | "") ;;
        *) NR_COLOR=1 ;;
    esac
fi

if (( NR_COLOR )); then
    C_MAG=$'\033[38;5;198m'   # neon magenta
    C_ORG=$'\033[38;5;215m'   # sunset orange
    C_CYN=$'\033[38;5;51m'    # neon cyan
    C_PUR=$'\033[38;5;141m'   # purple
    C_DIM=$'\033[38;5;103m'   # dim lavender
    C_TXT=$'\033[38;5;255m'
    C_BOLD=$'\033[1m'
    C_OFF=$'\033[0m'
else
    C_MAG="" C_ORG="" C_CYN="" C_PUR="" C_DIM="" C_TXT="" C_BOLD="" C_OFF=""
fi

NR_RULE="────────────────────────────────────────────────────────────"

nr_wordmark() {
    printf '%s\n' "${C_MAG}${C_BOLD}  N I G H T R U N${C_OFF} ${C_DIM}· image forge${C_OFF}"
}

nr_banner() {
    echo
    nr_wordmark
    printf '%s\n' "${C_PUR}${NR_RULE}${C_OFF}"
}

# Section heading: nr_section "SELECT NIGHTRUN TARGET"
nr_section() {
    echo
    printf '%s\n' "${C_ORG}${C_BOLD}$1${C_OFF}"
    printf '%s\n' "${C_PUR}${NR_RULE}${C_OFF}"
}

# Key/value line inside a card: nr_kv "Platform" "x86_64 UEFI"
nr_kv() {
    printf '  %s%-14s%s %s\n' "$C_DIM" "$1:" "$C_OFF" "$2"
}

nr_ok()    { printf '%s\n' "${C_CYN}  ok${C_OFF}      $1"; }
nr_warn()  { printf '%s\n' "${C_ORG}  warning${C_OFF} $1"; }
nr_error() { printf '%s\n' "${C_MAG}${C_BOLD}  ERROR${C_OFF}   $1" >&2; }
nr_note()  { printf '%s\n' "${C_DIM}  $1${C_OFF}"; }

# Menu item: nr_item "1" "x86_64 UEFI" "Boots from a USB drive..."
nr_item() {
    printf '  %s[%s]%s %s%s%s\n' "$C_CYN" "$1" "$C_OFF" "$C_TXT" "$2" "$C_OFF"
    if [[ -n "${3:-}" ]]; then
        printf '      %s%s%s\n' "$C_DIM" "$3" "$C_OFF"
    fi
}

# Checkbox menu item: nr_checkbox <0|1> "1" "Network stack" "detail".
nr_checkbox() {
    local mark=" "
    (( $1 )) && mark="x"
    printf '  %s[%s]%s %s[%s] %s%s\n' "$C_CYN" "$2" "$C_OFF" "$C_TXT" "$mark" "$3" "$C_OFF"
    if [[ -n "${4:-}" ]]; then
        printf '          %s%s%s\n' "$C_DIM" "$4" "$C_OFF"
    fi
}

# Prompt for a single choice. Args: prompt text. Reads into REPLY.
nr_ask() {
    printf '\n%s%s%s ' "$C_ORG" "$1" "$C_OFF"
    IFS= read -r REPLY
}

# Progress bar: nr_progress <done> <total> <label>. Redraws in place on a
# tty; prints occasional plain lines otherwise (no control codes in logs).
nr_progress() {
    local done="$1" total="$2" label="$3"
    local pct=0 width=36 filled
    (( total > 0 )) && pct=$(( done * 100 / total ))
    (( pct > 100 )) && pct=100
    filled=$(( pct * width / 100 ))
    if [[ -t 1 ]]; then
        local bar=""
        local i
        for (( i = 0; i < width; i++ )); do
            if (( i < filled )); then bar+="█"; else bar+="░"; fi
        done
        printf '\r  %s[%s]%s %3d%%  %s' "$C_MAG" "$bar" "$C_OFF" "$pct" "$label"
        (( pct >= 100 )) && echo
    else
        # Non-tty: line every 10%.
        if (( pct % 10 == 0 && pct != ${NR_LAST_PCT:--1} )); then
            printf '  %3d%%  %s\n' "$pct" "$label"
            NR_LAST_PCT=$pct
        fi
    fi
}
