//! Communication primitives for `febft`, such as wire message formats.

use std::collections::BTreeSet;
use std::convert::TryFrom;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use async_tls::{TlsAcceptor, TlsConnector};
use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use either::{Either, Left, Right};
use futures::AsyncReadExt;

use futures::io::{ AsyncWriteExt};
use futures_timer::Delay;
use intmap::IntMap;
use log::{debug, error, warn};

use rustls::{ClientConfig, ClientConnection, ServerConfig, ServerConnection, ServerName};

#[cfg(feature = "serialize_serde")]
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use febft_common::error::*;
use febft_common::crypto::signature::{KeyPair, PublicKey};
use febft_common::prng::ThreadSafePrng;
use febft_common::{prng, socket, threadpool};
use febft_common::async_runtime as rt;
use febft_common::crypto::hash::Digest;
use febft_common::ordering::{Orderable, SeqNo};
use febft_common::socket::{AsyncListener, AsyncSocket, SecureSocketRecvAsync, SecureSocketRecvSync, SecureSocketSend, SecureSocketSendAsync, SecureSocketSendSync, SocketSendAsync, SocketSendSync, SyncListener, SyncSocket};
use crate::benchmarks::CommStats;
use crate::incoming_peer_handling::{ConnectedPeer, PeerIncomingRqHandling};
use crate::message::{Header, NetworkMessage, NetworkMessageKind, SerializedMessage, StoredSerializedNetworkMessage, WireMessage};
use crate::peer_sending_threads::ConnectionHandle;
use crate::ping_handler::PingHandler;
use crate::serialize::{Buf, Serializable};


pub mod message;
pub mod incoming_peer_handling;
pub mod peer_sending_threads;
pub mod serialize;
pub mod ping_handler;
pub mod benchmarks;

/// A `NodeId` represents the id of a process in the BFT system.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
#[cfg_attr(feature = "serialize_serde", derive(Serialize, Deserialize))]
#[repr(transparent)]
pub struct NodeId(pub u32);

impl NodeId {
    pub fn targets_u32<I>(into_iterator: I) -> impl Iterator<Item=Self>
        where
            I: IntoIterator<Item=u32>,
    {
        into_iterator.into_iter().map(Self)
    }

    pub fn targets<I>(into_iterator: I) -> impl Iterator<Item=Self>
        where
            I: IntoIterator<Item=usize>,
    {
        into_iterator.into_iter().map(NodeId::from)
    }

    pub fn id(&self) -> u32 {
        self.0
    }
}

impl From<u32> for NodeId {
    #[inline]
    fn from(id: u32) -> NodeId {
        NodeId(id)
    }
}

impl From<u64> for NodeId {
    #[inline]
    fn from(id: u64) -> NodeId {
        NodeId(id as u32)
    }
}

impl From<usize> for NodeId {
    #[inline]
    fn from(id: usize) -> NodeId {
        NodeId(id as u32)
    }
}

impl From<NodeId> for usize {
    #[inline]
    fn from(id: NodeId) -> usize {
        id.0 as usize
    }
}

impl From<NodeId> for u64 {
    #[inline]
    fn from(id: NodeId) -> u64 {
        id.0 as u64
    }
}

impl From<NodeId> for u32 {
    #[inline]
    fn from(id: NodeId) -> u32 {
        id.0 as u32
    }
}

// TODO: maybe researh cleaner way to share the connections
// hashmap between two async tasks on the client
#[derive(Clone)]
pub enum PeerTx {
    // NOTE: comments below are invalid because of the changes we made to
    // the research branch; we now share a `SendNode` with the execution
    // layer, to allow faster reply delivery!
    //
    // clients need shared access to the hashmap; the `Arc` on the second
    // lock allows us to take ownership of a copy of the socket, so we
    // don't block the thread with the guard of the first lock waiting
    // on the second one
    Client {
        first_cli: NodeId,
        connected: Arc<DashMap<u64, ConnectionHandle>>,
    },
    // replicas don't need shared access to the hashmap, so
    // we only need one lock (to restrict I/O to one producer at a time)
    Server {
        first_cli: NodeId,
        connected: Arc<DashMap<u64, ConnectionHandle>>,
    },
}

impl PeerTx {
    ///Add a tx peer connection to the registry
    ///Requires knowing the first_cli
    pub fn add_peer(&self, client_id: u64, socket: ConnectionHandle) {
        let previous_conn = match self {
            PeerTx::Client { connected, .. } => connected.insert(client_id, socket),
            PeerTx::Server { connected, .. } => connected.insert(client_id, socket),
        };

        if let Some(existing_conn) = previous_conn {
            existing_conn.close();
        };
    }

    pub fn connected(&self) -> usize {
        match self {
            //+ 1 as we don't store the loopback connection here, it's in the replica handling.
            PeerTx::Client { connected, .. } => connected.len() + 1,
            PeerTx::Server { connected, .. } => connected.len() + 1
        }
    }

    pub fn find_peer(&self, client_id: u64) -> Option<ConnectionHandle> {
        match self {
            PeerTx::Client { connected, .. } => {
                let option = {
                    connected
                        .get(&client_id)
                        .map(|reference| (reference.value()).clone())
                };

                option
            }
            PeerTx::Server { connected, .. } => {
                let option = {
                    connected
                        .get(&client_id)
                        .map(|reference| (reference.value()).clone())
                };

                option
            }
        }
    }

    pub fn disconnect_peer(&self, peer_id: u64) -> Option<ConnectionHandle> {
        match self {
            PeerTx::Client { connected, .. } => {
                connected.remove(&peer_id).map(|(_u, v)| {
                    v
                })
            }
            PeerTx::Server { connected, .. } => {
                connected.remove(&peer_id).map(|(_u, v)| {
                    v
                })
            }
        }
    }
}

pub struct NodeShared {
    my_key: KeyPair,
    peer_keys: IntMap<PublicKey>,
}

pub struct SignDetached {
    shared: Arc<NodeShared>,
}

impl SignDetached {
    pub fn key_pair(&self) -> &KeyPair {
        &self.shared.my_key
    }
}

/// The connection type used for connections
/// Stores the connector needed
#[derive(Clone)]
pub enum TlsNodeConnector {
    Async(TlsConnector),
    Sync(Arc<ClientConfig>),
}

/// Establish safe tls node connections
#[derive(Clone)]
pub enum TlsNodeAcceptor {
    Async(TlsAcceptor),
    Sync(Arc<ServerConfig>),
}

/// Accept node connections
pub enum NodeConnectionAcceptor {
    Async(AsyncListener),
    Sync(SyncListener),
}

/// Container for handles to other processes in the system.
///
/// A `Node` constitutes the core component used in the wire
/// communication between processes.
pub struct Node<M: Serializable + 'static> {
    id: NodeId,
    first_cli: NodeId,
    //Handles the incoming connections' buffering and request collection
    //This is polled by the proposer for client requests and by the
    client_pooling: PeerIncomingRqHandling<NetworkMessage<M>>,
    //Handles the outgoing connection references
    peer_tx: PeerTx,
    //Handles the Pseudo random number generation for this node
    rng: ThreadSafePrng,
    //Some shared node information
    shared: Arc<NodeShared>,
    //A set of the nodes we are currently attempting to connect to
    currently_connecting: Mutex<BTreeSet<NodeId>>,
    //The connector used to establish secure connections between peers
    connector: TlsNodeConnector,
    //Handles pings and timeouts of said pings
    ping_handler: Arc<PingHandler>,
    //An address map of all the peers
    peer_addrs: IntMap<PeerAddr>,

    comm_stats: Option<Arc<CommStats>>,

    pub sent_rqs: Option<Arc<Vec<DashMap<u64, ()>>>>,
    pub recv_rqs: Option<Arc<RwLock<Vec<DashMap<u64, ()>>>>>,
}

///Represents the server addresses of a peer
///Clients will only have 1 address while replicas will have 2 addresses (1 for facing clients,
/// 1 for facing replicas)
pub struct PeerAddr {
    client_addr: (SocketAddr, String),
    replica_addr: Option<(SocketAddr, String)>,
}

impl PeerAddr {
    pub fn new(client_addr: (SocketAddr, String)) -> Self {
        Self {
            client_addr,
            replica_addr: None,
        }
    }

    pub fn new_replica(
        client_addr: (SocketAddr, String),
        replica_addr: (SocketAddr, String),
    ) -> Self {
        Self {
            client_addr,
            replica_addr: Some(replica_addr),
        }
    }
}

/// Represents a configuration used to bootstrap a `Node`.
pub struct NodeConfig {
    /// The total number of nodes in the system.
    ///
    /// Typically, BFT systems set this parameter to 4.
    /// This parameter is constrained by the following: `n >= 3*f + 1`.
    pub n: usize,
    /// The number of nodes allowed to fail in the system.
    ///
    /// Typically, BFT systems set this parameter to 1.
    pub f: usize,
    /// The id of this `Node`.
    pub id: NodeId,
    /// The first id assigned to a client`Node`.
    ///
    /// Every other client id of the form `first_cli + i`.
    pub first_cli: NodeId,
    ///The max size for batches of client operations
    pub batch_size: usize,
    /// The addresses of all nodes in the system (including clients),
    /// as well as the domain name associated with each address.
    ///
    /// For any `NodeConfig` assigned to `c`, the IP address of
    /// `c.addrs[&c.id]` should be equivalent to `localhost`.
    pub addrs: IntMap<PeerAddr>,
    /// The list of public keys of all nodes in the system.
    pub pk: IntMap<PublicKey>,
    /// The secret key of this particular `Node`.
    pub sk: KeyPair,
    /// The TLS configuration used to connect to replica nodes. (from client nodes)
    pub async_client_config: ClientConfig,
    /// The TLS configuration used to accept connections from client nodes.
    pub async_server_config: ServerConfig,
    ///The TLS configuration used to accept connections from replica nodes (Synchronously)
    pub sync_server_config: ServerConfig,
    ///The TLS configuration used to connect to replica nodes (from replica nodes) (Synchronousy)
    pub sync_client_config: ClientConfig,
    ///How many clients should be placed in a single collecting pool (seen in incoming_peer_handling)
    pub clients_per_pool: usize,
    ///The timeout for batch collection in each client pool.
    /// (The first to reach between batch size and timeout)
    pub batch_timeout_micros: u64,
    ///How long should a client pool sleep for before attempting to collect requests again
    /// (It actually will sleep between 3/4 and 5/4 of this value, to make sure they don't all sleep / wake up at the same time)
    pub batch_sleep_micros: u64,
    ///Statistics for communications
    pub comm_stats: Option<Arc<CommStats>>,
    ///Path for the db file
    pub db_path: String,
}

