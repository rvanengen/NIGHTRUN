//! Allocation-free Ethernet, ARP, IPv4, ICMP, and UDP for NightRun.
//!
//! The stack deliberately owns no device driver. A platform adapter gives
//! [`Stack::receive`] frames obtained from a NIC and transmits buffers produced
//! by [`Stack::send_udp`] or a [`ReceiveEvent::Reply`]. This keeps firmware
//! calls out of the protocol layer and makes the wire code host-testable.

#![cfg_attr(not(feature = "std"), no_std)]

use core::fmt;

const ETH_HEADER: usize = 14;
const IPV4_HEADER: usize = 20;
const UDP_HEADER: usize = 8;
const ARP_PACKET: usize = 28;
const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
const IP_PROTOCOL_ICMP: u8 = 1;
const IP_PROTOCOL_UDP: u8 = 17;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MacAddress(pub [u8; 6]);

impl MacAddress {
    pub const BROADCAST: Self = Self([0xff; 6]);
    pub const ZERO: Self = Self([0; 6]);
}

impl fmt::Display for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Ipv4Address(pub [u8; 4]);

impl Ipv4Address {
    pub const BROADCAST: Self = Self([255; 4]);
    pub const UNSPECIFIED: Self = Self([0; 4]);
    const fn as_u32(self) -> u32 {
        u32::from_be_bytes(self.0)
    }
}

