use anyhow::bail;

use anyhow::Context;
use anyhow::Result;
use derive_builder::Builder;
use log::{error, info, warn};
use mio::Interest;
use mio::Token;
use rosenpass_secret_memory::Public;
use rosenpass_secret_memory::Secret;
use rosenpass_util::build::ConstructionSite;
use rosenpass_util::file::StoreValueB64;
use rosenpass_util::functional::run;
use rosenpass_util::functional::ApplyExt;
use rosenpass_util::io::IoResultKindHintExt;
use rosenpass_util::io::SubstituteForIoErrorKindExt;
use rosenpass_util::option::SomeExt;
use rosenpass_util::result::OkExt;
use rosenpass_wireguard_broker::WireguardBrokerMio;
use rosenpass_wireguard_broker::{WireguardBrokerCfg, WG_KEY_LEN};
use zerocopy::AsBytes;

use std::cell::Cell;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::io;
use std::io::stdout;
use std::io::ErrorKind;
use std::io::Write;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::net::SocketAddrV6;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::slice;
use std::time::Duration;
use std::time::Instant;

use crate::protocol::BuildCryptoServer;
use crate::protocol::HostIdentification;
use crate::{
    config::Verbosity,
    protocol::{CryptoServer, MsgBuf, PeerPtr, SPk, SSk, SymKey, Timing},
};
use rosenpass_util::attempt;
use rosenpass_util::b64::B64Display;

const MAX_B64_KEY_SIZE: usize = 32 * 5 / 3;
const MAX_B64_PEER_ID_SIZE: usize = 32 * 5 / 3;

const IPV4_ANY_ADDR: Ipv4Addr = Ipv4Addr::new(0, 0, 0, 0);
const IPV6_ANY_ADDR: Ipv6Addr = Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0);

const UNDER_LOAD_RATIO: f64 = 0.5;
const DURATION_UPDATE_UNDER_LOAD_STATUS: Duration = Duration::from_millis(500);

const BROKER_ID_BYTES: usize = 8;

fn ipv4_any_binding() -> SocketAddr {
    // addr, port
    SocketAddr::V4(SocketAddrV4::new(IPV4_ANY_ADDR, 0))
}

fn ipv6_any_binding() -> SocketAddr {
    // addr, port, flowinfo, scope_id
    SocketAddr::V6(SocketAddrV6::new(IPV6_ANY_ADDR, 0, 0, 0))
}

#[derive(Debug, Default)]
pub struct MioTokenDispenser {
    counter: usize,
}

impl MioTokenDispenser {
    pub fn dispense(&mut self) -> Token {
        let r = self.counter;
        self.counter += 1;
        Token(r)
    }
}

#[derive(Debug, Default)]
pub struct BrokerStore {
    pub store: HashMap<
        Public<BROKER_ID_BYTES>,
        Box<dyn WireguardBrokerMio<Error = anyhow::Error, MioError = anyhow::Error>>,
    >,
}

#[derive(Debug, Clone)]
pub struct BrokerStorePtr(pub Public<BROKER_ID_BYTES>);

#[derive(Debug)]
pub struct BrokerPeer {
    ptr: BrokerStorePtr,
    peer_cfg: Box<dyn WireguardBrokerCfg>,
}

impl BrokerPeer {
    pub fn new(ptr: BrokerStorePtr, peer_cfg: Box<dyn WireguardBrokerCfg>) -> Self {
        Self { ptr, peer_cfg }
    }

    pub fn ptr(&self) -> &BrokerStorePtr {
        &self.ptr
    }
}

#[derive(Default, Debug)]
pub struct AppPeer {
    pub outfile: Option<PathBuf>,
    pub broker_peer: Option<BrokerPeer>,
    pub initial_endpoint: Option<Endpoint>,
    pub current_endpoint: Option<Endpoint>,
}

impl AppPeer {
    pub fn endpoint(&self) -> Option<&Endpoint> {
        self.current_endpoint
            .as_ref()
            .or(self.initial_endpoint.as_ref())
    }
}

#[derive(Default, Debug)]
pub struct WireguardOut {
    // impl KeyOutput
    pub dev: String,
    pub pk: String,
    pub extra_params: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DoSOperation {
    UnderLoad,
    Normal,
}
/// Integration test helpers for AppServer
#[derive(Debug, Builder)]
#[builder(pattern = "owned")]
pub struct AppServerTest {
    /// Enable DoS operation permanently
    #[builder(default = "false")]
    pub enable_dos_permanently: bool,
    /// Terminate application signal
    #[builder(default = "None")]
    pub termination_handler: Option<std::sync::mpsc::Receiver<()>>,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum AppServerIoSource {
    Socket(usize),
    PskBroker(Public<BROKER_ID_BYTES>),
    #[cfg(feature = "experiment_api")]
    MioManager(crate::api::mio::MioManagerIoSource),
}

const EVENT_CAPACITY: usize = 20;

/// Holds the state of the application, namely the external IO
///
/// Responsible for file IO, network IO
// TODO add user control via unix domain socket and stdin/stdout
#[derive(Debug)]
pub struct AppServer {
    #[cfg(feature = "internal_signal_handling_for_coverage_reports")]
    pub term_signal: terminate::TerminateRequested,
    pub crypto_site: ConstructionSite<BuildCryptoServer, CryptoServer>,
    pub sockets: Vec<mio::net::UdpSocket>,
    pub events: mio::Events,
    pub short_poll_queue: VecDeque<mio::event::Event>,
    pub performed_long_poll: bool,
    pub io_source_index: HashMap<mio::Token, AppServerIoSource>,
    pub mio_poll: mio::Poll,
    pub mio_token_dispenser: MioTokenDispenser,
    pub brokers: BrokerStore,
    pub peers: Vec<AppPeer>,
    pub verbosity: Verbosity,
    pub all_sockets_drained: bool,
    pub under_load: DoSOperation,
    pub blocking_polls_count: usize,
    pub non_blocking_polls_count: usize,
    pub unpolled_count: usize,
    pub last_update_time: Instant,
    pub test_helpers: Option<AppServerTest>,
    #[cfg(feature = "experiment_api")]
    pub api_manager: crate::api::mio::MioManager,
}

/// A socket pointer is an index assigned to a socket;
/// right now the index is just the sockets index in AppServer::sockets.
///
/// Holding this as a reference instead of an &mut UdpSocket is useful
/// to deal with the borrow checker, because otherwise we could not refer
/// to a socket and another member of AppServer at the same time.
#[derive(Debug)]
pub struct SocketPtr(pub usize);

impl SocketPtr {
    pub fn get<'a>(&self, srv: &'a AppServer) -> &'a mio::net::UdpSocket {
        &srv.sockets[self.0]
    }