// max no. of messages allowed in the channel
const NODE_CHAN_BOUND: usize = 1024;

// max no. of SendTo's to inline before doing a heap alloc
const NODE_VIEWSIZ: usize = 16;

type SendTos<M> = SmallVec<[SendTo<M>; NODE_VIEWSIZ]>;

type SerializedSendTos<M> = SmallVec<[SerializedSendTo<M>; NODE_VIEWSIZ]>;

impl<M> Node<M>
    where
        M: Serializable + 'static
{
    fn setup_connector(
        _sync_connector: Arc<ClientConfig>,
        _async_connector: TlsConnector,
    ) -> TlsNodeConnector {
        //TODO: Support Async connectors as well
        TlsNodeConnector::Async(_async_connector)
    }

    fn setup_acceptor(
        _sync_acceptor: Arc<ServerConfig>,
        _async_acceptor: TlsAcceptor,
    ) -> TlsNodeAcceptor {
        TlsNodeAcceptor::Async(_async_acceptor)
    }

    async fn setup_socket(
        id: &NodeId,
        server_addr: &SocketAddr,
    ) -> Result<NodeConnectionAcceptor> {
        let mut server_addr = server_addr.clone();

        server_addr.set_ip(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
        //TODO: Maybe add support for asynchronous listeners?
        debug!("{:?} // Binding to address (clients) {:?}", id, server_addr);

        //
        // let socket = socket::bind_sync_server(server_addr).wrapped_msg(
        //     ErrorKind::Communication,
        //     format!("Failed to bind to address {:?}", server_addr).as_str(),
        // )?;

        let socket = socket::bind_async_server(server_addr).await.wrapped_msg(
            ErrorKind::Communication,
            format!("Failed to bind to address {:?}", server_addr).as_str(),
        )?;

        Ok(NodeConnectionAcceptor::Async(socket))
    }

    async fn setup_client_facing_socket(
        id: NodeId,
        cfg: &NodeConfig,
    ) -> Result<NodeConnectionAcceptor> {
        debug!("{:?} // Attempt to setup client facing socket.", id);

        let peer_addr = cfg.addrs.get(id.into()).ok_or(Error::simple_with_msg(
            ErrorKind::Communication,
            "Failed to get client facing IP",
        ))?;

        let server_addr = &peer_addr.client_addr;

        Self::setup_socket(&id, &server_addr.0).await
    }

    async fn setup_replica_facing_socket(
        id: NodeId,
        cfg: &NodeConfig,
    ) -> Result<Option<NodeConnectionAcceptor>> {
        let peer_addr = cfg.addrs.get(id.into()).ok_or(Error::simple_with_msg(
            ErrorKind::Communication,
            "Failed to get replica facing IP",
        ))?;

        //Initialize the replica<->replica facing server
        let replica_listener = if id >= cfg.first_cli {
            //Clients don't have a replica<->replica facing server
            None
        } else {
            let replica_facing_addr =
                peer_addr
                    .replica_addr
                    .as_ref()
                    .ok_or(Error::simple_with_msg(
                        ErrorKind::Communication,
                        "Failed to get replica facing IP",
                    ))?;
            Some(Self::setup_socket(&id, &replica_facing_addr.0).await?)
        };

        Ok(replica_listener)
    }

    ///Sets up the thread or task (depending on runtime) to receive new connection attempts
    fn setup_socket_connection_workers_socket(
        self: Arc<Self>,
        node_connector: TlsNodeAcceptor,
        listener: NodeConnectionAcceptor,
    ) {
        match (node_connector, listener) {
            (TlsNodeAcceptor::Sync(cfg), NodeConnectionAcceptor::Sync(sync_listener)) => {
                let first_cli = self.first_client_id();

                let my_id = self.id();

                let sync_acceptor = cfg.clone();

                std::thread::Builder::new()
                    .name(format!("{:?} connection acceptor", self.id()))
                    .spawn(move || {
                        self.rx_side_accept_sync(first_cli, my_id, sync_listener, sync_acceptor);
                    })
                    .expect("Failed to start replica's connection acceptor");
            }
            (TlsNodeAcceptor::Async(cfg), NodeConnectionAcceptor::Async(async_listener)) => {
                let first_cli = self.first_client_id();

                let my_id = self.id();

                let async_acceptor = cfg.clone();

                rt::spawn(self.rx_side_accept(first_cli, my_id, async_listener, async_acceptor));
            }
            (_, _) => {
                unreachable!()
            }
        }
    }

    /// Bootstrap a `Node`, i.e. create connections between itself and its
    /// peer nodes.
    ///
    /// Rogue messages (i.e. not pertaining to the bootstrapping protocol)
    /// are returned in a `Vec`.
    pub async fn bootstrap(
        cfg: NodeConfig,
    ) -> Result<(Arc<Self>, Vec<NetworkMessage<M>>)> {
        let id = cfg.id;

        // initial checks of correctness
        if cfg.n < (3 * cfg.f + 1) {
            return Err("Invalid number of replicas").wrapped(ErrorKind::Communication);
        }

        if id >= NodeId::from(cfg.n) && id < cfg.first_cli {
            return Err("Invalid node ID").wrapped(ErrorKind::Communication);
        }

        debug!("Initializing sockets.");

        //Initialize the client facing server
        let client_listener = Self::setup_client_facing_socket(id, &cfg).await?;

        let replica_listener = Self::setup_replica_facing_socket(id, &cfg).await?;

        debug!("Initializing TLS configurations.");

        let async_acceptor: TlsAcceptor = cfg.async_server_config.into();
        let async_connector: TlsConnector = cfg.async_client_config.into();

        let sync_acceptor = Arc::new(cfg.sync_server_config);
        let sync_connector = Arc::new(cfg.sync_client_config);

        // node def
        let peer_tx = if id >= cfg.first_cli {
            PeerTx::Client {
                first_cli: cfg.first_cli,
                connected: Arc::new(DashMap::new()),
            }
        } else {
            PeerTx::Server {
                first_cli: cfg.first_cli,
                connected: Arc::new(DashMap::new()),
            }
        };

        debug!("Initializing client facing connections");

        let connector = Self::setup_connector(sync_connector, async_connector);

        let acceptor = Self::setup_acceptor(sync_acceptor, async_acceptor);

        let shared = Arc::new(NodeShared {
            my_key: cfg.sk,
            peer_keys: cfg.pk,
        });

        debug!("Initializing node peer handling.");

        let ping_handler = PingHandler::new();

        //Setup all the peer message reception handling.
        let peers = PeerIncomingRqHandling::new(
            cfg.id,
            cfg.first_cli,
            cfg.batch_size,
            cfg.clients_per_pool,
            cfg.batch_timeout_micros,
            cfg.batch_sleep_micros,
        );

        let rng = ThreadSafePrng::new();

        //TESTING
        let sent_rqs = if id > cfg.first_cli {
            None

            /*Some(Arc::new(
                std::iter::repeat_with(|| DashMap::with_capacity(20000))
                    .take(30)
                    .collect(),
            ))*/
        } else {
            None
        };

        let rcv_rqs =
            if id < cfg.first_cli {
                None

                //
                // //We want the replicas to log recved requests
                // let arc = Arc::new(RwLock::new(
                //     std::iter::repeat_with(|| { DashMap::with_capacity(20000) })
                //         .take(30)
                //         .collect()));
                //
                // let rqs = arc.clone();
                //
                // std::thread::Builder::new().name(String::from("Logging thread")).spawn(move || {
                //     loop {
                //         let new_vec: Vec<DashMap<u64, ()>> = std::iter::repeat_with(|| { DashMap::with_capacity(20000) })
                //             .take(30)
                //             .collect();
                //
                //         let mut old_vec = {
                //             let mut write_guard = rqs.write();
                //
                //             std::mem::replace(&mut *write_guard, new_vec)
                //         };
                //
                //         let mut print = String::new();
                //
                //         for bucket in old_vec {
                //             for (key, _) in bucket.into_iter() {
                //                 print = print + " , " + &*format!("{}", key);
                //             }
                //         }
                //
                //         println!("{}", print);
                //
                //         std::thread::sleep(Duration::from_secs(1));
                //     }
                // }).expect("Failed to start logging thread");
                //
                // Some(arc)
            } else { None };

        debug!("{:?} // Initializing node reference", id);

        let node = Arc::new(Node {
            id,
            rng,
            shared,
            peer_tx,
            client_pooling: peers,
            connector,
            peer_addrs: cfg.addrs,
            first_cli: cfg.first_cli,
            comm_stats: cfg.comm_stats,
            sent_rqs,
            ping_handler,
            recv_rqs: rcv_rqs,
            currently_connecting: Mutex::new(BTreeSet::new()),
        });

        let rx_node_clone = node.clone();

        debug!("{:?} // Initializing replica connection workers", id);

        //Setup the worker threads for receiving new connections before starting to connect to
        //other peers
        if let Some(replica_listener) = replica_listener {
            rx_node_clone
                .clone()
                .setup_socket_connection_workers_socket(
                    acceptor.clone(),
                    replica_listener,
                );
        }

        //Setup client listener
        {
            rx_node_clone
                .clone()
                .setup_socket_connection_workers_socket(
                    acceptor,
                    client_listener,
                );
        }

        let n = cfg.n;

        debug!("{:?} // Connect to other replicas", id);

        // tx side (connect to replica)
        if id < cfg.first_cli {
            //We are a replica, connect to all other replicas

            match node.connector.clone() {
                TlsNodeConnector::Async(async_connector) => {
                    let node = node.clone();

                    node.tx_side_connect_async(n as u32, async_connector).await;
                }
                TlsNodeConnector::Sync(sync_connector) => {
                    let node = node.clone();

                    node.tx_side_connect_sync(n as u32, sync_connector);
                }
            }
        } else {
            //Connect to all replicas as a client
            match node.connector.clone() {
                TlsNodeConnector::Async(async_connector) => {
                    let node = node.clone();

                    node.tx_side_connect_async(n as u32, async_connector).await;
                }
                TlsNodeConnector::Sync(sync_connector) => {
                    let node = node.clone();
                    threadpool::execute(move || {
                        node.tx_side_connect_sync(n as u32, sync_connector);
                    });
                }
            }
        }

        let rogue = Vec::new();

        //TODO: We need to receive the amount of replicas in the quorum
        while node.client_pooling.replica_count() < cfg.n || node.peer_tx.connected() < cfg.n {
            //Any received messages will be handled by the connection pool buffers
            debug!(
                "{:?} // Connected to {} RX replicas, {} TX",
                node.id,
                node.client_pooling.replica_count(),
                node.peer_tx.connected()
            );

            Delay::new(Duration::from_millis(500)).await;
        }

        debug!(
            "Found all nodes required {}, {} RX, {} TX",
            node.client_pooling.replica_count(),
            node.client_pooling.replica_count(),
            node.peer_tx.connected()
        );

        // success
        Ok((node, rogue))
    }

    pub fn batch_size(&self) -> usize {
        self.client_pooling.batch_size()
    }

    fn resolve_client_rx_connection(
        &self,
        node_id: NodeId,
    ) -> Option<Arc<ConnectedPeer<NetworkMessage<M>>>> {
        self.client_pooling.resolve_peer_conn(node_id)
    }

    // clone the shared data and pass it to a new object
    pub fn sign_detached(&self) -> SignDetached {
        let shared = Arc::clone(&self.shared);
        SignDetached { shared }
    }

    /// Returns the public key of the node with the given id `id`.
    pub fn get_public_key(&self, id: NodeId) -> Option<&PublicKey> {
        self.shared.peer_keys.get(id.into())
    }

    /// Get my own key pair
    pub fn get_key_pair(&self) -> &KeyPair {
        &self.shared.my_key
    }

    /// Reports the id of this `Node`.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// Reports the id of the first client.
    pub fn first_client_id(&self) -> NodeId {
        self.first_cli
    }

    pub fn connector(&self) -> &TlsNodeConnector {
        &self.connector
    }

    /// Returns a `SendNode` sharing the same handles as this `Node`.
    pub fn send_node(self: &Arc<Self>) -> SendNode<M> {
        SendNode {
            id: self.id,
            first_cli: self.first_cli,
            rng: prng::State::new(),
            shared: Arc::clone(&self.shared),
            peer_tx: self.peer_tx.clone(),
            parent_node: Arc::clone(self),
            channel: Arc::clone(self.loopback_channel()),
            comm_stats: self.comm_stats.clone(),
        }
    }

    /// Returns a handle to the loopback channel of this `Node`. (Sending messages to ourselves)
    pub fn loopback_channel(&self) -> &Arc<ConnectedPeer<NetworkMessage<M>>> {
        self.client_pooling.loopback_connection()
    }

    /// Send a `SystemMessage` to a single destination.
    ///
    /// This method is somewhat more efficient than calling `broadcast()`
    /// on a single target id.
    pub fn send(
        &self,
        message: NetworkMessageKind<M>,
        target: NodeId,
        flush: bool,
    ) {
        let comm_stats = received_network_rq!(&self.comm_stats);

        match self.resolve_client_rx_connection(target) {
            None => {
                error!(
                    "Failed to send message to client {:?} as the connection to it was not found!",
                    target
                );
            }
            Some(conn) => {
                let send_to = match Self::send_to(flush, self.id, target, None, conn, &self.peer_tx)
                {
                    Ok(send_to) => send_to,
                    Err(err) => {
                        error!("{:?} // {:?}", self.id, err);
                        return;
                    }
                };

                let my_id = self.id;
                let nonce = self.rng.next_state();

                Self::send_impl(
                    message,
                    send_to,
                    my_id,
                    target,
                    self.first_cli,
                    nonce,
                    comm_stats,
                );
            }
        };
    }

    /// Send a `SystemMessage` to a single destination.
    ///
    /// This method is somewhat more efficient than calling `broadcast()`
    /// on a single target id.
    ///
    /// This variant of `send()` signs the sent message.
    pub fn send_signed(
        &self,
        message: NetworkMessageKind<M>,
        target: NodeId,
    ) {
        let comm_stats = received_network_rq!(&self.comm_stats);

        match self.resolve_client_rx_connection(target) {
            None => {
                error!(
                    "Failed to send message to client {:?} as the connection to it was not found!",
                    target
                );
            }
            Some(conn) => {
                let send_to = match Self::send_to(
                    true,
                    self.id,
                    target,
                    Some(&self.shared),
                    conn,
                    &self.peer_tx,
                ) {
                    Ok(send_to) => send_to,
                    Err(error) => {
                        error!("{:?} // {:?}", self.id, error);
                        return;
                    }
                };

                let my_id = self.id;

                let nonce = self.rng.next_state();

                Self::send_impl(
                    message,
                    send_to,
                    my_id,
                    target,
                    self.first_cli,
                    nonce,
                    comm_stats,
                );
            }
        };
    }

    #[inline]
    fn send_impl(
        message: NetworkMessageKind<M>,
        send_to: SendTo<M>,
        my_id: NodeId,
        target: NodeId,
        _first_cli: NodeId,
        nonce: u64,
        comm_stats: Option<(Arc<CommStats>, Instant)>,
    ) {
        threadpool::execute(move || {
            // serialize
            start_measurement!(start_serialization);

            let mut buf = Vec::with_capacity(512);

            let digest = serialize::serialize_digest::<Vec<u8>, M>(&message, &mut buf).unwrap();

            let buf = Bytes::from(buf);

            message_digest_time!(&comm_stats, start_serialization);

            //Measuring time taken to get to the point of sending the message
            //We don't actually want to measure how long it takes to send the message
            start_measurement!(before_send_time);

            // send
            if my_id == target {
                // Right -> our turn

                //Send to myself, always synchronous since only replicas send to themselves
                send_to.value(Right((message, nonce, digest, buf)), None);

                message_sent_own!(&comm_stats, before_send_time, my_id);
            } else {
                // Left -> peer turn
                send_to.value(Left((nonce, digest, buf)), None);

                message_dispatched!(&comm_stats, before_send_time, target);
            }
        });
    }

    /// Broadcast a `SystemMessage` to a group of nodes.
    ///
    /// This variant of broadcast does not sign the messages that are sent
    pub fn broadcast(
        &self,
        message: NetworkMessageKind<M>,
        targets: impl Iterator<Item=NodeId>,
    ) {
        let comm_stats = received_network_rq!(&self.comm_stats);

        let (mine, others) = self.send_tos(self.id, &self.peer_tx, None, targets);

        let nonce = self.rng.next_state();

        Self::broadcast_impl(message, mine, others, self.first_cli, nonce, comm_stats);
    }

    /// Broadcast a `SystemMessage` to a group of nodes.
    ///
    /// This variant of `broadcast()` signs the sent message.
    pub fn broadcast_signed(
        &self,
        message: NetworkMessageKind<M>,
        targets: impl Iterator<Item=NodeId>,
    ) {
        start_measurement!(start_time);

        let (mine, others) = self.send_tos(self.id, &self.peer_tx, Some(&self.shared), targets);

        let nonce = self.rng.next_state();

        let comm_stats = if let Some(comm_stats) = &self.comm_stats {
            Some((comm_stats.clone(), start_time))
        } else {
            None
        };

        Self::broadcast_impl(message, mine, others, self.first_cli, nonce, comm_stats);
    }

    pub fn broadcast_serialized(&self, messages: IntMap<StoredSerializedNetworkMessage<M>>) {
        let start_time = Instant::now();
        let headers = messages.values().map(|stored| stored.header());

        let (mine, others) = self.serialized_send_tos(self.id, &self.peer_tx, headers);

        let comm_stats = if let Some(comm_stats) = &self.comm_stats {
            Some((comm_stats.clone(), start_time))
        } else {
            None
        };

        Self::broadcast_serialized_impl(messages, mine, others, self.first_client_id(), comm_stats);
    }

    #[inline]
    fn broadcast_serialized_impl(
        mut messages: IntMap<StoredSerializedNetworkMessage<M>>,
        my_send_to: Option<SerializedSendTo<M>>,
        other_send_tos: SerializedSendTos<M>,
        _first_client: NodeId,
        comm_stats: Option<(Arc<CommStats>, Instant)>,
    ) {
        threadpool::execute(move || {
            // send to ourselves
            if let Some(send_to) = my_send_to {
                let id = match &send_to {
                    SerializedSendTo::Me { id, .. } => *id,
                    _ => unreachable!(),
                };

                let (header, message) = messages
                    .remove(id.into())
                    .map(|stored| stored.into_inner())
                    .unwrap();

                //Measuring time taken to get to the point of sending the message
                //We don't actually want to measure how long it takes to send the message
                start_measurement!(before_send_time);

                send_to.value(header, message);

                message_sent_own!(&comm_stats, before_send_time, id);
            }

            // send to others
            for send_to in other_send_tos {
                let id = match &send_to {
                    SerializedSendTo::Peers { id, .. } => *id,
                    _ => unreachable!(),
                };

                let (header, message) = messages
                    .remove(id.into())
                    .map(|stored| stored.into_inner())
                    .unwrap();

                //Measuring time taken to get to the point of sending the message
                //We don't actually want to measure how long it takes to send the message
                start_measurement!(before_send_time);

                send_to.value(header, message);

                message_dispatched!(&comm_stats, before_send_time, id);
            }
        });
    }

    #[inline]
    fn broadcast_impl(
        message: NetworkMessageKind<M>,
        my_send_to: Option<SendTo<M>>,
        other_send_tos: SendTos<M>,
        _first_cli: NodeId,
        nonce: u64,
        comm_stats: Option<(Arc<CommStats>, Instant)>,
    ) {
        threadpool::execute(move || {
            start_measurement!(start_serialization);

            // serialize
            //TODO: Actually make this work well
            let mut buf = Vec::with_capacity(512);

            let digest = match serialize::serialize_digest::<Vec<u8>, M>(&message, &mut buf) {
                Ok(dig) => dig,
                Err(err) => {
                    error!("Failed to serialize message {:?}. Message is {:?}", err, message);

                    panic!("Failed to serialize message {:?}", err);
                }
            };

            let buf = Bytes::from(buf);

            message_digest_time!(&comm_stats, start_serialization);

            let rq_key =  None;

            // send to ourselves
            if let Some(send_to) = my_send_to {
                let id = match &send_to {
                    SendTo::Me { my_id, .. } => *my_id,
                    _ => unreachable!(),
                };

                let buf = buf.clone();

                let comm_stats = comm_stats.clone();

                //Measuring time taken to get to the point of sending the message
                //We don't actually want to measure how long it takes to send the message
                start_measurement!(before_send_time);

                // Right -> our turn
                send_to.value(Right((message, nonce, digest, buf)), rq_key.clone());

                message_sent_own!(&comm_stats, before_send_time, id);
            }

            // send to others
            for send_to in other_send_tos {
                let id = match &send_to {
                    SendTo::Peers { peer_id, .. } => *peer_id,
                    _ => unreachable!(),
                };

                let buf = buf.clone();

                let comm_stats = comm_stats.clone();

                //Measuring time taken to get to the point of sending the message
                //We don't actually want to measure how long it takes to send the message
                start_measurement!(before_send_time);

                // Left -> Other peers turn
                send_to.value(Left((nonce, digest, buf)), rq_key.clone());

                message_dispatched!(&comm_stats, before_send_time, id);
            }

            // NOTE: an either enum is used, which allows
            // rustc to prove only one task gets ownership
            // of the `message`, i.e. `Right` = ourselves
        });
    }

    #[inline]
    fn send_tos(
        &self,
        my_id: NodeId,
        peer_tx: &PeerTx,
        shared: Option<&Arc<NodeShared>>,
        targets: impl Iterator<Item=NodeId>,
    ) -> (Option<SendTo<M>>, SendTos<M>) {
        let mut my_send_to = None;
        let mut other_send_tos = SendTos::new();

        self.create_send_tos(
            my_id,
            shared,
            peer_tx,
            targets,
            &mut my_send_to,
            &mut other_send_tos,
        );

        (my_send_to, other_send_tos)
    }

    #[inline]
    fn serialized_send_tos<'a>(
        &self,
        my_id: NodeId,
        peer_tx: &PeerTx,
        headers: impl Iterator<Item=&'a Header>,
    ) -> (Option<SerializedSendTo<M>>, SerializedSendTos<M>) {
        let mut my_send_to = None;
        let mut other_send_tos = SerializedSendTos::new();

        self.create_serialized_send_tos(
            my_id,
            peer_tx,
            headers,
            &mut my_send_to,
            &mut other_send_tos,
        );

        (my_send_to, other_send_tos)
    }

    #[inline]
    fn create_serialized_send_tos<'a>(
        &self,
        my_id: NodeId,
        peer_tx: &PeerTx,
        headers: impl Iterator<Item=&'a Header>,
        mine: &mut Option<SerializedSendTo<M>>,
        others: &mut SerializedSendTos<M>,
    ) {
        for header in headers {
            let id = header.to();
            if id == my_id {
                let s = SerializedSendTo::Me {
                    id,
                    //get our own channel to send to ourselves
                    tx: self.loopback_channel().clone(),
                };
                *mine = Some(s);
            } else {
                let rx = self.resolve_client_rx_connection(id);

                let (sock, tx) = match (peer_tx.find_peer(id.id() as u64), rx) {
                    (None, None) => {
                        error!("Could not find socket nor rx for peer {:?}", id.id());

                        continue;
                    }
                    (None, Some(tx)) => {
                        error!(
                            "Cound not find socket but found rx, closing it {:?}",
                            id.id()
                        );

                        tx.disconnect();

                        continue;
                    }
                    (Some(_sock), None) => {
                        error!("Found socket but didn't find rx? Closing {:?}", id.id());

                        continue;
                    }
                    (Some(socket), Some(tx)) => (socket, tx),
                };

                //Get the RX channel for the corresponding peer to mark as disconnected if the sending fails

                let s = SerializedSendTo::Peers {
                    id,
                    our_id: my_id,
                    peer_tx: peer_tx.clone(),
                    sock,
                    //Get the RX channel for the peer to mark as DCed if it fails
                    tx,
                };

                others.push(s);
            }
        }
    }

    #[inline]
    fn create_send_tos(
        &self,
        my_id: NodeId,
        shared: Option<&Arc<NodeShared>>,
        tx_peers: &PeerTx,
        targets: impl Iterator<Item=NodeId>,
        mine: &mut Option<SendTo<M>>,
        others: &mut SendTos<M>,
    ) {
        for id in targets {
            if id == my_id {
                let s = SendTo::Me {
                    my_id,
                    //get our own channel to send to ourselves
                    tx: self.loopback_channel().clone(),
                    shared: shared.map(|sh| Arc::clone(sh)),
                };
                *mine = Some(s);
            } else {
                let sock = tx_peers.find_peer(id.id() as u64);

                let rx_conn = self.resolve_client_rx_connection(id);

                let (sock, rx_conn) = match (sock, rx_conn) {
                    (None, None) => {
                        error!("Could not find socket nor rx for peer {:?}", id.id());

                        continue;
                    }
                    (None, Some(tx)) => {
                        error!(
                            "Cound not find socket but found rx, closing it {:?}",
                            id.id()
                        );

                        tx.disconnect();

                        continue;
                    }
                    (Some(sock), None) => {
                        error!("Found socket but didn't find rx? Closing {:?}", id.id());

                        sock.close();

                        tx_peers.disconnect_peer(id.into());

                        continue;
                    }
                    (Some(socket), Some(tx)) => (socket, tx),
                };

                let s = SendTo::Peers {
                    sock,
                    my_id,
                    peer_id: id,
                    flush: true,
                    //Get the RX channel for the peer to mark as DCed if it fails
                    tx: rx_conn,
                    shared: shared.map(|sh| Arc::clone(sh)),
                    peer_tx: tx_peers.clone(),
                };

                others.push(s);
            }
        }
    }

    #[inline]
    fn send_to(
        flush: bool,
        my_id: NodeId,
        peer_id: NodeId,
        shared: Option<&Arc<NodeShared>>,
        cli: Arc<ConnectedPeer<NetworkMessage<M>>>,
        peer_tx: &PeerTx,
    ) -> Result<SendTo<M>> {
        let shared = shared.map(|sh| Arc::clone(sh));
        if my_id == peer_id {
            Ok(SendTo::Me {
                shared,
                my_id,
                tx: cli,
            })
        } else {
            let sock = match peer_tx.find_peer(peer_id.id() as u64) {
                None => {
                    return Err(Error::simple_with_msg(
                        ErrorKind::Communication,
                        "Failed to find peer id",
                    ));
                }
                Some(sock) => sock,
            }.clone();

            Ok(SendTo::Peers {
                flush,
                sock,
                shared,
                peer_id,
                my_id,
                tx: cli,
                peer_tx: peer_tx.clone(),
            })
        }
    }

    /// Check if we are currently connected to a provided node (in terms of message reception)
    pub fn is_connected_to_rx(&self, id: NodeId) -> bool {
        self.client_pooling.resolve_peer_conn(id).is_some()
    }

    /// Are we connected to a node (in terms of message sending)
    pub fn is_connected_to_tx(&self, id: NodeId) -> bool {
        self.peer_tx.find_peer(id.0 as u64).is_some()
    }

    //Get how many pending messages are in the requests channel
    pub fn rqs_len_from_clients(&self) -> usize {
        self.client_pooling.rqs_len_from_clients()
    }

    //Receive messages from the clients we are connected to
    pub fn receive_from_clients(
        &self,
        timeout: Option<Duration>,
    ) -> Result<Vec<NetworkMessage<M>>> {
        self.client_pooling.receive_from_clients(timeout)
    }

    pub fn try_recv_from_clients(
        &self,
    ) -> Result<Option<Vec<NetworkMessage<M>>>> {
        self.client_pooling.try_receive_from_clients()
    }

    //Receive messages from the replicas we are connected to
    pub fn receive_from_replicas(&self) -> Result<NetworkMessage<M>> {
        self.client_pooling.receive_from_replicas()
    }

    /// Registers the newly created transmission socket to the peer
    fn handle_connected_tx(self: &Arc<Self>, peer_id: NodeId, sock: SecureSocketSend) {
        debug!("{:?} // Connected TX to peer {:?}", self.id, peer_id);

        let conn_handle = match sock {
            SecureSocketSend::Async(socket) => {
                peer_sending_threads::initialize_async_sending_task_for(
                    Arc::clone(self),
                    peer_id.clone(),
                    socket,
                    self.comm_stats.clone(),
                )
            }
            SecureSocketSend::Sync(socket) => {
                peer_sending_threads::initialize_sync_sending_thread_for(
                    Arc::clone(self),
                    peer_id.clone(),
                    socket,
                    self.comm_stats.clone(),
                    self.sent_rqs.clone(),
                )
            }
        };

        self.peer_tx.add_peer(peer_id.id() as u64, conn_handle);
    }

    ///Connect to all other replicas in the cluster, utilizing tokio (utilizing regular
    /// synchronous APIs)
    #[inline]
    pub async fn tx_side_connect_async(self: Arc<Self>, n: u32, connector: TlsConnector) {
        for peer_id in NodeId::targets_u32(0..n).filter(|&id| id != self.id()) {
            let clone = self.clone();
            let _connector = connector.clone();

            Self::tx_connect_node_async(clone, peer_id, None);
        }
    }

    ///Connect to all other replicas in the cluster, but without using tokio (utilizing regular
    /// synchronous APIs)
    #[inline]
    //#[instrument(skip(self, n, connector))]
    pub fn tx_side_connect_sync(self: Arc<Self>, n: u32, connector: Arc<ClientConfig>) {
        for peer_id in NodeId::targets_u32(0..n).filter(|&id| id != self.id()) {
            let clone = self.clone();
            let _connector = connector.clone();

            Self::tx_connect_node_sync(clone, peer_id, None);
        }
    }

    /// Check if we are already attempting to connect to a given node.
    fn is_currently_connecting_to_node(&self, peer_id: NodeId) -> bool {
        let guard = self.currently_connecting.lock().unwrap();

        guard.contains(&peer_id)
    }

    fn register_currently_connecting_to_node(&self, peer_id: NodeId) -> bool {
        let mut guard = self.currently_connecting.lock().unwrap();

        guard.insert(peer_id)
    }

    fn unregister_currently_connecting_to_node(&self, peer_id: NodeId) -> bool {
        let mut guard = self.currently_connecting.lock().unwrap();

        guard.remove(&peer_id)
    }

    ///Connect to a given node.
    /// Also accepts a callback that is called when the connection is completed
    /// (Either not successfully or successfully)
    ///
    /// Spawns a task to handle the outgoing connection attempt
    pub fn tx_connect_node_async(
        self: Arc<Self>,
        peer_id: NodeId,
        callback: Option<Box<dyn FnOnce(bool) + Send>>,
    ) {
        rt::spawn(async move {
            if !self.register_currently_connecting_to_node(peer_id) {
                warn!(
                "{:?} // Tried to connect to node that I'm already connecting to {:?}",
                self.id, peer_id
            );
                return;
            }

            if self.is_connected_to_tx(peer_id) {
                match self.ping_handler.ping_peer(&self, peer_id) {
                    Ok(mut result_handle) => {
                        let ping_result = result_handle.recv_resp_async().await;

                        match ping_result {
                            Ok(_) => {
                                //Our connection is fine, we should not create a new one
                                self.unregister_currently_connecting_to_node(peer_id);
                                return;
                            }
                            Err(error) => {
                                debug!("Peer {:?} is not reachable. Attempting to reconnect. {:?}",
                                    peer_id, error);
                            }
                        }
                    }
                    Err(error) => {
                        error!("Failed to ping peer {:?} for {:?}", peer_id, error);
                    }
                }

                self.peer_tx.disconnect_peer(peer_id.into());
            }

            debug!("{:?} // Connecting to the node {:?}", self.id, peer_id);

            let mut rng = prng::State::new();

            let addr = match self.peer_addrs.get(peer_id.id() as u64) {
                None => {
                    error!(
                    "{:?} // Failed to find peer address for peer {:?}",
                    self.id, peer_id
                );

                    return;
                }
                Some(addr) => addr,
            };

            let nonce = rng.next_state();

            //Get the correct IP for us to address the node
            //If I'm a client I will always use the client facing addr
            //While if I'm a replica I'll connect to the replica addr (clients only have this addr)
            let peer_addr = if self.id >= self.first_cli {
                addr.client_addr.clone()
            } else {
                //We are a replica, but we are connecting to a client, so
                //We need the client addr.
                if peer_id >= self.first_cli {
                    addr.client_addr.clone()
                } else {
                    match addr.replica_addr.as_ref() {
                        Some(addr) => addr,
                        None => {
                            error!(
                            "{:?} // Failed to find IP address for peer {:?}",
                            self.id, peer_id
                        );

                            return;
                        }
                    }
                        .clone()
                }
            };

            debug!(
            "{:?} // Starting connection to node {:?} with address {:?}",
            self.id(),
            peer_id,
            peer_addr.0
        );

            let my_id = self.id();
            let first_cli = self.first_client_id();

            let connector = match &self.connector {
                TlsNodeConnector::Async(connector) => { connector }
                TlsNodeConnector::Sync(_) => { panic!("Failed, trying to use sync connector in async mode") }
            }.clone();

            self.tx_side_connect_task(
                my_id, first_cli, peer_id, nonce, connector, peer_addr, callback,
            ).await;
        });
    }

    ///Connect to a given node.
    /// Also accepts a callback that is called when the connection is completed
    /// (Either not successfully or successfully)
    ///
    /// This will spawn a new thread to handle the establishment of the new connection
    //TODO: Make this use the correct type of connections
    // depending on what is configured.
    pub fn tx_connect_node_sync(
        self: Arc<Self>,
        peer_id: NodeId,
        callback: Option<Box<dyn FnOnce(bool) + Send>>,
    ) {
        std::thread::Builder::new().name(format!("Tx connection thread {:?}", peer_id))
            .spawn(move || {
                if !self.register_currently_connecting_to_node(peer_id) {
                    warn!(
                "{:?} // Tried to connect to node that I'm already connecting to {:?}",
                self.id, peer_id
                );

                    return;
                }

                if self.is_connected_to_tx(peer_id) {
                    match self.ping_handler.ping_peer(&self, peer_id) {
                        Ok(mut result) => {
                            let result = result.recv_resp();
                            match result {
                                Ok(_) => {
                                    //Our connection to this peer is fine, we shouldn't try to reconnect
                                    debug!("Attempted to reconnect to peer {:?} but the current connection appears to be fine.", peer_id);

                                    self.unregister_currently_connecting_to_node(peer_id);
                                    return;
                                }
                                Err(error) => {
                                    debug!("Peer {:?} is not reachable. Attempting to reconnect. {:?}", peer_id, error);
                                }
                            }
                        }
                        Err(error) => {
                            error!("Failed to ping peer {:?} for {:?}", peer_id, error);
                        }
                    }

                    self.peer_tx.disconnect_peer(peer_id.into());
                }

                debug!("{:?} // Connecting to the node {:?}", self.id, peer_id);

                let mut rng = prng::State::new();

                let addr = match self.peer_addrs.get(peer_id.id() as u64) {
                    None => {
                        error!("{:?} // Failed to find peer address for peer {:?}",
                            self.id, peer_id);

                        return;
                    }
                    Some(addr) => addr,
                };

                let nonce = rng.next_state();

                //Get the correct IP for us to address the node
                //If I'm a client I will always use the client facing addr
                //While if I'm a replica I'll connect to the replica addr (clients only have this addr)
                let peer_addr = if self.id >= self.first_cli {
                    addr.client_addr.clone()
                } else {
                    //We are a replica, but we are connecting to a client, so
                    //We need the client addr.
                    if peer_id >= self.first_cli {
                        addr.client_addr.clone()
                    } else {
                        match addr.replica_addr.as_ref() {
                            Some(addr) => addr,
                            None => {
                                error!("{:?} // Failed to find IP address for peer {:?}",
                                    self.id, peer_id);

                                return;
                            }
                        }.clone()
                    }
                };

                debug!("{:?} // Starting connection to node {:?} with address {:?}",
                self.id(),peer_id,peer_addr.0
                );

                let my_id = self.id();
                let first_cli = self.first_client_id();

                let connector = match &self.connector {
                    TlsNodeConnector::Async(_) => {
                        panic!("Failed using async connector in sync mode");
                    }
                    TlsNodeConnector::Sync(connector) => {
                        connector
                    }
                }.clone();

                self.tx_side_connect_task_sync(
                    my_id, first_cli, peer_id, nonce, connector, peer_addr, callback,
                );
            }).expect("Failed to create thread to handle connection attempt");
    }

    ///Connect to a particular replica
    /// Should be called from a threadpool as initializing a thread just for this
    /// Would be kind of overkill
    fn tx_side_connect_task_sync(
        self: Arc<Self>,
        my_id: NodeId,
        first_cli: NodeId,
        peer_id: NodeId,
        nonce: u64,
        connector: Arc<ClientConfig>,
        (addr, hostname): (SocketAddr, String),
        callback: Option<Box<dyn FnOnce(bool) + Send>>,
    ) {
        const SECS: u64 = 1;
        const RETRY: usize = 3 * 60;

        // NOTE:
        // ========
        //
        // 1) not an issue if `tx` is closed, this is not a
        // permanently running task, so channel send failures
        // are tolerated
        //
        // 2) try to connect up to `RETRY` times, then announce
        // failure with a channel send op
        for _try in 0..RETRY {
            debug!(
                "Attempting to connect to node {:?} with addr {:?} for the {} time",
                peer_id, addr, _try
            );

            match socket::connect_sync(addr) {
                Ok(mut sock) => {
                    // create header
                    let (header, _) =
                        WireMessage::new(my_id, peer_id,
                                         Buf::new(), nonce,
                                         None, None).into_inner();

                    // serialize header
                    let mut buf = [0; Header::LENGTH];
                    header.serialize_into(&mut buf[..]).unwrap();

                    // send header
                    if let Err(_) = sock.write_all(&buf[..]) {
                        // errors writing -> faulty connection;
                        // drop this socket
                        break;
                    }

                    if let Err(_) = sock.flush() {
                        // errors flushing -> faulty connection;
                        // drop this socket
                        break;
                    }

                    // TLS handshake; drop connection if it fails
                    let sock = if peer_id >= first_cli || my_id >= first_cli {
                        SecureSocketSendSync::new_plain(sock)
                    } else {
                        let dns_ref = match ServerName::try_from(hostname.as_str()) {
                            Ok(server_name) => server_name,
                            Err(err) => {
                                error!("Failed to parse DNS name {:?}", err);

                                break;
                            }
                        };

                        if let Ok(session) = ClientConnection::new(connector.clone(), dns_ref) {
                            SecureSocketSendSync::new_tls(session, sock)
                        } else {
                            error!("Failed to establish tls connection.");

                            break;
                        }
                    };

                    let final_sock = SecureSocketSend::Sync(SocketSendSync::new(sock));

                    // success
                    self.handle_connected_tx(peer_id, final_sock);

                    self.unregister_currently_connecting_to_node(peer_id);

                    if let Some(callback) = callback {
                        callback(true);
                    }

                    return;
                }
                Err(err) => {
                    error!(
                        "{:?} // Error on connecting to {:?} addr {:?}: {:?}",
                        self.id, peer_id, addr, err
                    );
                }
            }

            // sleep for `SECS` seconds and retry
            std::thread::sleep(Duration::from_secs(SECS));
        }

        debug!(
            "{:?} // Failed to connect to the node {:?}",
            self.id, peer_id
        );

        self.unregister_currently_connecting_to_node(peer_id);

        if let Some(callback) = callback {
            callback(false);
        }
    }

    //#[instrument(skip(self, first_cli, nonce, connector, callback))]
    async fn tx_side_connect_task(
        self: Arc<Self>,
        my_id: NodeId,
        first_cli: NodeId,
        peer_id: NodeId,
        nonce: u64,
        connector: TlsConnector,
        (addr, hostname): (SocketAddr, String),
        callback: Option<Box<dyn FnOnce(bool) + Send>>,
    ) {
        const SECS: u64 = 1;
        const RETRY: usize = 3 * 60;

        // NOTE:
        // ========
        //
        // 1) not an issue if `tx` is closed, this is not a
        // permanently running task, so channel send failures
        // are tolerated
        //
        // 2) try to connect up to `RETRY` times, then announce
        // failure with a channel send op
        for _try in 0..RETRY {
            debug!(
                "Attempting to connect to node {:?} with addr {:?} for the {} time",
                peer_id, addr, _try
            );

            match socket::connect_async(addr).await {
                Ok(mut sock) => {

                    // create header
                    let (header, _) =
                        WireMessage::new(my_id, peer_id,
                                         Bytes::new(), nonce,
                                         None, None).into_inner();

                    // serialize header
                    let mut buf = [0; Header::LENGTH];
                    header.serialize_into(&mut buf[..]).unwrap();

                    // send header
                    if let Err(err) = sock.write_all(&buf[..]).await {
                        // errors writing -> faulty connection;
                        // drop this socket
                        error!("{:?} // Failed to connect to the node {:?} {:?} ", self.id, peer_id, err);
                        break;
                    }

                    if let Err(err) = sock.flush().await {
                        // errors flushing -> faulty connection;
                        // drop this socket
                        error!("{:?} // Failed to connect to the node {:?} {:?} ", self.id, peer_id, err);
                        break;
                    }

                    // TLS handshake; drop connection if it fails
                    let sock = if peer_id >= first_cli || my_id >= first_cli {
                        debug!(
                            "{:?} // Connecting with plain text to node {:?}",
                            my_id, peer_id
                        );

                        SecureSocketSendAsync::new_plain(sock)
                    } else {
                        match connector.connect(hostname, sock).await {
                            Ok(s) => SecureSocketSendAsync::new_tls(s),
                            Err(err) => {
                                error!("{:?} // Failed to connect to the node {:?} {:?} ", self.id, peer_id, err);
                                break;
                            }
                        }
                    };

                    let final_sock = SecureSocketSend::Async(SocketSendAsync::new(sock));

                    // success
                    self.handle_connected_tx(peer_id, final_sock);

                    self.unregister_currently_connecting_to_node(peer_id);

                    if let Some(callback) = callback {
                        callback(true);
                    }

                    return;
                }
                Err(err) => {
                    error!(
                        "{:?} // Error on connecting to {:?} addr {:?}: {:?}",
                        self.id, peer_id, addr, err
                    );
                }
            }

            // sleep for `SECS` seconds and retry
            Delay::new(Duration::from_secs(SECS)).await;
        }

        self.unregister_currently_connecting_to_node(peer_id);

        if let Some(callback) = callback {
            callback(false);
        }

        // announce we have failed to connect to the peer node
        //if we fail to connect, then just ignore
        error!("{:?} // Failed to connect to the node {:?} ", self.id, peer_id);
    }

    ///Accept synchronous connections
    fn rx_side_accept_sync(
        self: Arc<Self>,
        first_cli: NodeId,
        my_id: NodeId,
        listener: SyncListener,
        acceptor: Arc<ServerConfig>,
    ) {
        debug!("{:?} // Awaiting for new connections", my_id);

        loop {
            if let Ok(sock) = listener.accept() {
                let replica_acceptor = acceptor.clone();

                let rx_ref = self.clone();

                let first_cli = first_cli.clone();
                let my_id = my_id.clone();

                threadpool::execute(move || {
                    rx_ref.rx_side_establish_conn_task_sync(
                        first_cli,
                        my_id,
                        replica_acceptor,
                        sock,
                    );
                });
            }
        }
    }

    ///Accept connections from other nodes. Utilizes the async environment
    //#[instrument(skip(self, first_cli, listener, acceptor))]
    async fn rx_side_accept(
        self: Arc<Self>,
        first_cli: NodeId,
        my_id: NodeId,
        listener: AsyncListener,
        acceptor: TlsAcceptor,
    ) {
        debug!("{:?} // Awaiting for new connections (async)", my_id);

        loop {
            match listener.accept().await {
                Ok(sock) => {
                    let rand = fastrand::u32(0..);

                    debug!("{:?} // Accepting connection with rand {}", my_id, rand);

                    let acceptor = acceptor.clone();

                    rt::spawn(
                        self.clone()
                            .rx_side_establish_conn_task(first_cli, my_id, acceptor, sock, rand),
                    );
                }
                Err(err) => {
                    error!("{:?} // Failed to accept connection {:?}", my_id, err);
                }
            }
        }
    }

    /// performs a cryptographic handshake with a peer node;
    /// header doesn't need to be signed, since we won't be
    /// storing this message in the log
    /// So the same as [`rx_side_accept_task()`] but synchronously.
    fn rx_side_establish_conn_task_sync(
        self: Arc<Self>,
        first_cli: NodeId,
        my_id: NodeId,
        acceptor: Arc<ServerConfig>,
        mut sock: SyncSocket,
    ) {
        let mut buf_header = [0; Header::LENGTH];

        // this loop is just a trick;
        // the `break` instructions act as a `goto` statement
        loop {
            // read the peer's header
            if let Err(_) = sock.read_exact(&mut buf_header[..]) {
                // errors reading -> faulty connection;
                // drop this socket
                break;
            }

            // we are passing the correct length, safe to use unwrap()
            let header = Header::deserialize_from(&buf_header[..]).unwrap();

            // extract peer id
            let peer_id = match WireMessage::from_parts(header, Bytes::new()) {
                // drop connections from other clis if we are a cli
                Ok(wm) if wm.header().from() >= first_cli && my_id >= first_cli => break,
                // drop connections to the wrong dest
                Ok(wm) if wm.header().to() != my_id => break,
                // accept all other conns
                Ok(wm) => wm.header().from(),
                // drop connections with invalid headers
                Err(_) => break,
            };

            // TLS handshake; drop connection if it fails
            let sock = if peer_id >= first_cli || my_id >= first_cli {
                SecureSocketRecvSync::new_plain(sock)
            } else {
                if let Ok(tls_session) = ServerConnection::new(acceptor.clone()) {
                    SecureSocketRecvSync::new_tls(tls_session, sock)
                } else {
                    error!(
                        "{:?} // Failed to setup TLS Connection with peer {:?}",
                        my_id, peer_id
                    );

                    return;
                }
            };

            let cpy_peer_id = peer_id.clone();

            debug!(
                "{:?} // Received new connection from id {:?}",
                my_id, peer_id
            );

            //Launch a new thread to handle the inbound connection
            std::thread::Builder::new()
                .name(format!("Reception thread client {:?}", peer_id))
                .spawn(move || {
                    self.handle_connected_rx_sync(peer_id, sock);
                })
                .expect(
                    format!(
                        "Failed to create client connection thread for client {:?}",
                        cpy_peer_id
                    )
                        .as_str(),
                );

            return;
        }
    }

    /// performs a cryptographic handshake with a peer node;
    /// header doesn't need to be signed, since we won't be
    /// storing this message in the log
    //#[instrument(skip(self, first_cli, acceptor, sock, rand))]
    async fn rx_side_establish_conn_task(
        self: Arc<Self>,
        first_cli: NodeId,
        my_id: NodeId,
        acceptor: TlsAcceptor,
        mut sock: AsyncSocket,
        rand: u32,
    ) {
        let mut buf_header = [0; Header::LENGTH];

        debug!(
            "{:?} // Started handling connection from node {}",
            my_id, rand
        );

        // this loop is just a trick;
        // the `break` instructions act as a `goto` statement
        loop {
            // read the peer's header
            if let Err(_) = sock.read_exact(&mut buf_header[..]).await {
                // errors reading -> faulty connection;
                // drop this socket
                break;
            }

            debug!("{:?} // Received header from node {}", my_id, rand);

            // we are passing the correct length, safe to use unwrap()
            let header = Header::deserialize_from(&buf_header[..]).unwrap();

            // extract peer id
            let peer_id = match WireMessage::from_parts(header, Bytes::new()) {
                // drop connections from other clis if we are a cli
                Ok(wm) if wm.header().from() >= first_cli && my_id >= first_cli => break,
                // drop connections to the wrong dest
                Ok(wm) if wm.header().to() != my_id => break,
                // accept all other conns
                Ok(wm) => wm.header().from(),
                // drop connections with invalid headers
                Err(_) => break,
            };

            debug!(
                "{:?} // Received connection from node {:?}, {}",
                my_id, peer_id, rand
            );

            // TLS handshake; drop connection if it fails
            let sock = if peer_id >= first_cli || my_id >= first_cli {
                SecureSocketRecvAsync::new_plain(sock)
            } else {
                match acceptor.accept(sock).await {
                    Ok(s) => SecureSocketRecvAsync::new_tls(s),
                    Err(_) => {
                        error!(
                            "{:?} // Failed to setup tls connection to node {:?}",
                            my_id, peer_id
                        );

                        break;
                    }
                }
            };

            self.handle_connected_rx(peer_id, sock).await;

            return;
        }

        // announce we have failed to connect to the peer node
    }

    /// Handles connections asynchronously using tokio, attempts to connect to the client that connected to us
    /// If we are a replica and the other client is a node
    //#[instrument(skip(self, sock))]
    pub async fn handle_connected_rx(
        self: Arc<Self>,
        peer_id: NodeId,
        mut sock: SecureSocketRecvAsync,
    ) {
        self.clone().tx_connect_node_async(peer_id, None);

        //Init the per client queue and start putting the received messages into it
        debug!("{:?} // Handling connection of peer {:?}", self.id, peer_id);

        let client = self.client_pooling.init_peer_conn(peer_id.clone());

        let mut buf = BytesMut::with_capacity(16384);

        // TODO
        //  - verify signatures???
        //  - exit condition (when the `Replica` or `Client` is dropped)
        loop {
            // reserve space for header
            buf.clear();
            buf.resize(Header::LENGTH, 0);

            // read the peer's header
            // We are reading from a buffered connection so this is fine
            if let Err(err) = sock.read_exact(&mut buf[..Header::LENGTH]).await {
                // errors reading -> faulty connection;
                // drop this socket

                error!("{:?} // Failed to read header from socket, faulty connection {:?}", self.id, err);
                break;
            }

            // we are passing the correct length, safe to use unwrap()
            let header = Header::deserialize_from(&buf[..Header::LENGTH]).unwrap();

            // reserve space for message
            //
            // FIXME: add a max bound on the message payload length;
            // if the length is exceeded, reject connection;
            // the bound can be application defined, i.e.
            // returned by `SharedData`
            buf.clear();
            buf.reserve(header.payload_length());
            buf.resize(header.payload_length(), 0);

            // read the peer's payload
            if let Err(_) = sock.read_exact(&mut buf[..header.payload_length()]).await {
                // errors reading -> faulty connection;
                // drop this socket
                break;
            }

            // deserialize payload
            let message = match serialize::deserialize_message::<&[u8], M>(&buf[..header.payload_length()]) {
                Ok(m) => m,
                Err(err) => {
                    // errors deserializing -> faulty connection;
                    // drop this socket
                    error!("{:?} // Failed to deserialize message {:?}", self.id(), err);
                    break;
                }
            };

            //Also handle ping requests and prevent them from being inserted into the
            //Request handling system.
            match &message {
                NetworkMessageKind::Ping(ping_message) => {
                    //Handle the incoming ping requests
                    self.ping_handler.handle_ping_received(&self, ping_message, peer_id);

                    continue;
                }
                _ => {}
            };

            let msg = NetworkMessage::new(header, message);

            if let Err(inner) = client.push_request(msg) {
                error!(
                    "{:?} // Channel closed, closing tcp connection as well to peer {:?}. {:?}",
                    self.id(),
                    peer_id,
                    inner
                );
                break;
            }

            if let Some(comm_stats) = &self.comm_stats {
                comm_stats.register_rq_received(peer_id);
            }
        }

        // announce we have disconnected
        client.disconnect();
    }

    /// Handles connections synchronously, reading from stream and pushing message into the correct queue
    pub fn handle_connected_rx_sync(
        self: Arc<Self>,
        peer_id: NodeId,
        mut sock: SecureSocketRecvSync,
    ) {
        self.clone().tx_connect_node_sync(peer_id, None);

        let client = self.client_pooling.init_peer_conn(peer_id.clone());

        let mut buf = BytesMut::with_capacity(16384);

        // TODO
        //  - verify signatures???
        //  - exit condition (when the `Replica` or `Client` is dropped)
        loop {
            // reserve space for header
            buf.clear();
            buf.resize(Header::LENGTH, 0);

            // read the peer's header
            if let Err(err) = sock.read_exact(&mut buf[..Header::LENGTH]) {
                // errors reading -> faulty connection;
                // drop this socket
                error!("{:?} // Failed to read header from socket, faulty connection {:?}", self.id, err);
                break;
            }

            // we are passing the correct length, safe to use unwrap()
            let header = Header::deserialize_from(&buf[..Header::LENGTH]).unwrap();

            // reserve space for message
            //
            //FIXME: add a max bound on the message payload length;
            // if the length is exceeded, reject connection;
            // the bound can be application defined, i.e.
            // returned by `SharedData`
            buf.clear();
            buf.reserve(header.payload_length());
            buf.resize(header.payload_length(), 0);

            // read the peer's payload
            if let Err(_) = sock.read_exact(&mut buf[..header.payload_length()]) {
                // errors reading -> faulty connection;
                // drop this socket
                break;
            }

            // deserialize payload
            let message = match serialize::deserialize_message::<&[u8], M>(&buf[..header.payload_length()]) {
                Ok(m) => m,
                Err(err) => {
                    // errors deserializing -> faulty connection;
                    // drop this socket
                    error!("{:?} // Failed to deserialize message {:?}", self.id(), err);
                    break;
                }
            };

            //Just to obtain the request key for logging purposes, in the case this is indeed a
            //Client request
            //Also handle ping requests and prevent them from being inserted into the
            //Request handling system.
            let req_key = match &message {
                NetworkMessageKind::Ping(ping_message) => {
                    //Handle the incoming ping requests
                    self.ping_handler.handle_ping_received(&self, ping_message, peer_id);

                    continue;
                }
                _ => None,
            };

            let msg = NetworkMessage::new(header, message);

            if let Err(inner) = client.push_request(msg) {
                error!(
                    "{:?} // Channel closed, closing tcp connection as well to peer {:?}. {:?}",
                    self.id(),
                    peer_id,
                    inner
                );

                break;
            };

            if let Some(comm_stats) = &self.comm_stats {
                comm_stats.register_rq_received(peer_id);
            }

            if let Some( req_recv) = &self.recv_rqs {
                if let Some(req_key) = req_key {
                    let guard = (req_recv).read().unwrap();

                    let bucket = &guard[req_key as usize % guard.len()];

                    bucket.insert(req_key, ());
                }
            }
        }

        debug!(
            "{:?} // Terminated connection with node {:?}",
            self.id(),
            peer_id
        );

        // announce we have disconnected
        client.disconnect();
    }
}