impl fmt::Display for Ipv4Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}.{}", self.0[0], self.0[1], self.0[2], self.0[3])
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    pub mac: MacAddress,
    pub address: Ipv4Address,
    pub subnet_mask: Ipv4Address,
    pub gateway: Ipv4Address,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    BufferTooSmall,
    Truncated,
    Unsupported,
    InvalidHeader,
    InvalidChecksum,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SendOutcome {
    /// A complete UDP/IPv4/Ethernet frame is in the output buffer.
    Datagram(usize),
    /// An ARP request is in the output buffer. Transmit it, then retry the UDP
    /// send after its reply has passed through `receive`.
    NeighborDiscovery(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UdpDatagram<'a> {
    pub source: Ipv4Address,
    pub destination: Ipv4Address,
    pub source_port: u16,
    pub destination_port: u16,
    pub payload: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiveEvent<'a> {
    None,
    /// The output buffer contains a reply that should be transmitted.
    Reply(usize),
    NeighborLearned(Ipv4Address, MacAddress),
    Udp(UdpDatagram<'a>),
}

#[derive(Clone, Copy, Debug, Default)]
struct Neighbor {
    ip: Ipv4Address,
    mac: MacAddress,
    valid: bool,
}

/// A small, deterministic IPv4 stack. `N` is the fixed ARP-cache capacity.
pub struct Stack<const N: usize = 8> {
    config: Config,
    neighbors: [Neighbor; N],
    next_neighbor: usize,
    identification: u16,
}

impl<const N: usize> Stack<N> {
    pub const fn new(config: Config) -> Self {
        Self {
            config,
            neighbors: [Neighbor {
                ip: Ipv4Address::UNSPECIFIED,
                mac: MacAddress::ZERO,
                valid: false,
            }; N],
            next_neighbor: 0,
            identification: 1,
        }
    }

    pub const fn config(&self) -> Config {
        self.config
    }

    pub fn set_config(&mut self, config: Config) {
        self.config = config;
        self.neighbors.fill(Neighbor::default());
        self.next_neighbor = 0;
    }

    pub fn neighbor(&self, ip: Ipv4Address) -> Option<MacAddress> {
        self.neighbors
            .iter()
            .find(|entry| entry.valid && entry.ip == ip)
            .map(|entry| entry.mac)
    }

    /// Consume one Ethernet frame. ARP and ICMP responses are written into
    /// `response`; UDP payloads borrow directly from `frame`.
    pub fn receive<'a>(
        &mut self,
        frame: &'a [u8],
        response: &mut [u8],
    ) -> Result<ReceiveEvent<'a>, Error> {
        if frame.len() < ETH_HEADER {
            return Err(Error::Truncated);
        }
        let destination_mac = mac(&frame[0..6]);
        if destination_mac != self.config.mac && destination_mac != MacAddress::BROADCAST {
            return Ok(ReceiveEvent::None);
        }
        let source_mac = mac(&frame[6..12]);
        match be16(&frame[12..14]) {
            ETHERTYPE_ARP => self.receive_arp(source_mac, &frame[ETH_HEADER..], response),
            ETHERTYPE_IPV4 => self.receive_ipv4(source_mac, frame, response),
            _ => Ok(ReceiveEvent::None),
        }
    }

    pub fn send_udp(
        &mut self,
        destination: Ipv4Address,
        source_port: u16,
        destination_port: u16,
        payload: &[u8],
        output: &mut [u8],
    ) -> Result<SendOutcome, Error> {
        let total = ETH_HEADER + IPV4_HEADER + UDP_HEADER + payload.len();
        if payload.len() > (u16::MAX as usize - IPV4_HEADER - UDP_HEADER) {
            return Err(Error::Unsupported);
        }
        let destination_mac = if destination == Ipv4Address::BROADCAST {
            MacAddress::BROADCAST
        } else {
            let next_hop = if same_subnet(self.config.address, destination, self.config.subnet_mask)
            {
                destination
            } else {
                self.config.gateway
            };
            match self.neighbor(next_hop) {
                Some(mac) => mac,
                None => {
                    let len = write_arp_request(self.config, next_hop, output)?;
                    return Ok(SendOutcome::NeighborDiscovery(len));
                }
            }
        };
        if output.len() < total {
            return Err(Error::BufferTooSmall);
        }
        write_ethernet(output, destination_mac, self.config.mac, ETHERTYPE_IPV4);
        self.identification = self.identification.wrapping_add(1);
        write_ipv4_header(
            &mut output[ETH_HEADER..ETH_HEADER + IPV4_HEADER],
            UDP_HEADER + payload.len(),
            self.identification,
            IP_PROTOCOL_UDP,
            self.config.address,
            destination,
        );
        let udp = &mut output[ETH_HEADER + IPV4_HEADER..total];
        put16(&mut udp[0..2], source_port);
        put16(&mut udp[2..4], destination_port);
        put16(&mut udp[4..6], (UDP_HEADER + payload.len()) as u16);
        udp[6..8].fill(0);
        udp[UDP_HEADER..].copy_from_slice(payload);
        let sum = transport_checksum(self.config.address, destination, IP_PROTOCOL_UDP, udp);
        put16(&mut udp[6..8], if sum == 0 { 0xffff } else { sum });
        Ok(SendOutcome::Datagram(total))
    }

    fn receive_arp<'a>(
        &mut self,
        ethernet_source: MacAddress,
        packet: &[u8],
        response: &mut [u8],
    ) -> Result<ReceiveEvent<'a>, Error> {
        if packet.len() < ARP_PACKET {
            return Err(Error::Truncated);
        }
        if be16(&packet[0..2]) != 1
            || be16(&packet[2..4]) != ETHERTYPE_IPV4
            || packet[4] != 6
            || packet[5] != 4
        {
            return Err(Error::Unsupported);
        }
        let operation = be16(&packet[6..8]);
        let sender_mac = mac(&packet[8..14]);
        let sender_ip = ip(&packet[14..18]);
        let target_ip = ip(&packet[24..28]);
        if sender_mac != ethernet_source {
            return Err(Error::InvalidHeader);
        }
        self.learn(sender_ip, sender_mac);
        if operation == 1 && target_ip == self.config.address {
            let len = write_arp_reply(self.config, sender_ip, sender_mac, response)?;
            Ok(ReceiveEvent::Reply(len))
        } else if operation == 2 {
            Ok(ReceiveEvent::NeighborLearned(sender_ip, sender_mac))
        } else {
            Ok(ReceiveEvent::None)
        }
    }

    fn receive_ipv4<'a>(
        &mut self,
        source_mac: MacAddress,
        frame: &'a [u8],
        response: &mut [u8],
    ) -> Result<ReceiveEvent<'a>, Error> {
        let packet = &frame[ETH_HEADER..];
        if packet.len() < IPV4_HEADER || packet[0] >> 4 != 4 {
            return Err(Error::InvalidHeader);
        }
        let header_len = ((packet[0] & 0x0f) as usize) * 4;
        if header_len < IPV4_HEADER || packet.len() < header_len {
            return Err(Error::InvalidHeader);
        }
        let total_len = be16(&packet[2..4]) as usize;
        if total_len < header_len || packet.len() < total_len {
            return Err(Error::Truncated);
        }
        if checksum(&packet[..header_len]) != 0 {
            return Err(Error::InvalidChecksum);
        }
        if be16(&packet[6..8]) & 0x3fff != 0 {
            return Err(Error::Unsupported);
        }
        let source = ip(&packet[12..16]);
        let destination = ip(&packet[16..20]);
        if destination != self.config.address && destination != Ipv4Address::BROADCAST {
            return Ok(ReceiveEvent::None);
        }
        self.learn(source, source_mac);
        let payload = &packet[header_len..total_len];
        match packet[9] {
            IP_PROTOCOL_ICMP => self.receive_icmp(frame, header_len, total_len, response),
            IP_PROTOCOL_UDP => parse_udp(source, destination, payload).map(ReceiveEvent::Udp),
            _ => Ok(ReceiveEvent::None),
        }
    }

    fn receive_icmp<'a>(
        &mut self,
        frame: &[u8],
        header_len: usize,
        total_len: usize,
        response: &mut [u8],
    ) -> Result<ReceiveEvent<'a>, Error> {
        let end = ETH_HEADER + total_len;
        let icmp_start = ETH_HEADER + header_len;
        if frame.len() < end || total_len < header_len + 8 {
            return Err(Error::Truncated);
        }
        let icmp = &frame[icmp_start..end];
        if checksum(icmp) != 0 {
            return Err(Error::InvalidChecksum);
        }
        if icmp[0] != 8 || icmp[1] != 0 {
            return Ok(ReceiveEvent::None);
        }
        if response.len() < end {
            return Err(Error::BufferTooSmall);
        }
        response[..end].copy_from_slice(&frame[..end]);
        let source_mac = mac(&frame[6..12]);
        write_ethernet(response, source_mac, self.config.mac, ETHERTYPE_IPV4);
        let ip_header = &mut response[ETH_HEADER..icmp_start];
        ip_header[12..16].copy_from_slice(&self.config.address.0);
        ip_header[16..20].copy_from_slice(&frame[ETH_HEADER + 12..ETH_HEADER + 16]);
        ip_header[10..12].fill(0);
        let ip_checksum = checksum(ip_header);
        put16(&mut ip_header[10..12], ip_checksum);
        let reply = &mut response[icmp_start..end];
        reply[0] = 0;
        reply[2..4].fill(0);
        let icmp_checksum = checksum(reply);
        put16(&mut reply[2..4], icmp_checksum);
        Ok(ReceiveEvent::Reply(end))
    }

    fn learn(&mut self, ip: Ipv4Address, mac: MacAddress) {
        if ip == Ipv4Address::UNSPECIFIED || mac == MacAddress::ZERO || mac == MacAddress::BROADCAST
        {
            return;
        }
        if let Some(entry) = self
            .neighbors
            .iter_mut()
            .find(|entry| entry.valid && entry.ip == ip)
        {
            entry.mac = mac;
            return;
        }
        if N != 0 {
            self.neighbors[self.next_neighbor] = Neighbor {
                ip,
                mac,
                valid: true,
            };
            self.next_neighbor = (self.next_neighbor + 1) % N;
        }
    }
}

