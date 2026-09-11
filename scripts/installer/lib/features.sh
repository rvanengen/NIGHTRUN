# shellcheck shell=bash disable=SC2034
# features.sh — interactive, checkbox-style build feature selection.

# Offline remains the safe default. MCP depends on the network stack, while
# network-only builds contain no JSON-RPC/MCP application code.
NR_ENABLE_NETWORK="${NR_ENABLE_NETWORK:-0}"
NR_ENABLE_MCP="${NR_ENABLE_MCP:-0}"

nr_stack_label() {
    if (( NR_ENABLE_MCP )); then
        printf '%s' "network + MCP"
    elif (( NR_ENABLE_NETWORK )); then
        printf '%s' "network only"
    else
        printf '%s' "offline"
    fi
}

nr_select_features() {
    while :; do
        nr_section "OPTIONAL STACKS"
        nr_note "Toggle with 1/2, then choose C. Offline is the default."
        nr_checkbox "$NR_ENABLE_NETWORK" "1" "Network stack" \
            "Ethernet, ARP, IPv4, ICMP and UDP through UEFI firmware."
        if (( NR_ENABLE_NETWORK )); then
            nr_checkbox "$NR_ENABLE_MCP" "2" "MCP bridge" \
                "Authenticated inbound/outbound MCP through the trusted host gateway."
        else
            nr_checkbox 0 "2" "MCP bridge" \
                "Requires networking; enabling MCP turns networking on."
        fi
        nr_item "C" "Continue" "Build mode: $(nr_stack_label)"
        nr_item "B" "Back"
        nr_item "Q" "Quit"
        nr_ask "Toggle or continue:"
        case "$REPLY" in
            1)
                if (( NR_ENABLE_NETWORK )); then
                    NR_ENABLE_NETWORK=0
                    NR_ENABLE_MCP=0
                else
                    NR_ENABLE_NETWORK=1
                fi
                ;;
            2)
                if (( NR_ENABLE_MCP )); then
                    NR_ENABLE_MCP=0
                else
                    NR_ENABLE_NETWORK=1
                    NR_ENABLE_MCP=1
                fi
                ;;
            [cC]) return 0 ;;
            [bB]) return 1 ;;
            [qQ]) nr_note "Nothing was changed."; exit "$EXIT_USER_ABORT" ;;
            *) nr_warn "Choose 1, 2, C, B, or Q." ;;
        esac
    done
}