/// Represents a node with sending capabilities only.
pub struct SendNode<M: Serializable + 'static> {
    id: NodeId,
    first_cli: NodeId,
    shared: Arc<NodeShared>,
    rng: prng::State,
    peer_tx: PeerTx,
    parent_node: Arc<Node<M>>,
    channel: Arc<ConnectedPeer<NetworkMessage<M>>>,
    comm_stats: Option<Arc<CommStats>>,
}

impl<M: Serializable> Clone for SendNode<M> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            first_cli: self.first_cli,
            rng: prng::State::new(),
            shared: Arc::clone(&self.shared),
            peer_tx: self.peer_tx.clone(),
            parent_node: self.parent_node.clone(),
            channel: self.channel.clone(),
            comm_stats: self.comm_stats.clone(),
        }
    }
}

impl<M> SendNode<M>
    where
        M: Serializable
{
    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn loopback_channel(&self) -> &Arc<ConnectedPeer<NetworkMessage<M>>> {
        &self.channel
    }

    pub fn parent_node(&self) -> &Arc<Node<M>> {
        &self.parent_node
    }

    /// Check the `master_channel()` documentation for `Node`.

    /// Check the `send()` documentation for `Node`.
    pub fn send(
        &mut self,
        message: NetworkMessageKind<M>,
        target: NodeId,
        flush: bool,
    ) {
        let start_time = Instant::now();

        match self.parent_node.resolve_client_rx_connection(target) {
            None => {
                error!(
                    "Failed to send message to client {:?} as the connection to it was not found!",
                    target
                );
            }
            Some(conn) => {
                let send_to =
                    match <Node<M>>::send_to(flush, self.id, target, None, conn, &self.peer_tx) {
                        Ok(send_to) => send_to,
                        Err(error) => {
                            error!("{:?} // {:?}", self.id, error);

                            return;
                        }
                    };

                let my_id = self.id;
                let nonce = self.rng.next_state();

                let comm_stats = if let Some(comm_stats) = &self.comm_stats {
                    Some((comm_stats.clone(), start_time))
                } else {
                    None
                };

                <Node<M>>::send_impl(
                    message,
                    send_to,
                    my_id,
                    target,
                    self.first_cli,
                    nonce,
                    comm_stats,
                );
            }
        }
    }

    /// Check the `send_signed()` documentation for `Node`.
    pub fn send_signed(
        &mut self,
        message: NetworkMessageKind<M>,
        target: NodeId,
    ) {
        let comm_stats = received_network_rq!(&self.comm_stats);

        match self.parent_node.resolve_client_rx_connection(target) {
            None => {
                error!(
                    "Failed to send message to client {:?} as the connection to it was not found!",
                    target
                );
            }
            Some(conn) => {
                let send_to = match <Node<M>>::send_to(
                    true,
                    self.id,
                    target,
                    Some(&self.shared),
                    conn,
                    &self.peer_tx,
                ) {
                    Ok(send_to) => send_to,
                    Err(err) => {
                        error!("{:?} // {:?}", self.id, err);

                        return;
                    }
                };

                let my_id = self.id;
                let nonce = self.rng.next_state();

                <Node<M>>::send_impl(
                    message,
                    send_to,
                    my_id,
                    target,
                    self.first_cli,
                    nonce,
                    comm_stats,
                );
            }
        }
    }

    /// Check the `broadcast()` documentation for `Node`.
    pub fn broadcast(
        &mut self,
        message: NetworkMessageKind<M>,
        targets: impl Iterator<Item=NodeId>,
    ) {
        let comm_stats = received_network_rq!(&self.comm_stats);

        let (mine, others) = self
            .parent_node
            .send_tos(self.id, &self.peer_tx, None, targets);

        let nonce = self.rng.next_state();

        <Node<M>>::broadcast_impl(message, mine, others, self.first_cli, nonce, comm_stats);
    }

    /// Check the `broadcast_signed()` documentation for `Node`.
    pub fn broadcast_signed(
        &mut self,
        message: NetworkMessageKind<M>,
        targets: impl Iterator<Item=NodeId>,
    ) {
        let comm_stats = received_network_rq!(&self.comm_stats);

        let (mine, others) =
            self.parent_node
                .send_tos(self.id, &self.peer_tx, Some(&self.shared), targets);

        let nonce = self.rng.next_state();

        <Node<M>>::broadcast_impl(message, mine, others, self.first_cli, nonce, comm_stats);
    }
}