fn parse_udp(
    source: Ipv4Address,
    destination: Ipv4Address,
    packet: &[u8],
) -> Result<UdpDatagram<'_>, Error> {
    if packet.len() < UDP_HEADER {
        return Err(Error::Truncated);
    }
    let len = be16(&packet[4..6]) as usize;
    if len < UDP_HEADER || len > packet.len() {
        return Err(Error::Truncated);
    }
    let packet = &packet[..len];
    let wire_checksum = be16(&packet[6..8]);
    if wire_checksum != 0 && transport_checksum(source, destination, IP_PROTOCOL_UDP, packet) != 0 {
        return Err(Error::InvalidChecksum);
    }
    Ok(UdpDatagram {
        source,
        destination,
        source_port: be16(&packet[0..2]),
        destination_port: be16(&packet[2..4]),
        payload: &packet[UDP_HEADER..],
    })
}

fn write_arp_request(
    config: Config,
    target: Ipv4Address,
    output: &mut [u8],
) -> Result<usize, Error> {
    write_arp(
        config,
        MacAddress::BROADCAST,
        target,
        MacAddress::ZERO,
        1,
        output,
    )
}

fn write_arp_reply(
    config: Config,
    target_ip: Ipv4Address,
    target_mac: MacAddress,
    output: &mut [u8],
) -> Result<usize, Error> {
    write_arp(config, target_mac, target_ip, target_mac, 2, output)
}

