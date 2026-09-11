//! UEFI Simple Network Protocol adapter for `nr-net` and `nr-mcp`.
//!
//! Firmware calls stay on the boot processor. MCP JSON-RPC is carried in
//! bounded, checksummed UDP fragments to a trusted host gateway, which owns
//! standard HTTP/TLS transport.

#[cfg(feature = "mcp")]
use alloc::collections::VecDeque;
#[cfg(feature = "mcp")]
use alloc::vec::Vec;

#[cfg(feature = "mcp")]
use nr_mcp::{encode_fragment, Kind, Reassembler, MAX_DATAGRAM};
#[cfg(feature = "mcp")]
use nr_net::SendOutcome;
use nr_net::{Config, Ipv4Address, MacAddress, ReceiveEvent, Stack};
use uefi::boot::{self, ScopedProtocol};
use uefi::proto::network::snp::{NetworkState, ReceiveFlags, SimpleNetwork};
use uefi::Status;

const FRAME_BYTES: usize = 1536;
#[cfg(feature = "mcp")]
const MAX_MCP_MESSAGE: usize = 32 * 1024;
const MCP_PORT: u16 = 9000;

/// QEMU user-network defaults. Real hardware needs these values adapted to the
/// trusted gateway LAN until DHCP/config-file support lands.
const ADDRESS: Ipv4Address = Ipv4Address([10, 0, 2, 15]);
const SUBNET: Ipv4Address = Ipv4Address([255, 255, 255, 0]);
const GATEWAY: Ipv4Address = Ipv4Address([10, 0, 2, 2]);

#[cfg(feature = "mcp")]
pub struct McpMessage {
    pub kind: Kind,
    pub message_id: u32,
    pub json: Vec<u8>,
}

#[cfg(feature = "mcp")]
struct PendingMessage {
    kind: Kind,
    message_id: u32,
    json: Vec<u8>,
    fragment: u16,
}

pub struct Network {
    nic: ScopedProtocol<SimpleNetwork>,
    stack: Stack<8>,
    #[cfg(feature = "mcp")]
    reassembler: Reassembler<MAX_MCP_MESSAGE>,
    #[cfg(feature = "mcp")]
    pending: VecDeque<PendingMessage>,
    #[cfg(feature = "mcp")]
    inbox: Option<McpMessage>,
    rx: [u8; FRAME_BYTES],
    tx: [u8; FRAME_BYTES],
    #[cfg(feature = "mcp")]
    mcp_datagram: [u8; MAX_DATAGRAM],
    tx_busy: bool,
    #[cfg(feature = "mcp")]
    next_message_id: u32,
}

impl Network {
    /// Start the first firmware NIC. Networking is optional: missing or
    /// unsupported firmware protocols leave the appliance running offline.
    pub fn init() -> Option<Self> {
        let handle = boot::get_handle_for_protocol::<SimpleNetwork>().ok()?;
        let nic = boot::open_protocol_exclusive::<SimpleNetwork>(handle).ok()?;
        match nic.mode().state {
            NetworkState::STOPPED => {
                nic.start().ok()?;
                nic.initialize(0, 0).ok()?;
            }
            NetworkState::STARTED => nic.initialize(0, 0).ok()?,
            NetworkState::INITIALIZED => {}
            _ => return None,
        }
        if nic.mode().hw_address_size != 6 || nic.mode().media_header_size != 14 {
            return None;
        }
        nic.receive_filters(
            ReceiveFlags::UNICAST | ReceiveFlags::BROADCAST,
            ReceiveFlags::empty(),
            false,
            None,
        )
        .ok()?;
        let raw = nic.mode().current_address.0;
        let mac = MacAddress([raw[0], raw[1], raw[2], raw[3], raw[4], raw[5]]);
        serial_println!("[net] NIC up: mac={} ip={} (static)", mac, ADDRESS);
        let network = Self {
            nic,
            stack: Stack::new(Config {
                mac,
                address: ADDRESS,
                subnet_mask: SUBNET,
                gateway: GATEWAY,
            }),
            #[cfg(feature = "mcp")]
            reassembler: Reassembler::default(),
            #[cfg(feature = "mcp")]
            pending: VecDeque::new(),
            #[cfg(feature = "mcp")]
            inbox: None,
            rx: [0; FRAME_BYTES],
            tx: [0; FRAME_BYTES],
            #[cfg(feature = "mcp")]
            mcp_datagram: [0; MAX_DATAGRAM],
            tx_busy: false,
            #[cfg(feature = "mcp")]
            next_message_id: 1,
        };
        #[cfg(feature = "mcp")]
        let network = {
            let mut network = network;
            network.queue(Kind::Hello, 0, b"{}".to_vec());
            network
        };
        Some(network)
    }

