# NightRun network stack

`nr-net` is an allocation-free, `no_std` packet stack intended for NightRun's
UEFI environment. It currently implements:

- Ethernet II framing
- ARP requests, replies, and a fixed-size neighbor cache
- IPv4 validation and routing through a configured gateway
- ICMP echo replies
- UDP transmit and receive with pseudo-header checksums

MCP message framing is a separate optional layer in `nr-mcp`; it is not
included in a network-only boot binary.

It deliberately separates packet processing from the device driver. A platform
adapter receives a frame from UEFI `EFI_SIMPLE_NETWORK_PROTOCOL`, passes it to
`Stack::receive`, and transmits any `ReceiveEvent::Reply`. Outbound users call
`Stack::send_udp`; `NeighborDiscovery` means the returned ARP frame must be sent
and the UDP call retried after an ARP reply arrives.

```rust
use nr_net::{Config, Ipv4Address, MacAddress, ReceiveEvent, SendOutcome, Stack};

let mut stack = Stack::<8>::new(Config {
    mac: MacAddress([0x02, 0, 0, 0, 0, 2]),
    address: Ipv4Address([10, 0, 2, 15]),
    subnet_mask: Ipv4Address([255, 255, 255, 0]),
    gateway: Ipv4Address([10, 0, 2, 2]),
});

let mut tx = [0u8; 1536];
match stack.send_udp(Ipv4Address([10, 0, 2, 2]), 49152, 9000, b"hello", &mut tx)? {
    SendOutcome::Datagram(len) | SendOutcome::NeighborDiscovery(len) => nic.transmit(&tx[..len]),
}

let mut reply = [0u8; 1536];
match stack.receive(nic.receive(), &mut reply)? {
    ReceiveEvent::Reply(len) => nic.transmit(&reply[..len]),
    ReceiveEvent::Udp(packet) => consume(packet.payload),
    _ => {}
}
```

## Boundaries

This first layer uses static IPv4 configuration. DHCP, DNS, TCP, and TLS are
not included yet. The UEFI adapter is opt-in, so the existing offline appliance
behavior remains unchanged by default. Build the boot application with the
`network` feature to enable only the first firmware NIC and packet stack, or
with `mcp` (which implies `network`) to add the MCP application layer. The initial adapter uses
QEMU user-network defaults (`10.0.2.15/24`, gateway `10.0.2.2`), answers ARP and
ICMP echo requests and accepts validated UDP datagrams:

```sh
cargo xtask run --network --img --mem 4G --window
```

For the MCP gateway use `--mcp`; see [mcp.md](mcp.md).

Run the wire-format tests with:

```sh
cargo test -p nr-net
cargo test -p nr-net --no-default-features
cargo test -p nr-mcp --no-default-features
cargo test -p nrmcp-gateway
```