fn write_arp(
    config: Config,
    ethernet_dst: MacAddress,
    target_ip: Ipv4Address,
    target_mac: MacAddress,
    operation: u16,
    output: &mut [u8],
) -> Result<usize, Error> {
    let len = ETH_HEADER + ARP_PACKET;
    if output.len() < len {
        return Err(Error::BufferTooSmall);
    }
    write_ethernet(output, ethernet_dst, config.mac, ETHERTYPE_ARP);
    let arp = &mut output[ETH_HEADER..len];
    put16(&mut arp[0..2], 1);
    put16(&mut arp[2..4], ETHERTYPE_IPV4);
    arp[4] = 6;
    arp[5] = 4;
    put16(&mut arp[6..8], operation);
    arp[8..14].copy_from_slice(&config.mac.0);
    arp[14..18].copy_from_slice(&config.address.0);
    arp[18..24].copy_from_slice(&target_mac.0);
    arp[24..28].copy_from_slice(&target_ip.0);
    Ok(len)
}

fn write_ethernet(output: &mut [u8], destination: MacAddress, source: MacAddress, ethertype: u16) {
    output[0..6].copy_from_slice(&destination.0);
    output[6..12].copy_from_slice(&source.0);
    put16(&mut output[12..14], ethertype);
}

fn write_ipv4_header(
    header: &mut [u8],
    payload_len: usize,
    identification: u16,
    protocol: u8,
    source: Ipv4Address,
    destination: Ipv4Address,
) {
    header.fill(0);
    header[0] = 0x45;
    put16(&mut header[2..4], (IPV4_HEADER + payload_len) as u16);
    put16(&mut header[4..6], identification);
    put16(&mut header[6..8], 0x4000);
    header[8] = 64;
    header[9] = protocol;
    header[12..16].copy_from_slice(&source.0);
    header[16..20].copy_from_slice(&destination.0);
    let header_checksum = checksum(header);
    put16(&mut header[10..12], header_checksum);
}

fn same_subnet(a: Ipv4Address, b: Ipv4Address, mask: Ipv4Address) -> bool {
    a.as_u32() & mask.as_u32() == b.as_u32() & mask.as_u32()
}
fn mac(bytes: &[u8]) -> MacAddress {
    let mut value = [0; 6];
    value.copy_from_slice(&bytes[..6]);
    MacAddress(value)
}
fn ip(bytes: &[u8]) -> Ipv4Address {
    let mut value = [0; 4];
    value.copy_from_slice(&bytes[..4]);
    Ipv4Address(value)
}
fn be16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}
fn put16(bytes: &mut [u8], value: u16) {
    bytes[..2].copy_from_slice(&value.to_be_bytes());
}

/// Internet checksum (RFC 1071). A valid packet including its checksum yields 0.
pub fn checksum(bytes: &[u8]) -> u16 {
    finalize_sum(partial_sum(0, bytes))
}

fn partial_sum(mut sum: u32, bytes: &[u8]) -> u32 {
    let mut chunks = bytes.chunks_exact(2);
    for chunk in &mut chunks {
        sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
    }
    if let Some(&last) = chunks.remainder().first() {
        sum += (last as u32) << 8;
    }
    sum
}