    pub fn get_mut<'a>(&self, srv: &'a mut AppServer) -> &'a mut mio::net::UdpSocket {
        &mut srv.sockets[self.0]
    }

    pub fn send_to(&self, srv: &AppServer, buf: &[u8], addr: SocketAddr) -> anyhow::Result<()> {
        self.get(srv).send_to(buf, addr)?;
        Ok(())
    }
}

/// Index based pointer to a Peer
#[derive(Debug, Copy, Clone)]
pub struct AppPeerPtr(pub usize);

impl AppPeerPtr {
    /// Takes an index based handle and returns the actual peer
    pub fn lift(p: PeerPtr) -> Self {
        Self(p.0)
    }

    /// Returns an index based handle to one Peer
    pub fn lower(&self) -> PeerPtr {
        PeerPtr(self.0)
    }

    pub fn get_app<'a>(&self, srv: &'a AppServer) -> &'a AppPeer {
        &srv.peers[self.0]
    }

    pub fn get_app_mut<'a>(&self, srv: &'a mut AppServer) -> &'a mut AppPeer {
        &mut srv.peers[self.0]
    }

    pub fn set_psk(&self, server: &mut AppServer, psk: &Secret<WG_KEY_LEN>) -> anyhow::Result<()> {
        if let Some(broker) = server.peers[self.0].broker_peer.as_ref() {
            let config = broker.peer_cfg.create_config(psk);
            let broker = server.brokers.store.get_mut(&broker.ptr().0).unwrap();
            broker.set_psk(config)?;
        } else if server.peers[self.0].outfile.is_none() {
            log::warn!("No broker peer found for peer {}", self.0);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum AppPollResult {
    DeleteKey(AppPeerPtr),
    SendInitiation(AppPeerPtr),
    SendRetransmission(AppPeerPtr),
    ReceivedMessage(usize, Endpoint),
}

#[derive(Debug)]
pub enum KeyOutputReason {
    Exchanged,
    Stale,
}

/// Represents a communication partner rosenpass may be sending packets to
///
/// Generally at the start of Rosenpass either no address or a Hostname is known;
/// later when we actually start to receive RespHello packages, we know the specific Address
/// and socket to use with a peer
#[derive(Debug)]
pub enum Endpoint {
    /// Rosenpass supports multiple sockets, so we include the information
    /// which socket an address can be reached on. This probably does not
    /// make much of a difference in most setups where two sockets are just
    /// used to enable dual stack operation; it does make a difference in
    /// more complex use cases.
    ///
    /// For instance it enables using multiple interfaces with overlapping
    /// ip spaces, such as listening on a private IP network and a public IP
    /// at the same time. It also would reply on the same port RespHello was
    /// sent to when listening on multiple ports on the same interface. This
    /// may be required for some arcane firewall setups.
    SocketBoundAddress(SocketBoundEndpoint),
    // A host name or IP address; storing the hostname here instead of an
    // ip address makes sure that we look up the host name whenever we try
    // to make a connection; this may be beneficial in some setups where a host-name
    // at first can not be resolved but becomes resolvable later.
    Discovery(HostPathDiscoveryEndpoint),
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Endpoint::SocketBoundAddress(host) => write!(f, "{}", host),
            Endpoint::Discovery(host) => write!(f, "{}", host),
        }
    }
}

#[derive(Debug)]
pub struct SocketBoundEndpoint {
    /// The socket the address can be reached under; this is generally
    /// determined when we actually receive an RespHello message
    socket: SocketPtr,
    /// Just the address
    addr: SocketAddr,
    /// identifier
    bytes: (usize, [u8; SocketBoundEndpoint::BUFFER_SIZE]),
}

impl std::fmt::Display for SocketBoundEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.addr)
    }
}

impl SocketBoundEndpoint {
    const SOCKET_SIZE: usize = usize::BITS as usize / 8;
    const IPV6_SIZE: usize = 16;
    const PORT_SIZE: usize = 2;
    const SCOPE_ID_SIZE: usize = 4;

    const BUFFER_SIZE: usize = SocketBoundEndpoint::SOCKET_SIZE
        + SocketBoundEndpoint::IPV6_SIZE
        + SocketBoundEndpoint::PORT_SIZE
        + SocketBoundEndpoint::SCOPE_ID_SIZE;

    pub fn new(socket: SocketPtr, addr: SocketAddr) -> Self {
        let bytes = Self::to_bytes(&socket, &addr);
        Self {
            socket,
            addr,
            bytes,
        }
    }