// helper type used when either a `send()` or a `broadcast()`
// is called by a `Node` or `SendNode`.
//
// holds some data that can be shared between threads, relevant
// to a network write operation, or channel write operation,
// depending on whether we're sending a message to a peer node
// or ourselves
pub enum SendTo<M: Serializable> {
    Me {
        // our id
        my_id: NodeId,
        // shared data
        shared: Option<Arc<NodeShared>>,
        // a handle to our client handle
        tx: Arc<ConnectedPeer<NetworkMessage<M>>>,
    },
    Peers {
        // should we flush write calls?
        flush: bool,
        // our id
        my_id: NodeId,
        // the id of the peer
        peer_id: NodeId,
        // shared data
        shared: Option<Arc<NodeShared>>,
        // handle to the registry of connected tx peers
        peer_tx: PeerTx,
        // handle to socket
        sock: ConnectionHandle,
        // a handle to the message channel of the corresponding client
        tx: Arc<ConnectedPeer<NetworkMessage<M>>>,
    },
}

pub enum SerializedSendTo<M: Serializable> {
    Me {
        // our id
        id: NodeId,
        // a handle to our client handle
        tx: Arc<ConnectedPeer<NetworkMessage<M>>>,
    },
    Peers {
        // the id of the peer
        id: NodeId,
        //Our own ID
        our_id: NodeId,
        // handle to the registry of connected tx peers
        peer_tx: PeerTx,
        // handle to socket
        sock: ConnectionHandle,
        // a handle to the message channel of the corresponding client
        tx: Arc<ConnectedPeer<NetworkMessage<M>>>,
    },
}

