use anyhow::anyhow;
use chrono::{DateTime, Utc};
use futures::channel::{mpsc, oneshot};
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::net::Ipv6Addr;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::RwLock;

use ya_relay_core::crypto::PublicKey;
use ya_relay_core::NodeId;

use ya_relay_proto::proto::{Forward, Payload, SlotId};
use ya_relay_stack::interface::{add_iface_address, add_iface_route, default_iface, to_mac};
use ya_relay_stack::smoltcp::iface::Route;
use ya_relay_stack::smoltcp::wire::{IpAddress, IpCidr, IpEndpoint};
use ya_relay_stack::socket::{SocketEndpoint, TCP_CONN_TIMEOUT};
use ya_relay_stack::{Channel, EgressEvent, IngressEvent, Network, Protocol, Stack};

use crate::client::ClientConfig;
use crate::client::{ForwardSender, Forwarded};
use crate::registry::NodeEntry;
use crate::session::Session;
use crate::ForwardReceiver;

const TCP_BIND_PORT: u16 = 1;
const IPV6_DEFAULT_CIDR: u8 = 0;

/// Information about virtual node in TCP network built over UDP protocol.
#[derive(Clone)]
pub struct VirtNode {
    pub id: NodeId,
    pub endpoint: IpEndpoint,
    pub session: Arc<Session>,
    pub session_slot: SlotId,
}

/// Client implements TCP protocol over underlying UDP.
/// To use TCP we need to create virtual network, so that TCP stack appears to
/// connect to real IP addresses. This layer translates NodeIds into virtual IPs
/// and handles virtual connections.
#[derive(Clone)]
pub struct TcpLayer {
    net: Network,
    state: Arc<RwLock<TcpLayerState>>,
}

struct TcpLayerState {
    ingress: Channel<Forwarded>,

    nodes: HashMap<Box<[u8]>, VirtNode>,
    ips: HashMap<NodeId, Box<[u8]>>,

    forward_senders: HashMap<NodeId, ForwardSender>,
}

impl VirtNode {
    pub fn try_new(id: &[u8], session: Arc<Session>, session_slot: SlotId) -> anyhow::Result<Self> {
        let id = id.into();
        let ip = IpAddress::from(to_ipv6(&id));
        let endpoint = (ip, TCP_BIND_PORT).into();

        Ok(Self {
            id,
            endpoint,
            session,
            session_slot,
        })
    }
}

impl TcpLayer {
    pub fn new(config: Arc<ClientConfig>, ingress: Channel<Forwarded>) -> TcpLayer {
        let net = default_network(config.node_pub_key.clone());
        TcpLayer {
            net,
            state: Arc::new(RwLock::new(TcpLayerState {
                ingress,
                nodes: Default::default(),
                ips: Default::default(),
                forward_senders: Default::default(),
            })),
        }
    }

    fn net_id(&self) -> String {
        self.net.name.as_ref().clone()
    }

    pub async fn receiver(&self) -> Option<ForwardReceiver> {
        self.state.read().await.ingress.receiver()
    }

    pub async fn spawn(&self, our_id: NodeId) -> anyhow::Result<()> {
        let virt_endpoint: IpEndpoint = (to_ipv6(&our_id), TCP_BIND_PORT).into();

        self.net.spawn_local();
        self.net.bind(Protocol::Tcp, virt_endpoint)?;

        self.spawn_ingress_router().await?;
        self.spawn_egress_router().await?;
        Ok(())
    }

    pub async fn resolve_node(&self, node: NodeId) -> anyhow::Result<VirtNode> {
        let state = self.state.read().await;
        state.resolve_node(node)
    }

    pub async fn add_virt_node(&self, node: NodeEntry) -> anyhow::Result<VirtNode> {
        let node = VirtNode::try_new(&node.id.into_array(), node.session, node.slot)?;
        {
            let mut state = self.state.write().await;
            let ip: Box<[u8]> = node.endpoint.addr.as_bytes().into();

            state.nodes.insert(ip.clone(), node.clone());
            state.ips.insert(node.id, ip);
        }
        Ok(node)
    }

    pub async fn remove_node(&self, node_id: NodeId) -> anyhow::Result<()> {
        let mut state = self.state.write().await;

        if let Some(ip) = state.ips.remove(&node_id) {
            state.nodes.remove(&ip);
        }

        if let Some(mut sender) = state.forward_senders.remove(&node_id) {
            sender.close().await.ok();
        }

        Ok(())
    }