    #[cfg(feature = "mcp")]
    pub fn queue_response(&mut self, message_id: u32, json: Vec<u8>) {
        self.queue(Kind::DeviceResponse, message_id, json);
    }

    #[cfg(feature = "mcp")]
    pub fn queue_upstream_request(&mut self, json: Vec<u8>) -> u32 {
        let id = self.next_message_id;
        self.next_message_id = self.next_message_id.wrapping_add(1).max(1);
        self.queue(Kind::DeviceRequest, id, json);
        id
    }

    #[cfg(feature = "mcp")]
    fn queue(&mut self, kind: Kind, message_id: u32, json: Vec<u8>) {
        if json.len() <= MAX_MCP_MESSAGE && !json.is_empty() {
            self.pending.push_back(PendingMessage {
                kind,
                message_id,
                json,
                fragment: 0,
            });
        } else {
            serial_println!("[mcp] refused message of {} bytes", json.len());
        }
    }

    /// Poll without blocking. QEMU's SNP packet event is unreliable, so the
    /// chat loop calls this directly. In network-only builds the stack still
    /// services ARP and ICMP, but does not parse or emit MCP datagrams.
    fn poll_stack(&mut self) {
        self.reclaim_transmit();
        #[cfg(feature = "mcp")]
        self.send_pending();

        loop {
            let len = match self.nic.receive(&mut self.rx, None, None, None, None) {
                Ok(len) => len,
                Err(error) if error.status() == Status::NOT_READY => break,
                Err(error) => {
                    serial_println!("[net] receive error: {:?}", error.status());
                    break;
                }
            };
            match self.stack.receive(&self.rx[..len], &mut self.tx) {
                Ok(ReceiveEvent::Reply(len)) if !self.tx_busy => self.transmit(len),
                Ok(ReceiveEvent::Udp(packet)) if packet.destination_port == MCP_PORT => {
                    #[cfg(feature = "mcp")]
                    match self.reassembler.push(packet.payload) {
                        Ok(Some(message)) => {
                            let complete = McpMessage {
                                kind: message.kind,
                                message_id: message.message_id,
                                json: message.payload.to_vec(),
                            };
                            serial_println!(
                                "[mcp] received {:?} #{} ({} bytes)",
                                complete.kind,
                                complete.message_id,
                                complete.json.len()
                            );
                            self.inbox = Some(complete);
                            break;
                        }
                        Ok(None) => {}
                        Err(error) => serial_println!("[mcp] fragment error: {:?}", error),
                    }
                }
                Ok(_) => {}
                Err(error) => serial_println!("[net] dropped frame: {:?}", error),
            }
        }
    }

    #[cfg(not(feature = "mcp"))]
    pub fn poll(&mut self) {
        self.poll_stack();
    }

    #[cfg(feature = "mcp")]
    pub fn poll_mcp(&mut self) -> Option<McpMessage> {
        self.poll_stack();
        self.inbox.take()
    }

    fn reclaim_transmit(&mut self) {
        if self.tx_busy && matches!(self.nic.get_recycled_transmit_buffer_status(), Ok(Some(_))) {
            self.tx_busy = false;
        }
    }

    #[cfg(feature = "mcp")]
    fn send_pending(&mut self) {
        if self.tx_busy {
            return;
        }
        let Some(message) = self.pending.front() else {
            return;
        };
        let count = nr_mcp::fragment_count(message.json.len()).unwrap_or(1);
        let datagram_len = match encode_fragment(
            message.kind,
            message.message_id,
            &message.json,
            message.fragment,
            &mut self.mcp_datagram,
        ) {
            Ok(len) => len,
            Err(error) => {
                serial_println!("[mcp] encode error: {:?}", error);
                self.pending.pop_front();
                return;
            }
        };
        match self.stack.send_udp(
            GATEWAY,
            MCP_PORT,
            MCP_PORT,
            &self.mcp_datagram[..datagram_len],
            &mut self.tx,
        ) {
            Ok(SendOutcome::NeighborDiscovery(len)) => self.transmit(len),
            Ok(SendOutcome::Datagram(len)) => {
                self.transmit(len);
                let message = self.pending.front_mut().unwrap();
                message.fragment += 1;
                if message.fragment == count {
                    self.pending.pop_front();
                }
            }
            Err(error) => {
                serial_println!("[mcp] send error: {:?}", error);
                self.pending.pop_front();
            }
        }
    }

    fn transmit(&mut self, len: usize) {
        if self
            .nic
            .transmit(0, &self.tx[..len], None, None, None)
            .is_ok()
        {
            self.tx_busy = true;
        }
    }
}