    fn to_bytes(
        socket: &SocketPtr,
        addr: &SocketAddr,
    ) -> (usize, [u8; SocketBoundEndpoint::BUFFER_SIZE]) {
        let mut buf = [0u8; SocketBoundEndpoint::BUFFER_SIZE];
        let addr = match addr {
            SocketAddr::V4(addr) => {
                //Map IPv4-mapped to IPv6 addresses
                let ip = addr.ip().to_ipv6_mapped();
                SocketAddrV6::new(ip, addr.port(), 0, 0)
            }
            SocketAddr::V6(addr) => *addr,
        };
        let mut len: usize = 0;
        buf[len..len + SocketBoundEndpoint::SOCKET_SIZE].copy_from_slice(&socket.0.to_be_bytes());
        len += SocketBoundEndpoint::SOCKET_SIZE;
        buf[len..len + SocketBoundEndpoint::IPV6_SIZE].copy_from_slice(&addr.ip().octets());
        len += SocketBoundEndpoint::IPV6_SIZE;
        buf[len..len + SocketBoundEndpoint::PORT_SIZE].copy_from_slice(&addr.port().to_be_bytes());
        len += SocketBoundEndpoint::PORT_SIZE;
        buf[len..len + SocketBoundEndpoint::SCOPE_ID_SIZE]
            .copy_from_slice(&addr.scope_id().to_be_bytes());
        len += SocketBoundEndpoint::SCOPE_ID_SIZE;
        (len, buf)
    }
}

impl HostIdentification for SocketBoundEndpoint {
    fn encode(&self) -> &[u8] {
        &self.bytes.1[0..self.bytes.0]
    }
}

impl Endpoint {
    /// Start discovery from some addresses
    pub fn discovery_from_addresses(addresses: Vec<SocketAddr>) -> Self {
        Endpoint::Discovery(HostPathDiscoveryEndpoint::from_addresses(addresses))
    }

    /// Start endpoint discovery from a hostname
    pub fn discovery_from_hostname(hostname: String) -> anyhow::Result<Self> {
        let host = HostPathDiscoveryEndpoint::lookup(hostname)?;
        Ok(Endpoint::Discovery(host))
    }

    // Restart discovery; joining two sources of (potential) addresses
    //
    // This is used when the connection to an endpoint is lost in order
    // to include the addresses specified on the command line and the
    // address last used in the discovery process
    pub fn discovery_from_multiple_sources(
        a: Option<&Endpoint>,
        b: Option<&Endpoint>,
    ) -> Option<Self> {
        let sources = match (a, b) {
            (Some(e), None) | (None, Some(e)) => e.addresses().iter().chain(&[]),
            (Some(e1), Some(e2)) => e1.addresses().iter().chain(e2.addresses()),
            (None, None) => return None,
        };
        let lower_size_bound = sources.size_hint().0;
        let mut dedup = std::collections::HashSet::with_capacity(lower_size_bound);
        let mut addrs = Vec::with_capacity(lower_size_bound);
        for a in sources {
            if dedup.insert(a) {
                addrs.push(*a);
            }
        }
        Some(Self::discovery_from_addresses(addrs))
    }

    pub fn send(&self, srv: &AppServer, buf: &[u8]) -> anyhow::Result<()> {
        use Endpoint::*;
        match self {
            SocketBoundAddress(host) => host.socket.send_to(srv, buf, host.addr),
            Discovery(host) => host.send_scouting(srv, buf),
        }
    }

    fn addresses(&self) -> &[SocketAddr] {
        use Endpoint::*;
        match self {
            SocketBoundAddress(host) => slice::from_ref(&host.addr),
            Discovery(host) => host.addresses(),
        }
    }
}

/// Handles host-path discovery
///
/// When rosenpass is started, we either know no peer address
/// or we know a hostname. How to contact this hostname may not
/// be entirely clear for two reasons:
///
/// 1. We have multiple sockets; only a subset of those may be able to contact the host
/// 2. DNS resolution can return multiple addresses
///
/// We could just use the first working socket and the first address returned, but this
/// may be error prone: Some of the sockets may appear to be able to contact the host,
/// but the packets will be dropped. Some of the addresses may appear to be reachable
/// but the packets could be lost.
///
/// In contrast to TCP, UDP has no mechanism to ensure packets actually arrive.
///
/// To robustly handle host path discovery, we try each socket-ip-combination in a round
/// robin fashion; the struct stores the offset of the last used combination internally and
/// and will continue with the next combination on every call.
///
/// Retransmission handling will continue normally; i.e. increasing the distance between
/// retransmissions on every retransmission, until it is long enough to bore a human. Therefor
/// it is important to avoid having a large number of sockets drop packets not just for efficiency
/// but to avoid latency issues too.
///
// TODO: We might consider adjusting the retransmission handling to account for host-path discovery
#[derive(Debug)]
pub struct HostPathDiscoveryEndpoint {
    scouting_state: Cell<(usize, usize)>, // addr_off, sock_off
    addresses: Vec<SocketAddr>,
}

impl std::fmt::Display for HostPathDiscoveryEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.addresses)
    }
}

impl HostPathDiscoveryEndpoint {
    pub fn from_addresses(addresses: Vec<SocketAddr>) -> Self {
        let scouting_state = Cell::new((0, 0));
        Self {
            addresses,
            scouting_state,
        }
    }

    /// Lookup a hostname
    pub fn lookup(hostname: String) -> anyhow::Result<Self> {
        Ok(Self {
            addresses: ToSocketAddrs::to_socket_addrs(&hostname)?.collect(),
            scouting_state: Cell::new((0, 0)),
        })
    }

    pub fn addresses(&self) -> &Vec<SocketAddr> {
        &self.addresses
    }

    fn insert_next_scout_offset(&self, srv: &AppServer, addr_no: usize, sock_no: usize) {
        self.scouting_state.set((
            (addr_no + 1) % self.addresses.len(),
            (sock_no + 1) % srv.sockets.len(),
        ));
    }