impl<M> SendTo<M>
    where
M: Serializable
{
    fn socket_type(&self) -> Option<&ConnectionHandle> {
        match self {
            SendTo::Me { .. } => None,
            SendTo::Peers { sock, .. } => Some(sock),
        }
    }

    fn value(
        self,
        m: Either<
            (u64, Digest, Buf),
            (
                NetworkMessageKind<M>,
                u64,
                Digest,
                Buf,
            ),
        >,
        rq_key: Option<u64>,
    ) {
        match self {
            SendTo::Me {
                my_id,
                shared: ref sh,
                tx,
            } => {
                let key = sh.as_ref().map(|ref sh| &sh.my_key);

                if let Right((m, n, d, b)) = m {
                    Self::me(my_id, m, n, d, b, key, tx);
                } else {
                    // optimize code path
                    unreachable!()
                }
            }
            SendTo::Peers {
                flush,
                my_id,
                peer_id,
                shared: ref sh,
                sock,
                peer_tx,
                tx: _,
            } => {
                let key = sh.as_ref().map(|ref sh| &sh.my_key);

                if let Left((n, d, b)) = m {
                    Self::peers(flush, my_id, peer_id, n, d, b, key, &peer_tx, sock, rq_key);
                } else {
                    // optimize code path
                    unreachable!()
                }
            }
        }
    }

    fn me(
        my_id: NodeId,
        m: NetworkMessageKind<M>,
        n: u64,
        d: Digest,
        b: Buf,
        sk: Option<&KeyPair>,
        cli: Arc<ConnectedPeer<NetworkMessage<M>>>,
    ) {
        // create wire msg
        let (h, _) = WireMessage::new(my_id, my_id, b, n, Some(d), sk).into_inner();

        // send
        if let Err(inner) = cli.push_request(NetworkMessage::new(h, m)) {
            error!("{:?} // Failed to push to myself! {:?}", my_id, inner);
        };
    }

    fn peers(
        _flush: bool,
        my_id: NodeId,
        peer_id: NodeId,
        n: u64,
        d: Digest,
        b: Buf,
        sk: Option<&KeyPair>,
        peer_tx: &PeerTx,
        conn_handle: ConnectionHandle,
        rq_key: Option<u64>,
    ) {
        // create wire msg
        let wm = WireMessage::new(my_id, peer_id, b, n, Some(d), sk);

        match conn_handle.send(wm, rq_key) {
            Ok(_) => {}
            Err(err) => {
                error!("{:?} // Failed to send message to node {:?}, {:?}. Disconnecting", my_id, peer_id, err);

                conn_handle.close();
                peer_tx.disconnect_peer(peer_id.into());
            }
        }
    }
}

