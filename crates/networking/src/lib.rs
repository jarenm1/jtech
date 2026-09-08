use protocol::{
    ClientMessage, InputPacket, MAX_DATAGRAM, MAX_FRAME, MAX_PLAYERS, PROTOCOL_VERSION,
    ServerMessage, Snapshot, decode, encode,
};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    time::{Duration, Instant},
};

const MAX_PENDING: usize = 2 * 1024 * 1024;
const IO_BUDGET: usize = 256 * 1024;
const MESSAGE_BUDGET: usize = 64;
#[derive(Clone, Copy, Debug, Default)]
pub struct TrafficStats {
    pub received_bytes: u64,
    pub sent_bytes: u64,
    pub rejected_datagrams: u64,
}
struct Framed {
    socket: TcpStream,
    input: Vec<u8>,
    output: VecDeque<Vec<u8>>,
    offset: usize,
    pending: usize,
}
impl Framed {
    fn new(socket: TcpStream) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        socket.set_nodelay(true)?;
        Ok(Self {
            socket,
            input: Vec::new(),
            output: VecDeque::new(),
            offset: 0,
            pending: 0,
        })
    }
    fn queue(&mut self, message: &impl Serialize) -> io::Result<()> {
        let bytes = encode(message, MAX_FRAME)?;
        if self.pending + bytes.len() + 4 > MAX_PENDING {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "reliable queue full",
            ));
        }
        let mut frame = Vec::with_capacity(bytes.len() + 4);
        frame.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        frame.extend_from_slice(&bytes);
        self.pending += frame.len();
        self.output.push_back(frame);
        Ok(())
    }
    fn poll<T: DeserializeOwned>(&mut self, stats: &mut TrafficStats) -> io::Result<Vec<T>> {
        let mut budget = IO_BUDGET;
        while budget > 0 {
            let Some(frame) = self.output.front() else {
                break;
            };
            let end = frame.len().min(self.offset + budget);
            match self.socket.write(&frame[self.offset..end]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.offset += n;
                    self.pending -= n;
                    budget -= n;
                    stats.sent_bytes += n as u64;
                    if self.offset == frame.len() {
                        self.output.pop_front();
                        self.offset = 0;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        let mut buffer = [0_u8; 8192];
        let mut budget = IO_BUDGET;
        while budget > 0 && self.input.len() < MAX_FRAME + 4 {
            let count = buffer
                .len()
                .min(budget)
                .min(MAX_FRAME + 4 - self.input.len());
            match self.socket.read(&mut buffer[..count]) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    self.input.extend_from_slice(&buffer[..n]);
                    budget -= n;
                    stats.received_bytes += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        let mut result = Vec::new();
        let mut consumed = 0;
        while result.len() < MESSAGE_BUDGET && self.input.len() - consumed >= 4 {
            let length =
                u32::from_le_bytes(self.input[consumed..consumed + 4].try_into().unwrap()) as usize;
            if length == 0 || length > MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid frame length",
                ));
            }
            if self.input.len() - consumed < length + 4 {
                break;
            }
            result.push(decode(
                &self.input[consumed + 4..consumed + 4 + length],
                MAX_FRAME,
            )?);
            consumed += length + 4;
        }
        if consumed > 0 {
            self.input.drain(..consumed);
        }
        Ok(result)
    }
}
#[derive(Default)]
pub struct Incoming {
    pub reliable: Vec<ServerMessage>,
    pub snapshots: Vec<Snapshot>,
}
pub struct ClientTransport {
    tcp: Framed,
    udp: UdpSocket,
    pub stats: TrafficStats,
}
impl ClientTransport {
    pub fn connect(addr: SocketAddr) -> io::Result<Self> {
        let socket = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        let mut tcp = Framed::new(socket)?;
        let udp = UdpSocket::bind(if addr.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })?;
        udp.connect(addr)?;
        udp.set_nonblocking(true)?;
        tcp.queue(&ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        })?;
        Ok(Self {
            tcp,
            udp,
            stats: TrafficStats::default(),
        })
    }
    pub fn send(&mut self, message: ClientMessage) -> io::Result<()> {
        self.tcp.queue(&message)
    }
    pub fn send_inputs(&mut self, packet: InputPacket) -> io::Result<()> {
        if packet.inputs.is_empty() || packet.inputs.len() > 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "input redundancy must be 1..=8",
            ));
        }
        let bytes = encode(&packet, MAX_DATAGRAM)?;
        match self.udp.send(&bytes) {
            Ok(n) => {
                self.stats.sent_bytes += n as u64;
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(e),
        }
    }
    pub fn poll(&mut self) -> io::Result<Incoming> {
        let reliable = self.tcp.poll(&mut self.stats)?;
        let mut snapshots = Vec::new();
        let mut bytes = [0_u8; MAX_DATAGRAM + 1];
        for _ in 0..64 {
            match self.udp.recv(&mut bytes) {
                Ok(n) => {
                    self.stats.received_bytes += n as u64;
                    match decode::<Snapshot>(&bytes[..n], MAX_DATAGRAM) {
                        Ok(snapshot) if snapshot.players.len() < MAX_PLAYERS => {
                            snapshots.push(snapshot)
                        }
                        _ => self.stats.rejected_datagrams += 1,
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(Incoming {
            reliable,
            snapshots,
        })
    }
}
struct Peer {
    tcp: Framed,
    address: SocketAddr,
    udp: Option<SocketAddr>,
    session: u64,
    connected: Instant,
    active: bool,
    last_seen: Instant,
}
#[derive(Default)]
pub struct ServerIncoming {
    pub connected: Vec<(u64, u64)>,
    pub disconnected: Vec<u64>,
    pub reliable: Vec<(u64, ClientMessage)>,
    pub inputs: Vec<(u64, InputPacket)>,
}
pub struct ServerTransport {
    listener: TcpListener,
    udp: UdpSocket,
    peers: HashMap<u64, Peer>,
    next_id: u64,
    pub stats: TrafficStats,
}
impl ServerTransport {
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let udp = UdpSocket::bind(listener.local_addr()?)?;
        udp.set_nonblocking(true)?;
        Ok(Self {
            listener,
            udp,
            peers: HashMap::new(),
            next_id: 1,
            stats: TrafficStats::default(),
        })
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
    pub fn disconnect(&mut self, id: u64) {
        self.peers.remove(&id);
    }
    pub fn send(&mut self, id: u64, message: &ServerMessage) -> io::Result<()> {
        self.peers
            .get_mut(&id)
            .ok_or(io::ErrorKind::NotConnected)?
            .tcp
            .queue(message)
    }
    pub fn snapshot(&mut self, id: u64, snapshot: &Snapshot) -> io::Result<()> {
        let Some(address) = self.peers.get(&id).and_then(|peer| peer.udp) else {
            return Ok(());
        };
        let bytes = encode(snapshot, MAX_DATAGRAM)?;
        match self.udp.send_to(&bytes, address) {
            Ok(n) => {
                self.stats.sent_bytes += n as u64;
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(e),
        }
    }
    pub fn poll(&mut self) -> io::Result<ServerIncoming> {
        let mut incoming = ServerIncoming::default();
        for _ in 0..MAX_PLAYERS {
            match self.listener.accept() {
                Ok((socket, address)) => {
                    if self.peers.len() >= MAX_PLAYERS {
                        continue;
                    }
                    let mut random = [0_u8; 8];
                    getrandom::fill(&mut random).map_err(|e| io::Error::other(e.to_string()))?;
                    let session = u64::from_ne_bytes(random);
                    if session == 0 || self.peers.values().any(|p| p.session == session) {
                        continue;
                    }
                    let id = self.next_id;
                    self.next_id += 1;
                    self.peers.insert(
                        id,
                        Peer {
                            tcp: Framed::new(socket)?,
                            address,
                            udp: None,
                            session,
                            connected: Instant::now(),
                            active: false,
                            last_seen: Instant::now(),
                        },
                    );
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        for (&id, peer) in &mut self.peers {
            if (!peer.active && peer.connected.elapsed() > Duration::from_secs(5))
                || peer.last_seen.elapsed() > Duration::from_secs(30)
            {
                incoming.disconnected.push(id);
                continue;
            }
            match peer.tcp.poll::<ClientMessage>(&mut self.stats) {
                Ok(messages) => {
                    for message in messages {
                        if !peer.active {
                            if matches!(
                                message,
                                ClientMessage::Hello {
                                    version: PROTOCOL_VERSION
                                }
                            ) {
                                peer.active = true;
                                incoming.connected.push((id, peer.session));
                            } else {
                                incoming.disconnected.push(id);
                                break;
                            }
                        } else if matches!(message, ClientMessage::Hello { .. }) {
                            incoming.disconnected.push(id);
                            break;
                        } else {
                            incoming.reliable.push((id, message));
                        }
                    }
                }
                Err(_) => incoming.disconnected.push(id),
            }
        }
        for id in &incoming.disconnected {
            self.peers.remove(id);
        }
        let mut bytes = [0_u8; MAX_DATAGRAM + 1];
        for _ in 0..256 {
            match self.udp.recv_from(&mut bytes) {
                Ok((n, address)) => {
                    self.stats.received_bytes += n as u64;
                    let Ok(packet) = decode::<InputPacket>(&bytes[..n], MAX_DATAGRAM) else {
                        self.stats.rejected_datagrams += 1;
                        continue;
                    };
                    if packet.inputs.is_empty() || packet.inputs.len() > 8 {
                        self.stats.rejected_datagrams += 1;
                        continue;
                    }
                    let Some((&id, peer)) = self.peers.iter_mut().find(|(_, peer)| {
                        peer.active
                            && peer.session == packet.session
                            && peer.address.ip() == address.ip()
                            && peer.udp.is_none_or(|bound| bound == address)
                    }) else {
                        self.stats.rejected_datagrams += 1;
                        continue;
                    };
                    if packet.inputs.iter().any(|input| {
                        !input.yaw.is_finite()
                            || !input.pitch.is_finite()
                            || input.movement.iter().any(|v| !v.is_finite())
                            || input.sequence == 0
                    }) {
                        self.stats.rejected_datagrams += 1;
                        continue;
                    }
                    peer.udp = Some(address);
                    peer.last_seen = Instant::now();
                    incoming.inputs.push((id, packet));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(incoming)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_frame_disconnects_without_waiting_for_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (receiver, _) = listener.accept().unwrap();
        sender
            .write_all(&((MAX_FRAME + 1) as u32).to_le_bytes())
            .unwrap();
        let mut framed = Framed::new(receiver).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match framed.poll::<ClientMessage>(&mut TrafficStats::default()) {
                Err(error) => {
                    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                    break;
                }
                Ok(_) => assert!(Instant::now() < deadline),
            }
            std::thread::yield_now();
        }
    }
    #[test]
    fn reliable_backpressure_is_bounded() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_receiver, _) = listener.accept().unwrap();
        let mut framed = Framed::new(sender).unwrap();
        let message = ServerMessage::Disconnect {
            reason: "x".repeat(64000),
        };
        for _ in 0..100 {
            if framed.queue(&message).is_err() {
                assert!(framed.pending <= MAX_PENDING);
                return;
            }
        }
        panic!("queue never applied backpressure");
    }
}