    /// Attempt to reach the host
    ///
    /// Will round-robin-try different socket-ip-combinations on each call.
    pub fn send_scouting(&self, srv: &AppServer, buf: &[u8]) -> anyhow::Result<()> {
        let (addr_off, sock_off) = self.scouting_state.get();

        let mut addrs = (self.addresses)
            .iter()
            .enumerate()
            .cycle()
            .skip(addr_off)
            .take(self.addresses.len());
        let mut sockets = (srv.sockets)
            .iter()
            .enumerate()
            .cycle()
            .skip(sock_off)
            .take(srv.sockets.len());

        for (addr_no, addr) in addrs.by_ref() {
            for (sock_no, sock) in sockets.by_ref() {
                let res = sock.send_to(buf, *addr);
                let err = match res {
                    Ok(_) => {
                        self.insert_next_scout_offset(srv, addr_no, sock_no);
                        return Ok(());
                    }
                    Err(e) => e,
                };

                // TODO: replace this by
                // e.kind() == io::ErrorKind::NetworkUnreachable
                // once https://github.com/rust-lang/rust/issues/86442 lands
                let ignore = err
                    .to_string()
                    .starts_with("Address family not supported by protocol");
                if !ignore {
                    warn!("Socket #{} refusing to send to {}: {}", sock_no, addr, err);
                }
            }
        }

        bail!("Unable to send message: All sockets returned errors.")
    }
}

impl AppServer {
    pub fn new(
        keypair: Option<(SSk, SPk)>,
        addrs: Vec<SocketAddr>,
        verbosity: Verbosity,
        test_helpers: Option<AppServerTest>,
    ) -> anyhow::Result<Self> {
        // setup mio
        let mio_poll = mio::Poll::new()?;
        let events = mio::Events::with_capacity(EVENT_CAPACITY);
        let mut mio_token_dispenser = MioTokenDispenser::default();

        // bind each SocketAddr to a socket
        let maybe_sockets: Result<Vec<_>, _> =
            addrs.into_iter().map(mio::net::UdpSocket::bind).collect();
        let mut sockets = maybe_sockets?;

        // When no socket is specified, rosenpass should open one port on all
        // available interfaces best-effort. Here are the cases how this can possibly go:
        //
        // Some operating systems (such as Linux [^linux] and FreeBSD [^freebsd])
        // using IPv6 sockets to handle IPv4 connections; on these systems
        // binding to the `[::]:0` address will typically open a dual-stack
        // socket. Some other systems such as OpenBSD [^openbsd] do not support this feature.
        //
        // Dual-stack systems provide a flag to enable or disable this
        // behavior – the IPV6_V6ONLY flag. OpenBSD supports this flag
        // read-only. MIO[^mio] provides a way to read this flag but not
        // to write it.
        //
        // - One dual-stack IPv6 socket, if the operating supports dual-stack sockets and
        //   correctly reports this
        // - One IPv6 socket and one IPv4 socket if the operating does not support dual stack
        //   sockets or disables them by default assuming this is also correctly reported
        // - One IPv6 socket and no IPv4 socket if IPv6 socket is not dual-stack and opening
        //   the IPv6 socket fails
        // - One IPv4 socket and no IPv6 socket if opening the IPv6 socket fails
        // - One dual-stack IPv6 socket and a redundant IPv4 socket if dual-stack sockets are
        //   supported but the operating system does not correctly report this (specifically,
        //   if the only_v6() call raises an error)
        // - Rosenpass exits if no socket could be opened
        //
        // [^freebsd]: https://man.freebsd.org/cgi/man.cgi?query=ip6&sektion=4&manpath=FreeBSD+6.0-RELEASE
        // [^openbsd]: https://man.openbsd.org/ip6.4
        // [^linux]: https://man7.org/linux/man-pages/man7/ipv6.7.html
        // [^mio]: https://docs.rs/mio/0.8.6/mio/net/struct.UdpSocket.html#method.only_v6
        if sockets.is_empty() {
            macro_rules! try_register_socket {
                ($title:expr, $binding:expr) => {{
                    let r = mio::net::UdpSocket::bind($binding);
                    match r {
                        Ok(sock) => {
                            sockets.push(sock);
                            Some(sockets.len() - 1)
                        }
                        Err(e) => {
                            warn!("Could not bind to {} socket: {}", $title, e);
                            None
                        }
                    }
                }};
            }

            let v6 = try_register_socket!("IPv6", ipv6_any_binding());

            let need_v4 = match v6.map(|no| sockets[no].only_v6()) {
                Some(Ok(v)) => v,
                None => true,
                Some(Err(e)) => {
                    warn!("Unable to detect whether the IPv6 socket supports dual-stack operation: {}", e);
                    true
                }
            };

            if need_v4 {
                try_register_socket!("IPv4", ipv4_any_binding());
            }
        }

        if sockets.is_empty() {
            bail!("No sockets to listen on!")
        }

        // register all sockets to mio
        let mut io_source_index = HashMap::new();
        for (idx, socket) in sockets.iter_mut().enumerate() {
            let mio_token = mio_token_dispenser.dispense();
            mio_poll
                .registry()
                .register(socket, mio_token, Interest::READABLE)?;
            let prev = io_source_index.insert(mio_token, AppServerIoSource::Socket(idx));
            assert!(prev.is_none());
        }

        let crypto_site = match keypair {
            Some((sk, pk)) => ConstructionSite::from_product(CryptoServer::new(sk, pk)),
            None => ConstructionSite::new(BuildCryptoServer::empty()),
        };

        Ok(Self {
            #[cfg(feature = "internal_signal_handling_for_coverage_reports")]
            term_signal: terminate::TerminateRequested::new()?,
            crypto_site,
            peers: Vec::new(),
            verbosity,
            sockets,
            events,
            short_poll_queue: Default::default(),
            performed_long_poll: false,
            io_source_index,
            mio_poll,
            mio_token_dispenser,
            brokers: BrokerStore::default(),
            all_sockets_drained: false,
            under_load: DoSOperation::Normal,
            blocking_polls_count: 0,
            non_blocking_polls_count: 0,
            unpolled_count: 0,
            last_update_time: Instant::now(),
            test_helpers,
            #[cfg(feature = "experiment_api")]
            api_manager: crate::api::mio::MioManager::default(),
        })
    }

    pub fn crypto_server(&self) -> anyhow::Result<&CryptoServer> {
        self.crypto_site
            .product_ref()
            .context("Cryptography handler not initialized")
    }

    pub fn crypto_server_mut(&mut self) -> anyhow::Result<&mut CryptoServer> {
        self.crypto_site
            .product_mut()
            .context("Cryptography handler not initialized")
    }