    /// Connects to other Node and returns `Sink` for sending data
    /// and channel that will notify us, when connection will be broken.
    /// Drop `Sink` to close the TCP connection.
    pub async fn connect(
        &self,
        node: NodeEntry,
        paused_forwarding: Arc<RwLock<Option<DateTime<Utc>>>>,
    ) -> anyhow::Result<(ForwardSender, oneshot::Receiver<()>)> {
        log::debug!(
            "[VirtualTcp] Connecting to node [{}] using session {}.",
            node.id,
            node.session.id
        );

        // This will override previous Node settings, if we had them.
        let node = self.add_virt_node(node).await?;
        let connection = self.net.connect(node.endpoint, TCP_CONN_TIMEOUT).await?;

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(1);
        let (disconnect_tx, disconnect_rx) = oneshot::channel();

        let id = self.net_id();
        let myself = self.clone();
        let paused = paused_forwarding.clone();

        // We keep sender only to drop it on disconnection.
        self.register_sender(node.id, tx.clone()).await;

        tokio::task::spawn_local(async move {
            log::trace!("Forwarding messages to {}", node.id);

            while let Some(payload) = myself.get_next_fwd_payload(&mut rx, paused.clone()).await {
                log::trace!("Forwarding message to {}", node.id);
                let _ = myself
                    .net
                    .send(payload, connection)
                    .unwrap_or_else(|e| Box::pin(futures::future::err(e)))
                    .await
                    .map_err(|e| {
                        log::warn!(
                            "[{}] unable to forward via {}: {}",
                            id,
                            node.session.remote,
                            e
                        )
                    });
            }

            myself.net.close_connection(&connection.meta);

            // Cleanup all internal info about Node.
            myself
                .remove_node(node.id)
                .await
                .map_err(|e| log::warn!("TcpLayer - error removing node {}: {}", node.id, e))
                .ok();

            disconnect_tx.send(()).ok();
            rx.close();

            log::debug!(
                "[VirtualTcp]: disconnected from: {}. Stopping forwarding to Node [{}].",
                node.session.remote,
                node.id,
            );
        });

        Ok((tx, disconnect_rx))
    }

    async fn register_sender(&self, node_id: NodeId, sender: ForwardSender) {
        self.state
            .write()
            .await
            .forward_senders
            .insert(node_id, sender);
    }

    pub async fn get_next_fwd_payload<T>(
        &self,
        rx: &mut mpsc::Receiver<T>,
        forward_paused_till: Arc<RwLock<Option<DateTime<Utc>>>>,
    ) -> Option<T> {
        let raw_date = {
            // dont lock the state longer then needed.
            *forward_paused_till.read().await
        };
        if let Some(date) = raw_date {
            if let Ok(duration) = (date - Utc::now()).to_std() {
                log::debug!("Receiver Paused!!! {:?}", duration);
                tokio::time::delay_for(duration).await;
                log::trace!("Receiver Continues...");
            }
            (*forward_paused_till.write().await) = None;
            log::debug!("reset date");
        }
        log::trace!("Waiting for data...");
        rx.next().await
    }

    pub async fn receive(&self, node: NodeEntry, payload: Payload) {
        if self.resolve_node(node.id).await.is_err() {
            log::debug!(
                "[VirtualTcp] Incoming message from new Node [{}]. Adding connection.",
                node.id
            );

            self.add_virt_node(node).await.ok();
        }

        self.net.receive(payload.into_vec());
        self.net.poll();
    }

    pub async fn shutdown(&self) {
        let mut state = self.state.write().await;

        for (_, mut sender) in state.forward_senders.drain() {
            sender.close().await.ok();
        }
    }

    async fn spawn_ingress_router(&self) -> anyhow::Result<()> {
        let ingress_rx = self
            .net
            .ingress_receiver()
            .ok_or_else(|| anyhow::anyhow!("Ingress traffic router already spawned"))?;

        tokio::task::spawn_local(self.clone().ingress_router(ingress_rx));
        Ok(())
    }

    async fn spawn_egress_router(&self) -> anyhow::Result<()> {
        let egress_rx = self
            .net
            .egress_receiver()
            .ok_or_else(|| anyhow::anyhow!("Egress traffic router already spawned"))?;

        tokio::task::spawn_local(self.clone().egress_router(egress_rx));
        Ok(())
    }