impl<M> SerializedSendTo<M>
    where
M: Serializable
{
    fn socket_type(&self) -> Option<&ConnectionHandle> {
        match self {
            SerializedSendTo::Me { .. } => None,
            SerializedSendTo::Peers { sock, .. } => Some(sock),
        }
    }

    fn value(
        self,
        h: Header,
        m: SerializedMessage<NetworkMessageKind<M>>,
    ) {
        match self {
            SerializedSendTo::Me { tx, .. } => {
                Self::me(h, m, tx);
            }
            SerializedSendTo::Peers {
                id,
                our_id: _,
                sock,
                peer_tx,
                tx,
            } => {
                Self::peers(id, h, m, &peer_tx, sock, tx);
            }
        }
    }

    fn me(
        h: Header,
        m: SerializedMessage<NetworkMessageKind<M>>,
        cli: Arc<ConnectedPeer<NetworkMessage<M>>>,
    ) {
        let (original, _) = m.into_inner();

        let myself = h.from;

        // send to ourselves
        if let Err(err) = cli.push_request(NetworkMessage::new(h, original)) {
            error!("{:?} // FAILED TO SEND TO MYSELF {:?}", myself, err);
        }
    }

    fn peers(
        peer_id: NodeId,
        h: Header,
        m: SerializedMessage<NetworkMessageKind<M>>,
        peer_tx: &PeerTx,
        conn_handle: ConnectionHandle,
        _cli: Arc<ConnectedPeer<NetworkMessage<M>>>,
    ) {
        // create wire msg
        let (_, raw) = m.into_inner();
        let wm = WireMessage::from_parts(h, Bytes::from(raw)).unwrap();

        match conn_handle.send(wm, None) {
            Ok(_) => {}
            Err(_) => {
                conn_handle.close();

                peer_tx.disconnect_peer(peer_id.into());
            }
        }
    }
}

///TODO: REMOVE THIS AFTER TESTING
#[inline]
fn get_request_key(
    session_id: SeqNo,
    operation_id: SeqNo,
) -> u64 {
    let sess: u64 = session_id.into();
    let opid: u64 = operation_id.into();
    sess | (opid << 32)
}