    pub fn verbose(&self) -> bool {
        matches!(self.verbosity, Verbosity::Verbose)
    }

    pub fn register_listen_socket(&mut self, mut sock: mio::net::UdpSocket) -> anyhow::Result<()> {
        let mio_token = self.mio_token_dispenser.dispense();
        self.mio_poll
            .registry()
            .register(&mut sock, mio_token, mio::Interest::READABLE)?;
        let io_source = self.sockets.len().apply(AppServerIoSource::Socket);
        self.sockets.push(sock);
        self.register_io_source(mio_token, io_source);
        Ok(())
    }

    pub fn register_io_source(&mut self, token: mio::Token, io_source: AppServerIoSource) {
        let prev = self.io_source_index.insert(token, io_source);
        assert!(prev.is_none());
    }

    pub fn unregister_io_source(&mut self, token: mio::Token) {
        let value = self.io_source_index.remove(&token);
        assert!(value.is_some(), "Removed IO source that does not exist");
    }

    pub fn register_broker(
        &mut self,
        broker: Box<dyn WireguardBrokerMio<Error = anyhow::Error, MioError = anyhow::Error>>,
    ) -> Result<BrokerStorePtr> {
        let ptr = Public::from_slice((self.brokers.store.len() as u64).as_bytes());
        if self.brokers.store.insert(ptr, broker).is_some() {
            bail!("Broker already registered");
        }

        let mio_token = self.mio_token_dispenser.dispense();
        let io_source = ptr.apply(AppServerIoSource::PskBroker);
        //Register broker
        self.brokers
            .store
            .get_mut(&ptr)
            .ok_or(anyhow::format_err!("Broker wasn't added to registry"))?
            .register(self.mio_poll.registry(), mio_token)?;
        self.register_io_source(mio_token, io_source);

        Ok(BrokerStorePtr(ptr))
    }

    pub fn unregister_broker(&mut self, ptr: BrokerStorePtr) -> Result<()> {
        let mut broker = self
            .brokers
            .store
            .remove(&ptr.0)
            .context("Broker not found")?;
        self.unregister_io_source(broker.mio_token().unwrap());
        broker.unregister(self.mio_poll.registry())?;
        Ok(())
    }

    pub fn add_peer(
        &mut self,
        psk: Option<SymKey>,
        pk: SPk,
        outfile: Option<PathBuf>,
        broker_peer: Option<BrokerPeer>,
        hostname: Option<String>,
    ) -> anyhow::Result<AppPeerPtr> {
        let PeerPtr(pn) = match &mut self.crypto_site {
            ConstructionSite::Void => bail!("Crypto server construction site is void"),
            ConstructionSite::Builder(builder) => builder.add_peer(psk, pk),
            ConstructionSite::Product(srv) => srv.add_peer(psk, pk)?,
        };
        assert!(pn == self.peers.len());

        let initial_endpoint = hostname
            .map(Endpoint::discovery_from_hostname)
            .transpose()?;
        let current_endpoint = None;
        self.peers.push(AppPeer {
            outfile,
            broker_peer,
            initial_endpoint,
            current_endpoint,
        });
        Ok(AppPeerPtr(pn))
    }

    pub fn event_loop(&mut self) -> anyhow::Result<()> {
        const INIT_SLEEP: f64 = 0.01;
        const MAX_FAILURES: i32 = 10;
        let mut failure_cnt = 0;

        loop {
            let msgs_processed = 0usize;
            let err = match self.event_loop_without_error_handling() {
                Ok(()) => return Ok(()),
                Err(e) => e,
            };

            #[cfg(feature = "internal_signal_handling_for_coverage_reports")]
            {
                let terminated_by_signal = err
                    .downcast_ref::<std::io::Error>()
                    .filter(|e| e.kind() == std::io::ErrorKind::Interrupted)
                    .filter(|_| self.term_signal.value())
                    .is_some();
                if terminated_by_signal {
                    log::warn!(
                        "\
                        Terminated by signal; this signal handler is correct during coverage testing \
                        but should be otherwise disabled"
                    );
                    return Ok(());
                }
            }

            // This should not happen…
            failure_cnt = if msgs_processed > 0 {
                0
            } else {
                failure_cnt + 1
            };
            let sleep = INIT_SLEEP * 2.0f64.powf(f64::from(failure_cnt - 1));
            let tries_left = MAX_FAILURES - (failure_cnt - 1);
            error!(
                "unexpected error after processing {} messages: {:?} {}",
                msgs_processed,
                err,
                err.backtrace()
            );
            if tries_left > 0 {
                error!("re-initializing networking in {sleep}! {tries_left} tries left.");
                std::thread::sleep(Duration::from_secs_f64(sleep));
                continue;
            }

            bail!("too many network failures");
        }
    }