fn finalize_sum(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn transport_checksum(
    source: Ipv4Address,
    destination: Ipv4Address,
    protocol: u8,
    packet: &[u8],
) -> u16 {
    let mut sum = partial_sum(0, &source.0);
    sum = partial_sum(sum, &destination.0);
    sum += protocol as u32;
    sum += packet.len() as u32;
    finalize_sum(partial_sum(sum, packet))
}

#[cfg(test)]
mod tests {
    use super::*;
    const LOCAL_MAC: MacAddress = MacAddress([0x02, 0, 0, 0, 0, 2]);
    const PEER_MAC: MacAddress = MacAddress([0x02, 0, 0, 0, 0, 9]);
    const LOCAL_IP: Ipv4Address = Ipv4Address([10, 0, 2, 15]);
    const PEER_IP: Ipv4Address = Ipv4Address([10, 0, 2, 2]);

    fn config() -> Config {
        Config {
            mac: LOCAL_MAC,
            address: LOCAL_IP,
            subnet_mask: Ipv4Address([255, 255, 255, 0]),
            gateway: PEER_IP,
        }
    }

    #[test]
    fn arp_request_is_answered_and_neighbor_is_learned() {
        let peer = Stack::<4>::new(Config {
            mac: PEER_MAC,
            address: PEER_IP,
            ..config()
        });
        let mut request = [0u8; 64];
        let len = write_arp_request(peer.config(), LOCAL_IP, &mut request).unwrap();
        let mut reply = [0u8; 64];
        let mut local = Stack::<4>::new(config());
        assert_eq!(
            local.receive(&request[..len], &mut reply),
            Ok(ReceiveEvent::Reply(42))
        );
        assert_eq!(local.neighbor(PEER_IP), Some(PEER_MAC));
        assert_eq!(be16(&reply[20..22]), 2);
        assert_eq!(&reply[0..6], &PEER_MAC.0);
    }

    #[test]
    fn udp_send_resolves_arp_then_round_trips() {
        let mut local = Stack::<4>::new(config());
        let mut frame = [0u8; 128];
        assert_eq!(
            local.send_udp(PEER_IP, 49152, 9000, b"night", &mut frame),
            Ok(SendOutcome::NeighborDiscovery(42))
        );
        let mut peer = Stack::<4>::new(Config {
            mac: PEER_MAC,
            address: PEER_IP,
            ..config()
        });
        let mut arp_reply = [0u8; 64];
        let arp_len = match peer.receive(&frame[..42], &mut arp_reply).unwrap() {
            ReceiveEvent::Reply(len) => len,
            other => panic!("unexpected {other:?}"),
        };
        local.receive(&arp_reply[..arp_len], &mut frame).unwrap();
        let len = match local
            .send_udp(PEER_IP, 49152, 9000, b"night", &mut frame)
            .unwrap()
        {
            SendOutcome::Datagram(len) => len,
            other => panic!("unexpected {other:?}"),
        };
        let mut response = [0u8; 128];
        match peer.receive(&frame[..len], &mut response).unwrap() {
            ReceiveEvent::Udp(datagram) => {
                assert_eq!(datagram.source, LOCAL_IP);
                assert_eq!(datagram.destination_port, 9000);
                assert_eq!(datagram.payload, b"night");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn corrupted_ipv4_header_is_rejected() {
        let mut local = Stack::<4>::new(config());
        let mut frame = [0u8; 128];
        let mut peer = Stack::<4>::new(Config {
            mac: PEER_MAC,
            address: PEER_IP,
            ..config()
        });
        peer.learn(LOCAL_IP, LOCAL_MAC);
        let len = match peer.send_udp(LOCAL_IP, 1, 2, b"x", &mut frame).unwrap() {
            SendOutcome::Datagram(len) => len,
            _ => unreachable!(),
        };
        frame[ETH_HEADER + 8] ^= 1;
        let mut response = [0u8; 64];
        assert_eq!(
            local.receive(&frame[..len], &mut response),
            Err(Error::InvalidChecksum)
        );
    }

    #[test]
    fn icmp_echo_request_gets_a_valid_reply() {
        let mut frame = [0u8; 128];
        let icmp_len = 12;
        let frame_len = ETH_HEADER + IPV4_HEADER + icmp_len;
        write_ethernet(&mut frame, LOCAL_MAC, PEER_MAC, ETHERTYPE_IPV4);
        write_ipv4_header(
            &mut frame[ETH_HEADER..ETH_HEADER + IPV4_HEADER],
            icmp_len,
            7,
            IP_PROTOCOL_ICMP,
            PEER_IP,
            LOCAL_IP,
        );
        let icmp = &mut frame[ETH_HEADER + IPV4_HEADER..frame_len];
        icmp.copy_from_slice(&[8, 0, 0, 0, 0x12, 0x34, 0, 1, b'p', b'i', b'n', b'g']);
        let sum = checksum(icmp);
        put16(&mut icmp[2..4], sum);

        let mut stack = Stack::<4>::new(config());
        let mut reply = [0u8; 128];
        assert_eq!(
            stack.receive(&frame[..frame_len], &mut reply),
            Ok(ReceiveEvent::Reply(frame_len))
        );
        assert_eq!(&reply[0..6], &PEER_MAC.0);
        assert_eq!(&reply[6..12], &LOCAL_MAC.0);
        assert_eq!(reply[ETH_HEADER + IPV4_HEADER], 0);
        assert_eq!(checksum(&reply[ETH_HEADER..ETH_HEADER + IPV4_HEADER]), 0);
        assert_eq!(checksum(&reply[ETH_HEADER + IPV4_HEADER..frame_len]), 0);
    }

    #[test]
    fn checksum_matches_rfc_1071_example() {
        assert_eq!(
            checksum(&[0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7]),
            0x220d
        );
    }
}
