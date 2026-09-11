# MCP bridge

NightRun can operate as both an MCP server and an MCP client when built with
`--mcp`. The bare-metal application does not implement TCP or TLS. Instead,
it exchanges bounded JSON-RPC messages with `nrmcp-gateway` over the `nr-net`
UDP stack. The gateway exposes and consumes standard MCP Streamable HTTP.

```text
MCP client -- HTTP + bearer token --> nrmcp-gateway -- NRMP/UDP --> NightRun
MCP server <-- HTTP + optional token -- nrmcp-gateway <-- NRMP/UDP -- NightRun
```

The NRMP link fragments messages below the Ethernet MTU, supports out-of-order
reassembly, and verifies the complete message with CRC-32. Device messages are
limited to 32 KiB; the gateway refuses HTTP messages over 64 KiB.

## Start the gateway

Create a long random bearer token and keep it out of shell history where
possible:

```sh
export NRMCP_TOKEN="replace-with-at-least-24-random-characters"
cargo run -p nrmcp-gateway
```

The defaults are:

- MCP endpoint: `http://127.0.0.1:9001/mcp`
- UDP endpoint: `127.0.0.1:9000`
- NightRun gateway address: `10.0.2.2:9000` (QEMU's host alias)

The HTTP endpoint binds to loopback, validates browser `Origin`, and requires
`Authorization: Bearer $NRMCP_TOKEN` on every request.

To give NightRun outbound access to an MCP server:

```sh
export NRMCP_UPSTREAM_TOKEN="remote-server-token-if-needed"
cargo run -p nrmcp-gateway -- \
  --upstream https://example.test/mcp \
  --upstream-token-env NRMCP_UPSTREAM_TOKEN
```

The gateway performs MCP initialization and session propagation before
forwarding NightRun's first request. Both JSON and SSE responses are accepted.

## Boot NightRun with MCP

```sh
cargo xtask run --mcp --img --mem 4G --window
```

`--mcp` adds the firmware NIC and builds `nr-boot` with the MCP feature, which
depends on the lower-level network feature. `--network` alone deliberately
omits MCP/JSON-RPC code. Normal builds remain offline.

## Inbound MCP tools

Point an MCP client at `http://127.0.0.1:9001/mcp` and configure the bearer
token. NightRun advertises:

- `nightrun_status`: model, memory, CPU-core, and context information
- `nightrun_prompt`: run a prompt through the resident model and return text

Inbound prompts are displayed in NightRun's visible conversation. The gateway
times out after ten minutes so slower Raspberry Pi generation can finish.

## Outbound MCP commands

From the NightRun prompt:

```text
/mcp-list
/mcp-call TOOL_NAME {"argument":"value"}
```

Responses appear as system turns. This is an explicit user-controlled client;
the language model does not autonomously invoke remote tools yet.

## Security boundary

The HTTP side is authenticated, but the UDP link is not encrypted or
cryptographically authenticated. Use it only across QEMU's private user network
or a physically trusted LAN. Do not expose UDP port 9000 to an untrusted
network. MCP tool metadata and results are untrusted input, and sensitive remote
tool calls should remain user-confirmed.

Real hardware currently requires editing the static address constants in
`crates/nr-boot/src/network.rs`. DHCP and a boot-time configuration file are
future work.