    pub fn event_loop_without_error_handling(&mut self) -> anyhow::Result<()> {
        let (mut rx, mut tx) = (MsgBuf::zero(), MsgBuf::zero());

        /// if socket address for peer is known, call closure
        /// assumes that closure leaves a message in `tx`
        /// assumes that closure returns the length of message in bytes
        macro_rules! tx_maybe_with {
            ($peer:expr, $fn:expr) => {
                attempt!({
                    let p = $peer;
                    if p.get_app(self).endpoint().is_some() {
                        let len = $fn()?;
                        let ep: &Endpoint = p.get_app(self).endpoint().unwrap();
                        ep.send(self, &tx[..len])?;
                    }
                    Ok(())
                })
            };
        }

        loop {
            use crate::protocol::HandleMsgResult;
            use AppPollResult::*;
            use KeyOutputReason::*;

            if let Some(AppServerTest {
                termination_handler: Some(terminate),
                ..
            }) = &self.test_helpers
            {
                if terminate.try_recv().is_ok() {
                    return Ok(());
                }
            }

            enum CryptoSrv {
                Avail,
                Missing,
            }

            let poll_result = self.poll(&mut *rx)?;
            let have_crypto = match self.crypto_site.is_available() {
                true => CryptoSrv::Avail,
                false => CryptoSrv::Missing,
            };

            #[allow(clippy::redundant_closure_call)]
            match (have_crypto, poll_result) {
                (CryptoSrv::Missing, SendInitiation(_)) => {}
                (CryptoSrv::Avail, SendInitiation(peer)) => tx_maybe_with!(peer, || self
                    .crypto_server_mut()?
                    .initiate_handshake(peer.lower(), &mut *tx))?,

                (CryptoSrv::Missing, SendRetransmission(_)) => {}
                (CryptoSrv::Avail, SendRetransmission(peer)) => tx_maybe_with!(peer, || self
                    .crypto_server_mut()?
                    .retransmit_handshake(peer.lower(), &mut *tx))?,

                (CryptoSrv::Missing, DeleteKey(_)) => {}
                (CryptoSrv::Avail, DeleteKey(peer)) => {
                    self.output_key(peer, Stale, &SymKey::random())?;

                    // There was a loss of connection apparently; restart host discovery
                    // starting from the last used address but including all the initially
                    // specified addresses
                    // TODO: We could do this preemptively, before any connection loss actually occurs.
                    let p = peer.get_app_mut(self);
                    p.current_endpoint = Endpoint::discovery_from_multiple_sources(
                        p.current_endpoint.as_ref(),
                        p.initial_endpoint.as_ref(),
                    );
                }

                (CryptoSrv::Missing, ReceivedMessage(_, _)) => {}
                (CryptoSrv::Avail, ReceivedMessage(len, endpoint)) => {
                    let msg_result = match self.under_load {
                        DoSOperation::UnderLoad => {
                            self.handle_msg_under_load(&endpoint, &rx[..len], &mut *tx)
                        }
                        DoSOperation::Normal => {
                            self.crypto_server_mut()?.handle_msg(&rx[..len], &mut *tx)
                        }
                    };
                    match msg_result {
                        Err(ref e) => {
                            self.verbose().then(|| {
                                info!(
                                    "error processing incoming message from {}: {:?} {}",
                                    endpoint,
                                    e,
                                    e.backtrace()
                                );
                            });
                        }

                        Ok(HandleMsgResult {
                            resp,
                            exchanged_with,
                            ..
                        }) => {
                            if let Some(len) = resp {
                                endpoint.send(self, &tx[0..len])?;
                            }

                            if let Some(p) = exchanged_with {
                                let ap = AppPeerPtr::lift(p);
                                ap.get_app_mut(self).current_endpoint = Some(endpoint);

                                // TODO: Maybe we should rather call the key "rosenpass output"?
                                let osk = &self.crypto_server_mut()?.osk(p)?;
                                self.output_key(ap, Exchanged, osk)?;
                            }
                        }
                    }
                }
            };
        }
    }

    fn handle_msg_under_load(
        &mut self,
        endpoint: &Endpoint,
        rx: &[u8],
        tx: &mut [u8],
    ) -> Result<crate::protocol::HandleMsgResult> {
        match endpoint {
            Endpoint::SocketBoundAddress(socket) => self
                .crypto_server_mut()?
                .handle_msg_under_load(rx, &mut *tx, socket),
            Endpoint::Discovery(_) => {
                anyhow::bail!("Host-path discovery is not supported under load")
            }
        }
    }

    pub fn output_key(
        &mut self,
        peer: AppPeerPtr,
        why: KeyOutputReason,
        key: &SymKey,
    ) -> anyhow::Result<()> {
        let peerid = peer.lower().get(self.crypto_server()?).pidt()?;

        if self.verbose() {
            let msg = match why {
                KeyOutputReason::Exchanged => "Exchanged key with peer",
                KeyOutputReason::Stale => "Erasing outdated key from peer",
            };
            info!("{} {}", msg, peerid.fmt_b64::<MAX_B64_PEER_ID_SIZE>());
        }

        let ap = peer.get_app(self);

        if let Some(of) = ap.outfile.as_ref() {
            // This might leave some fragments of the secret on the stack;
            // in practice this is likely not a problem because the stack likely
            // will be overwritten by something else soon but this is not exactly
            // guaranteed. It would be possible to remedy this, but since the secret
            // data will linger in the linux page cache anyways with the current
            // implementation, going to great length to erase the secret here is
            // not worth it right now.
            key.store_b64::<MAX_B64_KEY_SIZE, _>(of)?;
            let why = match why {
                KeyOutputReason::Exchanged => "exchanged",
                KeyOutputReason::Stale => "stale",
            };

            // this is intentionally writing to stdout instead of stderr, because
            // it is meant to allow external detection of a successful key-exchange
            let stdout = stdout();
            let mut stdout = stdout.lock();
            writeln!(
                stdout,
                "output-key peer {} key-file {of:?} {why}",
                peerid.fmt_b64::<MAX_B64_PEER_ID_SIZE>()
            )?;
            stdout.flush()?;
        }

        peer.set_psk(self, key)?;

        Ok(())
    }

    pub fn poll(&mut self, rx_buf: &mut [u8]) -> anyhow::Result<AppPollResult> {
        use crate::protocol::PollResult as C;
        use AppPollResult as A;
        let res = loop {
            // Call CryptoServer's poll (if available)
            let crypto_poll = self
                .crypto_site
                .product_mut()
                .map(|crypto| crypto.poll())
                .transpose()?;

            // Map crypto server's poll result to our poll result
            let io_poll_timeout = match crypto_poll {
                Some(C::DeleteKey(PeerPtr(no))) => break A::DeleteKey(AppPeerPtr(no)),
                Some(C::SendInitiation(PeerPtr(no))) => break A::SendInitiation(AppPeerPtr(no)),
                Some(C::SendRetransmission(PeerPtr(no))) => {
                    break A::SendRetransmission(AppPeerPtr(no))
                }
                Some(C::Sleep(timeout)) => timeout, // No event from crypto-server, do IO
                None => crate::protocol::UNENDING,  // Crypto server is uninitialized, do IO
            };

            // Perform IO (look for a message)
            if let Some((len, addr)) = self.try_recv(rx_buf, io_poll_timeout)? {
                break A::ReceivedMessage(len, addr);
            }
        };

        Ok(res)
    }

