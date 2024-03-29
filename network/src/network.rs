use {
  crate::{
    behaviour,
    channel::Channel,
    runloop::{self, Runloop},
    topic::{self, Event, Topic},
    wire::{AddressablePeer, Message},
    Config,
  },
  futures::{Stream, StreamExt},
  libp2p::{
    identity::Keypair,
    noise::NoiseError,
    Multiaddr,
    PeerId,
    TransportError,
  },
  std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    pin::Pin,
    task::{Context, Poll},
  },
  thiserror::Error,
  tracing::{error, info},
};

#[derive(Debug, Error)]
pub enum Error {
  #[error("IO Error: {0}")]
  Io(#[from] std::io::Error),

  #[error("Transport layer security error: {0}")]
  TlsError(#[from] NoiseError),

  #[error("Transport error: {0}")]
  TransportError(#[from] TransportError<std::io::Error>),

  #[error("Topic {0} already joined")]
  TopicAlreadyJoined(String),
}

/// Commands sent by different components to the network layer
/// instructing it to perform some operation on its network-managed
/// threads and runloops. Examples of types invoking those commands
/// are Topics, Behaviour, etc.
#[derive(Debug, Clone)]
pub(crate) enum Command {
  /// Invoked by topics when adding new peer to the active view.
  Connect { addr: Multiaddr, topic: String },

  /// Invoked by topics when removing peers from the active view.
  Disconnect { peer: PeerId, topic: String },

  /// Sends a message to one peer in the active view of
  /// one of the topics.
  SendMessage { peer: PeerId, msg: Message },

  /// Invoked by the runloop when a behaviour-level event
  /// is emitted on the background network thread.
  InjectEvent(behaviour::Event),
}

/// This type is the entrypoint to using the network API.
///
/// It is used to configure general network settings such as
/// the underlying transport protocol, encryption scheme, dns
/// lookup and other non-topic specific configuration values.
///
/// An instance of this type is used to join topics and acquire
/// instances of types for interacting with individul topics.
///
/// On the implementation level, this type acts as a multiplexer
/// for topics, routing incoming packets to their appropriate topic
/// instance.
pub struct Network {
  /// Global network-level configuration
  config: Config,

  /// Local identity of the current local node and all its known addresses.
  this: AddressablePeer,

  /// Network background runloop.
  ///
  /// Stored here so users of this type can block on
  /// the network object.
  runloop: Runloop,

  /// Commands sent to the network module by the runloop and topics.
  commands: Channel<Command>,

  /// All joined topic addressed by their topic name.
  /// Each topic is its own instance of HyParView overlay.
  topics: HashMap<String, Topic>,

  /// Used to track pending and active connections to peers
  /// and refcount them by the number of topics using a connection.
  connections: ConnectionTracker,
}

impl Default for Network {
  fn default() -> Self {
    Self::new(Config::default(), Keypair::generate_ed25519())
      .expect("Failed to instantiate network instance using default config")
  }
}

impl Network {
  /// Instanciates a network object.
  pub fn new(config: Config, keypair: Keypair) -> Result<Self, Error> {
    let peer_id = keypair.public().into();
    let commands = Channel::new();

    Ok(Self {
      topics: HashMap::new(),
      connections: ConnectionTracker::new(),
      runloop: Runloop::new(&config, keypair, commands.sender())?,
      this: AddressablePeer {
        peer_id,
        addresses: HashSet::new(), // none discovered yet
      },
      config,
      commands,
    })
  }

  /// Joins a new topic on this network.
  ///
  /// The config value specifies mainly the topic name and
  /// a list of bootstrap peers. If the bootstrap list is empty
  /// then this node will not dial into any peers but listen on
  /// incoming connections on that topic. It will not receive or
  /// send any values unless at least one other node connects to it.
  pub fn join(&mut self, config: topic::Config) -> Result<Topic, Error> {
    if self.topics.contains_key(&config.name) {
      return Err(Error::TopicAlreadyJoined(config.name));
    }

    let name = config.name.clone();
    self.topics.insert(
      name.clone(),
      Topic::new(
        config,
        self.config.clone(),
        self.this.clone(),
        self.commands.sender(),
      ),
    );
    Ok(self.topics.get(&name).unwrap().clone())
  }

  /// Runs the network event loop.
  ///
  /// This loop must be running all the time to drive the network layer,
  /// this function makes it easy to move the whole network layer to the
  /// background by calling:
  ///
  /// ```rust
  /// tokio::spawn(network.runloop());
  /// ```
  ///
  /// The network object is needed only to join topics, after that all
  /// interactions with the network happen through the [`Topic`] instances
  /// created when calling [`Network::join`].
  pub async fn runloop(mut self) {
    while let Some(()) = self.next().await {}
  }
}

impl Network {
  /// Invoked by topics when they are attempting to establish
  /// a new active connection with some peer who's identity is
  /// not known yet but we know its address.
  ///
  /// If peers are trying to connect to a node that we already
  /// have a connection to (like other topics have it in their
  /// active view), then it increments the refcount on that
  /// connection.
  fn begin_connect(&mut self, addr: Multiaddr, topic: String) {
    if !self.connections.connected(&addr) {
      // need to first establish a physical connection with the peer
      self.connections.add_pending_dial(addr.clone(), topic);
      self.runloop.send_command(runloop::Command::Connect(addr));
    } else {
      // peer already connected, emit an event to the topic that
      // a connection has been established with the peer.
      let peer = self.connections.get_peer(&addr).unwrap_or_else(|| {
        panic!(
          "Bug in connection tracker. It thinks that address {addr:?} is \
           connected but cannot map it to a peer id."
        )
      });

      self // track this connection refcount
        .connections
        .add_connection(&peer.peer_id, topic.clone());

      let topic = self.topics.get_mut(&topic).unwrap_or_else(|| {
        panic!(
          "Bug in topics tracker. Trying to establish connection with peer \
           {peer:?} from a topic that is not joined: {topic}"
        )
      });
      topic.inject_event(Event::PeerConnected(peer));
    }
  }

  /// Invoked by the background network runloop when a connection
  /// is established with a peer and its identity is known.
  fn complete_connect(&mut self, peer: AddressablePeer, dialer: bool) {
    if dialer {
      for topic in self.connections.get_pending_dials(&peer) {
        self
          .connections
          .add_connection(&peer.peer_id, topic.clone());
        self.connections.remove_pending_dial(&peer, &topic);
        let topic = self.topics.get_mut(&topic).unwrap_or_else(|| {
          panic!(
            "Bug in topics tracker. Nonexistant {topic} pending connect \
             {peer:?} from a topic that is not joined: {topic}"
          )
        });
        topic.inject_event(Event::PeerConnected(peer.clone()));
      }
    }
  }

  fn begin_disconnect(&mut self, peer: PeerId, topic: String) {
    let refcount = self
      .connections
      .remove_connection(peer, &topic)
      .unwrap_or_else(|| {
        panic!(
          "Bug in connection tracker. Topic {topic} is trying to disconnect \
           from an untracked peer {peer:?}"
        );
      });

    if refcount == 0 {
      // this was the last topic that disconnected from this peer, close the
      // connection and on successfull close of the link signal that to the
      // topic.
      self.connections.add_pending_disconnect(peer, topic);
      self
        .runloop
        .send_command(runloop::Command::Disconnect(peer))
    } else {
      // other topics are still connected to this peer, in that case
      // the link between peers will not be closed, instead we just
      // signal to the topic that its disconnected and decrement the
      // refcount.

      let topic = self.topics.get_mut(&topic).unwrap_or_else(|| {
        panic!(
          "Bug in topic tracker. Attempting to disconnect peer {peer} from an \
           unknown topic {topic}"
        );
      });

      topic.inject_event(Event::PeerDisconnected(peer, true));
    }
  }

  /// This happens when the last topic on this node requests a connection to a
  /// peer
  fn complete_disconnect(&mut self, peer: PeerId) {
    // emit event for the peer that closed the physical link
    if let Some(pending) = self.connections.take_pending_disconnect(&peer) {
      self
        .topics
        .get_mut(&pending)
        .unwrap_or_else(|| {
          panic!(
            "Bug in connection tracker. Unknown topic {pending} is waiting \
             for peer {peer} to disconnect"
          );
        })
        .inject_event(Event::PeerDisconnected(peer, true));
    }

    // Emit disconnect events for all topics connected to this peer.
    // This happens when the link is lost because the remote peer disconnected
    // for whatever reason including TCP errors and we still have topics that
    // think that they are connected to it.
    if let Some(topics) = self.connections.remove_all_connections(peer) {
      for topic in topics {
        self
          .topics
          .get_mut(&topic)
          .unwrap_or_else(|| {
            panic!(
              "Bug in connection tracker. Unknown topic {topic} thinks that \
               it is connected to peer {peer}"
            );
          })
          .inject_event(Event::PeerDisconnected(peer, true));
      }
    }
  }

  fn append_local_address(&mut self, address: Multiaddr) {
    self.this.addresses.insert(address.clone());

    // update all topics about new local addresses
    for topic in self.topics.values_mut() {
      topic.inject_event(topic::Event::LocalAddressDiscovered(address.clone()));
    }
  }

  fn accept_message(&mut self, from: PeerId, msg: Message) {
    if let Some(topic) = self.topics.get_mut(&msg.topic) {
      // route message to appropriate topic
      topic.inject_event(topic::Event::MessageReceived(from, msg));
    } else {
      // sending messages on unsubscribed topics is a protocol violation
      self.runloop.send_command(runloop::Command::BanPeer(from));
    }
  }

  /// Processes events generated by the background runloop
  fn inject_event(&mut self, event: behaviour::Event) {
    info!("net event: {event:?}");
    match event {
      behaviour::Event::MessageReceived(from, msg) => {
        self.accept_message(from, msg)
      }
      behaviour::Event::LocalAddressDiscovered(addr) => {
        self.append_local_address(addr)
      }
      behaviour::Event::ConnectionEstablished { peer, dialer } => {
        self.complete_connect(peer, dialer)
      }
      behaviour::Event::ConnectionClosed(peer) => {
        self.complete_disconnect(peer);
      }
    }
  }
}

/// Drives the network event loop.
///
/// This should not be used directly, use [`Self::runloop`] instead.
impl Stream for Network {
  type Item = ();

  fn poll_next(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
  ) -> Poll<Option<Self::Item>> {
    if let Poll::Ready(Some(command)) = self.commands.poll_recv(cx) {
      match command {
        Command::Connect { addr, topic } => {
          self.begin_connect(addr, topic);
        }
        Command::Disconnect { peer, topic } => {
          self.begin_disconnect(peer, topic)
        }
        Command::SendMessage { peer, msg } => {
          self
            .runloop
            .send_command(runloop::Command::SendMessage { peer, msg });
        }
        Command::InjectEvent(event) => self.inject_event(event),
      }

      return Poll::Ready(Some(()));
    }
    Poll::Pending
  }
}

struct ConnectionTracker {
  /// maps IP addresses to peer identities.
  addresses: HashMap<Multiaddr, PeerId>,

  /// Keeps track of topics that have requested connections
  /// to a given peer but a connection was not established yet.
  pending_dials: HashMap<Multiaddr, HashSet<String>>,

  /// refcount: how many topics are connected to this peer id
  connections: HashMap<PeerId, HashSet<String>>,

  /// Tracks the topic that was last to disconnect from a peer
  /// and waits for the physical link to be closed.
  pending_disconnects: HashMap<PeerId, String>,
}

impl ConnectionTracker {
  pub fn new() -> Self {
    Self {
      addresses: HashMap::new(),
      connections: HashMap::new(),
      pending_dials: HashMap::new(),
      pending_disconnects: HashMap::new(),
    }
  }

  pub fn connected(&self, addr: &Multiaddr) -> bool {
    if let Some(peer) = self.addresses.get(addr) {
      return self.connections.contains_key(peer);
    }
    false
  }

  fn add_pending_dial(&mut self, addr: Multiaddr, topic: String) {
    match self.pending_dials.entry(addr) {
      Entry::Occupied(mut entry) => {
        entry.get_mut().insert(topic);
      }
      Entry::Vacant(entry) => {
        entry.insert([topic].into_iter().collect());
      }
    }
  }

  /// Called when the last connected topic requests disconnection from
  /// a peer (refcount reached zero). That case will start physically
  /// disconnecting from the peer TCP link.
  fn add_pending_disconnect(&mut self, peer: PeerId, topic: String) {
    self.pending_disconnects.insert(peer, topic);
  }

  fn get_peer(&self, addr: &Multiaddr) -> Option<AddressablePeer> {
    if let Some(peer) = self.addresses.get(addr) {
      return Some(AddressablePeer {
        peer_id: *peer,
        addresses: [addr.clone()].into_iter().collect(),
      });
    }
    None
  }

  /// Registers a connection with a peer for a topic
  fn add_connection(&mut self, peer: &PeerId, topic: String) -> usize {
    match self.connections.entry(*peer) {
      Entry::Occupied(mut o) => {
        o.get_mut().insert(topic);
      }
      Entry::Vacant(v) => {
        v.insert([topic].into_iter().collect());
      }
    };

    self.connections.get(peer).expect("just inserted").len()
  }

  fn remove_connection(
    &mut self,
    peer: PeerId,
    topic: &String,
  ) -> Option<usize> {
    match self.connections.entry(peer) {
      Entry::Occupied(mut o) => {
        o.get_mut().remove(topic);
        Some(o.get().len())
      }
      Entry::Vacant(_) => None,
    };

    self.connections.remove(&peer).map(|t| t.len())
  }

  fn remove_all_connections(&mut self, peer: PeerId) -> Option<Vec<String>> {
    self
      .connections
      .remove(&peer)
      .map(|topics| topics.into_iter().collect())
  }

  fn take_pending_disconnect(&mut self, peer: &PeerId) -> Option<String> {
    self.pending_disconnects.remove(peer)
  }

  fn get_pending_dials(&self, peer: &AddressablePeer) -> Vec<String> {
    let mut output = vec![];
    for addr in &peer.addresses {
      if let Some(pending) = self.pending_dials.get(addr) {
        output.append(&mut pending.iter().cloned().collect());
      }
    }
    output
  }

  fn remove_pending_dial(&mut self, peer: &AddressablePeer, topic: &str) {
    for addr in &peer.addresses {
      if let Some(topics) = self.pending_dials.get_mut(addr) {
        topics.remove(topic);
        if topics.is_empty() {
          self.pending_dials.remove(addr);
        }
      }
    }
  }
}