    async fn ingress_router(self, ingress_rx: UnboundedReceiver<IngressEvent>) {
        ingress_rx
            .for_each(move |event| {
                let myself = self.clone();
                async move {
                    let (desc, payload) = match event {
                        IngressEvent::InboundConnection { desc } => {
                            log::trace!(
                                "[{}] ingress router: new connection from {:?} to {:?} ",
                                myself.net_id(),
                                desc.remote,
                                desc.local,
                            );
                            return;
                        }
                        IngressEvent::Disconnected { desc } => {
                            log::trace!(
                                "[{}] ingress router: ({}) {:?} disconnected from {:?}",
                                myself.net_id(),
                                desc.protocol,
                                desc.remote,
                                desc.local,
                            );
                            return;
                        }
                        IngressEvent::Packet { desc, payload, .. } => (desc, payload),
                    };

                    if desc.protocol != Protocol::Tcp {
                        log::trace!(
                            "[{}] ingress router: dropping {} payload",
                            myself.net_id(),
                            desc.protocol
                        );
                        return;
                    }

                    let remote_address = match desc.remote {
                        SocketEndpoint::Ip(endpoint) => endpoint.addr,
                        _ => {
                            log::trace!(
                                "[{}] ingress router: remote endpoint {:?} is not supported",
                                myself.net_id(),
                                desc.remote
                            );
                            return;
                        }
                    };

                    match {
                        // nodes are populated via `Client::on_forward` and `Client::forward`
                        let state = myself.state.read().await;
                        state
                            .nodes
                            .get(remote_address.as_bytes())
                            .map(|node| (node.id, state.ingress.tx.clone()))
                    } {
                        Some((node_id, tx)) => {
                            let payload = Forwarded {
                                reliable: true,
                                node_id,
                                payload,
                            };

                            let payload_len = payload.payload.len();

                            if tx.send(payload).is_err() {
                                log::trace!(
                                    "[{}] ingress router: ingress handler closed for node {}",
                                    myself.net_id(),
                                    node_id
                                );
                            } else {
                                log::trace!(
                                    "[{}] ingress router: forwarded {} B",
                                    myself.net_id(),
                                    payload_len
                                );
                            }
                        }
                        _ => log::trace!(
                            "[{}] ingress router: unknown remote address {}",
                            myself.net_id(),
                            remote_address
                        ),
                    };
                }
            })
            .await
    }

    async fn egress_router(self, egress_rx: UnboundedReceiver<EgressEvent>) {
        egress_rx
            .for_each(move |egress| {
                let myself = self.clone();
                async move {
                    let node = match {
                        let state = myself.state.read().await;
                        state.nodes.get(&egress.remote).cloned()
                    } {
                        Some(node) => node,
                        None => {
                            log::trace!(
                                "[{}] egress router: unknown address {:02x?}",
                                myself.net_id(),
                                egress.remote
                            );
                            return;
                        }
                    };

                    let forward = Forward::new(node.session.id, node.session_slot, egress.payload);
                    if let Err(error) = node.session.send(forward).await {
                        log::trace!(
                            "[{}] egress router: forward to {} failed: {}",
                            myself.net_id(),
                            node.id,
                            error
                        );
                    }
                }
            })
            .await
    }
}

impl TcpLayerState {
    fn resolve_node(&self, node: NodeId) -> anyhow::Result<VirtNode> {
        let ip = self
            .ips
            .get(&node)
            .ok_or_else(|| anyhow!("Virtual node for id [{}] not found.", node))?;
        self.nodes
            .get(ip)
            .cloned()
            .ok_or_else(|| anyhow!("Virtual node for ip {:?} not found.", ip))
    }
}

fn to_ipv6(bytes: impl AsRef<[u8]>) -> Ipv6Addr {
    const IPV6_ADDRESS_LEN: usize = 16;

    let bytes = bytes.as_ref();
    let len = IPV6_ADDRESS_LEN.min(bytes.len());
    let mut ipv6_bytes = [0u8; IPV6_ADDRESS_LEN];

    // copy source bytes
    ipv6_bytes[..len].copy_from_slice(&bytes[..len]);
    // no multicast addresses
    ipv6_bytes[0] %= 0xff;
    // no unspecified or localhost addresses
    if ipv6_bytes[0..15] == [0u8; 15] && ipv6_bytes[15] < 0x02 {
        ipv6_bytes[15] = 0x02;
    }

    Ipv6Addr::from(ipv6_bytes)
}

fn default_network(key: PublicKey) -> Network {
    let address = key.address();
    let ipv6_addr = to_ipv6(address);
    let ipv6_cidr = IpCidr::new(IpAddress::from(ipv6_addr), IPV6_DEFAULT_CIDR);
    let mut iface = default_iface(to_mac(&address[..6]));

    let name = format!(
        "{:02x}{:02x}{:02x}{:02x}",
        address[0], address[1], address[2], address[3]
    );

    log::debug!("[{}] Ethernet address: {}", name, iface.ethernet_addr());
    log::debug!("[{}] IP address: {}", name, ipv6_addr);

    add_iface_address(&mut iface, ipv6_cidr);
    add_iface_route(
        &mut iface,
        ipv6_cidr,
        Route::new_ipv6_gateway(ipv6_addr.into()),
    );

    Network::new(name, Stack::with(iface))
}