    /// Tries to receive a new message
    ///
    /// - might wait for an duration up to `timeout`
    /// - returns immediately if an error occurs
    /// - returns immediately if a new message is received
    pub fn try_recv(
        &mut self,
        buf: &mut [u8],
        timeout: Timing,
    ) -> anyhow::Result<Option<(usize, Endpoint)>> {
        let timeout = Duration::from_secs_f64(timeout);

        // if there is no time to wait on IO, well, then, lets not waste any time!
        if timeout.is_zero() {
            return Ok(None);
        }

        // NOTE when using mio::Poll, there are some particularities (taken from
        // https://docs.rs/mio/latest/mio/struct.Poll.html):
        //
        // - poll() might return readiness, even if nothing is ready
        // - in this case, a WouldBlock error is returned from actual IO operations
        // - after receiving readiness for a source, it must be drained until a WouldBlock
        //   is received
        //
        // This would usually require us to maintain the drainage status of each socket;
        // a socket would only become drained when it returned WouldBlock and only
        // non-drained when receiving a readiness event from mio for it. Then, only the
        // ready sockets should be worked on, ideally without requiring an O(n) search
        // through all sockets for checking their drained status. However, our use-case
        // is primarily heaving one or two sockets (if IPv4 and IPv6 IF_ANY listen is
        // desired on a non-dual-stack OS), thus just checking every socket after any
        // readiness event seems to be good enough™ for now.

        // only poll if we drained all sockets before
        run(|| -> anyhow::Result<()> {
            if !self.all_sockets_drained || !self.short_poll_queue.is_empty() {
                self.unpolled_count += 1;
                return Ok(());
            }

            self.perform_mio_poll_and_register_events(Duration::from_secs(0))?; // Non-blocking poll
            if !self.short_poll_queue.is_empty() {
                // Got some events in non-blocking mode
                self.non_blocking_polls_count += 1;
                return Ok(());
            }

            if !self.performed_long_poll {
                // pass – go perform a full long poll before we enter blocking poll mode
                // to make sure our experimental short poll feature did not miss any events
                // due to being buggy.
                return Ok(());
            }

            // Perform and register blocking poll
            self.blocking_polls_count += 1;
            self.perform_mio_poll_and_register_events(timeout)?;
            self.performed_long_poll = false;

            Ok(())
        })?;

        if let Some(AppServerTest {
            enable_dos_permanently: true,
            ..
        }) = self.test_helpers
        {
            self.under_load = DoSOperation::UnderLoad;
        } else {
            //Reset blocking poll count if waiting for more than BLOCKING_POLL_COUNT_DURATION
            if self.last_update_time.elapsed() > DURATION_UPDATE_UNDER_LOAD_STATUS {
                self.last_update_time = Instant::now();
                let total_polls = self.blocking_polls_count + self.non_blocking_polls_count;

                let load_ratio = if total_polls > 0 {
                    self.non_blocking_polls_count as f64 / total_polls as f64
                } else if self.unpolled_count > 0 {
                    //There are no polls, so we are under load
                    1.0
                } else {
                    0.0
                };

                if load_ratio > UNDER_LOAD_RATIO {
                    self.under_load = DoSOperation::UnderLoad;
                } else {
                    self.under_load = DoSOperation::Normal;
                }

                self.blocking_polls_count = 0;
                self.non_blocking_polls_count = 0;
                self.unpolled_count = 0;
            }
        }

        // Focused polling – i.e. actually using mio::Token – is experimental for now.
        // The reason for this is that we need to figure out how to integrate load detection
        // and focused polling for one. Mio event-based polling also does not play nice with
        // the current function signature and its reentrant design which is focused around receiving UDP socket packages
        // for processing by the crypto protocol server.
        // Besides that, there are also some parts of the code which intentionally block
        // despite available data. This is the correct behavior; e.g. api::mio::Connection blocks
        // further reads from its unix socket until the write buffer is flushed. In other words
        // the connection handler makes sure that there is a buffer to put the response in while
        // before reading further request.
        // The potential problem with this behavior is that we end up ignoring instructions from
        // epoll() to read from the particular sockets, so epoll will return information about that
        // particular – blocked – file descriptor every call. We have only so many event slots and
        // in theory, the event array could fill up entirely with intentionally blocked sockets.
        // We need to figure out how to deal with this situation.
        // Mio uses uses epoll in level-triggered mode, so we could handle taint-tracking for ignored
        // sockets ourselves. The facilities are available in epoll and Mio, but we need to figure out how mio uses those
        // facilities and how we can integrate them here.
        // This will involve rewriting a lot of IO code and we should probably have integration
        // tests  before we approach that.
        //
        // This hybrid approach is not without merit though; the short poll implementation covers
        // all our IO sources, so under contention, rosenpass should generally not hit the long
        // poll mode below. We keep short polling and calling epoll() in non-blocking mode (timeout
        // of zero) until we run out of IO events processed. Then, just before we would perform a
        // blocking poll, we go through all available IO sources to see if we missed anything.
        {
            while let Some(ev) = self.short_poll_queue.pop_front() {
                if let Some(v) = self.try_recv_from_mio_token(buf, ev.token())? {
                    return Ok(Some(v));
                }
            }
        }

        // drain all sockets
        let mut would_block_count = 0;
        for sock_no in 0..self.sockets.len() {
            match self
                .try_recv_from_listen_socket(buf, sock_no)
                .io_err_kind_hint()
            {
                Ok(None) => continue,
                Ok(Some(v)) => {
                    // at least one socket was not drained...
                    self.all_sockets_drained = false;
                    return Ok(Some(v));
                }
                Err((_, ErrorKind::WouldBlock)) => {
                    would_block_count += 1;
                }
                // TODO if one socket continuously returns an error, then we never poll, thus we never wait for a timeout, thus we have a spin-lock
                Err((e, _)) => return Err(e)?,
            }
        }

        // if each socket returned WouldBlock, then we drained them all at least once indeed
        self.all_sockets_drained = would_block_count == self.sockets.len();

        // Process brokers poll
        for (_, broker) in self.brokers.store.iter_mut() {
            broker.process_poll()?;
        }

        // API poll

        #[cfg(feature = "experiment_api")]
        {
            use crate::api::mio::MioManagerContext;
            MioManagerFocus(self).poll()?;
        }

        self.performed_long_poll = true;

        Ok(None)
    }

