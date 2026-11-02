//! Frames the stack answers by hand: UDP (which smoltcp only sees as a socket API), DHCP, ICMP.
use smoltcp::{
    phy::ChecksumCapabilities,
    wire::{
        EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, IpProtocol, Ipv4Address,
        Ipv4Packet, Ipv4Repr, UdpPacket, UdpRepr,
    },
};

pub const GUEST_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
pub const GATEWAY_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);
pub const DNS_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 3);
pub const GATEWAY_MAC: EthernetAddress = EthernetAddress([0x52, 0x54, 0x00, 0x12, 0x35, 0x02]);
const LEASE_SECONDS: u32 = 86400;
const DHCP_OPTIONS: usize = 240;
const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;

fn ethernet(to: EthernetAddress, payload_len: usize) -> Vec<u8> {
    let repr = EthernetRepr {
        src_addr: GATEWAY_MAC,
        dst_addr: to,
        ethertype: EthernetProtocol::Ipv4,
    };
    let mut frame = vec![0; repr.buffer_len() + payload_len];
    repr.emit(&mut EthernetFrame::new_unchecked(&mut frame));
    frame
}

/// A UDP datagram to the guest as if `source` sent it.
pub fn udp_frame(
    to: EthernetAddress,
    source: (Ipv4Address, u16),
    destination: (Ipv4Address, u16),
    payload: &[u8],
) -> Vec<u8> {
    let udp = UdpRepr {
        src_port: source.1,
        dst_port: destination.1,
    };
    let ip = Ipv4Repr {
        src_addr: source.0,
        dst_addr: destination.0,
        next_header: IpProtocol::Udp,
        payload_len: udp.header_len() + payload.len(),
        hop_limit: 64,
    };
    let mut frame = ethernet(to, ip.buffer_len() + ip.payload_len);
    let checksums = ChecksumCapabilities::default();
    let mut packet = Ipv4Packet::new_unchecked(&mut frame[14..]);
    ip.emit(&mut packet, &checksums);
    udp.emit(
        &mut UdpPacket::new_unchecked(packet.payload_mut()),
        &source.0.into(),
        &destination.0.into(),
        payload.len(),
        |buffer| buffer.copy_from_slice(payload),
        &checksums,
    );
    frame
}

fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = data
        .chunks(2)
        .map(|c| u32::from(u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)])))
        .sum();
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// ICMP "administratively prohibited" about the guest's IP packet `original`.
pub fn prohibited_frame(to: EthernetAddress, from: Ipv4Address, original: &[u8]) -> Vec<u8> {
    let quoted = &original[..original.len().min(28)];
    let mut icmp = vec![3, 13, 0, 0, 0, 0, 0, 0];
    icmp.extend_from_slice(quoted);
    let sum = checksum(&icmp).to_be_bytes();
    icmp[2..4].copy_from_slice(&sum);
    let ip = Ipv4Repr {
        src_addr: from,
        dst_addr: GUEST_IP,
        next_header: IpProtocol::Icmp,
        payload_len: icmp.len(),
        hop_limit: 64,
    };
    let mut frame = ethernet(to, ip.buffer_len() + icmp.len());
    let mut packet = Ipv4Packet::new_unchecked(&mut frame[14..]);
    ip.emit(&mut packet, &ChecksumCapabilities::default());
    packet.payload_mut().copy_from_slice(&icmp);
    frame
}

/// Answers a DISCOVER or REQUEST with the one lease there is. `None` for anything else.
pub fn dhcp_reply(request: &[u8], to: EthernetAddress) -> Option<Vec<u8>> {
    if request.len() < DHCP_OPTIONS || request[0] != 1 || request[236..240] != [99, 130, 83, 99] {
        return None;
    }
    let mut options = &request[DHCP_OPTIONS..];
    let mut kind = None;
    while let [code, rest @ ..] = options {
        match code {
            0 => options = rest,
            255 => break,
            _ => {
                let (&length, rest) = rest.split_first()?;
                let value = rest.get(..length as usize)?;
                if *code == 53 {
                    kind = value.first().copied();
                }
                options = &rest[length as usize..];
            }
        }
    }
    let answer = match kind? {
        DISCOVER => OFFER,
        REQUEST => ACK,
        _ => return None,
    };
    let mut reply = vec![0; DHCP_OPTIONS];
    reply[0] = 2;
    reply[1] = 1;
    reply[2] = 6;
    reply[4..8].copy_from_slice(&request[4..8]);
    reply[10..12].copy_from_slice(&request[10..12]);
    reply[16..20].copy_from_slice(&GUEST_IP.octets());
    reply[20..24].copy_from_slice(&GATEWAY_IP.octets());
    reply[28..44].copy_from_slice(&request[28..44]);
    reply[236..240].copy_from_slice(&[99, 130, 83, 99]);
    reply.extend_from_slice(&[53, 1, answer, 54, 4]);
    reply.extend_from_slice(&GATEWAY_IP.octets());
    reply.extend_from_slice(&[1, 4, 255, 255, 255, 0, 3, 4]);
    reply.extend_from_slice(&GATEWAY_IP.octets());
    reply.extend_from_slice(&[6, 4]);
    reply.extend_from_slice(&DNS_IP.octets());
    reply.extend_from_slice(&[51, 4]);
    reply.extend_from_slice(&LEASE_SECONDS.to_be_bytes());
    reply.push(255);
    Some(udp_frame(
        to,
        (GATEWAY_IP, 67),
        (Ipv4Address::BROADCAST, 68),
        &reply,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUEST: EthernetAddress = EthernetAddress([2, 0, 0, 0, 0, 1]);

    fn request(kind: u8) -> Vec<u8> {
        let mut bytes = vec![0; 240];
        bytes[0] = 1;
        bytes[4..8].copy_from_slice(&[1, 2, 3, 4]);
        bytes[236..240].copy_from_slice(&[99, 130, 83, 99]);
        bytes.extend_from_slice(&[0, 53, 1, kind, 255]);
        bytes
    }

    #[test]
    fn dhcp_offers_then_acks_the_slirp_lease() {
        let offer = dhcp_reply(&request(DISCOVER), GUEST).unwrap();
        let udp = 14 + 20 + 8;
        assert_eq!(offer[udp + 4..udp + 8], [1, 2, 3, 4]);
        assert_eq!(offer[udp + 16..udp + 20], [10, 0, 2, 15]);
        assert_eq!(offer[udp + 240..udp + 243], [53, 1, OFFER]);
        let ack = dhcp_reply(&request(REQUEST), GUEST).unwrap();
        assert_eq!(ack[udp + 240..udp + 243], [53, 1, ACK]);
        assert!(
            Ipv4Packet::new_checked(&offer[14..])
                .unwrap()
                .verify_checksum()
        );
    }

    #[test]
    fn dhcp_ignores_garbage() {
        assert!(dhcp_reply(&[1; 10], GUEST).is_none());
        assert!(dhcp_reply(&request(7), GUEST).is_none());
        let mut lying = request(DISCOVER);
        lying.truncate(241);
        lying.push(200); // an option longer than the packet
        assert!(dhcp_reply(&lying, GUEST).is_none());
    }

    #[test]
    fn icmp_checksum_verifies() {
        let original = [0x45; 40];
        let frame = prohibited_frame(GUEST, GATEWAY_IP, &original);
        assert_eq!(checksum(&frame[14 + 20..]), 0);
        assert_eq!(frame[14 + 20], 3);
    }
}