    fn perform_mio_poll_and_register_events(&mut self, timeout: Duration) -> io::Result<()> {
        self.mio_poll.poll(&mut self.events, Some(timeout))?;
        // Fill the short poll buffer with the acquired events
        self.events
            .iter()
            .cloned()
            .for_each(|v| self.short_poll_queue.push_back(v));
        Ok(())
    }

    fn try_recv_from_mio_token(
        &mut self,
        buf: &mut [u8],
        token: mio::Token,
    ) -> anyhow::Result<Option<(usize, Endpoint)>> {
        let io_source = match self.io_source_index.get(&token) {
            Some(io_source) => *io_source,
            None => {
                log::warn!("No IO source assiociated with mio token ({token:?}). Polling using mio tokens directly is an experimental feature and IO handler should recover when all available io sources are polled. This is a developer error. Please report it.");
                return Ok(None);
            }
        };

        self.try_recv_from_io_source(buf, io_source)
    }

    fn try_recv_from_io_source(
        &mut self,
        buf: &mut [u8],
        io_source: AppServerIoSource,
    ) -> anyhow::Result<Option<(usize, Endpoint)>> {
        match io_source {
            AppServerIoSource::Socket(idx) => self
                .try_recv_from_listen_socket(buf, idx)
                .substitute_for_ioerr_wouldblock(None)?
                .ok(),

            AppServerIoSource::PskBroker(key) => self
                .brokers
                .store
                .get_mut(&key)
                .with_context(|| format!("No PSK broker under key {key:?}"))?
                .process_poll()
                .map(|_| None),

            #[cfg(feature = "experiment_api")]
            AppServerIoSource::MioManager(mmio_src) => {
                use crate::api::mio::MioManagerContext;

                MioManagerFocus(self)
                    .poll_particular(mmio_src)
                    .map(|_| None)
            }
        }
    }

    fn try_recv_from_listen_socket(
        &mut self,
        buf: &mut [u8],
        idx: usize,
    ) -> io::Result<Option<(usize, Endpoint)>> {
        use std::io::ErrorKind as K;
        let (n, addr) = loop {
            match self.sockets[idx].recv_from(buf).io_err_kind_hint() {
                Ok(v) => break v,
                Err((_, K::Interrupted)) => continue,
                Err((e, _)) => return Err(e)?,
            }
        };
        SocketPtr(idx)
            .apply(|sp| SocketBoundEndpoint::new(sp, addr))
            .apply(Endpoint::SocketBoundAddress)
            .apply(|ep| (n, ep))
            .some()
            .ok()
    }

    #[cfg(feature = "experiment_api")]
    pub fn add_api_connection(&mut self, connection: mio::net::UnixStream) -> std::io::Result<()> {
        use crate::api::mio::MioManagerContext;
        MioManagerFocus(self).add_connection(connection)
    }

    #[cfg(feature = "experiment_api")]
    pub fn add_api_listener(&mut self, listener: mio::net::UnixListener) -> std::io::Result<()> {
        use crate::api::mio::MioManagerContext;
        MioManagerFocus(self).add_listener(listener)
    }
}

#[cfg(feature = "experiment_api")]
struct MioManagerFocus<'a>(&'a mut AppServer);

#[cfg(feature = "experiment_api")]
impl crate::api::mio::MioManagerContext for MioManagerFocus<'_> {
    fn mio_manager(&self) -> &crate::api::mio::MioManager {
        &self.0.api_manager
    }

    fn mio_manager_mut(&mut self) -> &mut crate::api::mio::MioManager {
        &mut self.0.api_manager
    }

    fn app_server(&self) -> &AppServer {
        self.0
    }

    fn app_server_mut(&mut self) -> &mut AppServer {
        self.0
    }
}

/// These signal handlers are used exclusively used during coverage testing
/// to ensure that the llvm-cov can produce reports during integration tests
/// with multiple processes where subprocesses are terminated via kill(2).
///
/// llvm-cov does not support producing coverage reports when the process exits
/// through a signal, so this is necessary.
///
/// The functionality of exiting gracefully upon reception of a terminating signal
/// is desired for the production variant of Rosenpass, but we should make sure
/// to use a higher quality implementation; in particular, we should use signalfd(2).
///
#[cfg(feature = "internal_signal_handling_for_coverage_reports")]
mod terminate {
    use signal_hook::flag::register as sig_register;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    /// Automatically register a signal handler for common termination signals;
    /// whether one of these signals was issued can be polled using [Self::value].
    ///
    /// The signal handler is not removed when this struct goes out of scope.
    #[derive(Debug)]
    pub struct TerminateRequested {
        value: Arc<AtomicBool>,
    }

    impl TerminateRequested {
        /// Register signal handlers watching for common termination signals
        pub fn new() -> anyhow::Result<Self> {
            let value = Arc::new(AtomicBool::new(false));
            for sig in signal_hook::consts::TERM_SIGNALS.iter().copied() {
                sig_register(sig, Arc::clone(&value))?;
            }
            Ok(Self { value })
        }

        /// Check whether a termination signal has been set
        pub fn value(&self) -> bool {
            self.value.load(Ordering::Relaxed)
        }
    }
}
