//! A peer-to-peer networked bus for publishing and subscribing to
//! `Dynamic<T>`s.
#![warn(missing_docs)]

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use std::{io, thread};

use alot::{LotId, Lots};
use gooey::value::{CallbackHandle, Dynamic, DynamicGuard, WeakDynamic};
use intentional::Assert;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio::runtime;
use tokio::sync::mpsc::{self, UnboundedReceiver};

/// A connection to a network bus that enables [publishing](Self::publish) and
/// [subscribing](Self::subscribe_to) to [`Dynamic<T>`s](Dynamic).
#[derive(Clone, Debug)]
pub struct GooBus {
    data: Arc<GooBusData>,
}

impl GooBus {
    /// Returns a new bus connection that identifies itself as `name` and
    /// listens for incoming TCP connections on `bind`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// - tokio has en error initializing
    /// - binding to `bind` results in an error
    /// - spawning a named thread for tokio returns an error
    pub fn new(name: impl Into<String>, bind: Option<impl ToSocketAddrs>) -> io::Result<GooBus> {
        let name = name.into();

        let rt = runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;

        // Begin listening.
        let bus = rt.block_on(async {
            let listener = if let Some(bind) = bind {
                TcpListener::bind(bind).await?
            } else {
                TcpListener::bind("::1:0").await?
            };
            let listening_on = listener.local_addr()?;
            println!("{name} listening on {listening_on}");
            let bus = GooBus {
                data: Arc::new(GooBusData {
                    name,
                    listening_on,
                    tokio: rt.handle().clone(),
                    state: Dynamic::default(),
                }),
            };
            rt.spawn(bus.clone().listen_loop(listener));

            Ok::<_, io::Error>(bus)
        })?;

        // Move the tokio runtime to its own thread to continue processing
        // networking.
        thread::Builder::new()
            .name(String::from("goobus"))
            .spawn(move || {
                rt.block_on(async {
                    loop {
                        tokio::time::sleep(Duration::from_secs(60 * 60 * 24)).await;
                    }
                });
            })?;

        Ok(bus)
    }

    /// Returns the name of this connection.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.data.name
    }

    /// Returns the local address of the listening socket.
    #[must_use]
    pub fn listening_on(&self) -> SocketAddr {
        self.data.listening_on
    }

    /// Invokes `changed` each time the [`GooBusState`] has been updated.
    pub fn on_state_change<F>(&self, changed: F) -> CallbackHandle
    where
        F: for<'a> FnMut(&'a GooBusState) + Send + 'static,
    {
        self.data.state.for_each(changed)
    }

    fn state(&self) -> DynamicGuard<'_, GooBusState> {
        self.data.state.lock()
    }

    async fn listen_loop(self, listener: TcpListener) {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(self.clone().initialize_peer(stream));
        }
    }

    async fn initialize_peer(self, stream: TcpStream) -> anyhow::Result<()> {
        let peer_addr = stream.peer_addr()?;
        let (peer_id, messages_receiver) = {
            let mut state = self.state();
            if state.peers.iter().any(|peer| peer.address == peer_addr) {
                // We already have a connection to this peer.
                return Ok(());
            }
            let (messages_sender, messages_receiver) = mpsc::unbounded_channel();
            let peer_id = state.peers.push(Peer {
                info: None,
                address: peer_addr,
                outbox: messages_sender,
                subscriptions: HashSet::new(),
            });

            (peer_id, messages_receiver)
        };

        let (mut rx, mut tx) = stream.into_split();
        Message::Hello(PeerInfo {
            name: self.data.name.clone(),
            public_addr: self.data.listening_on,
        })
        .write(&mut tx)
        .await?;

        let peer_name = match Message::read(&mut rx).await? {
            Message::Hello(info) => {
                println!("{}: Connected to {}", self.data.name, info.name);
                let mut state = self.state();
                // Announce the new peer to our peers
                for (_, peer) in state.peers.entries().filter(|(id, _)| *id != peer_id) {
                    // TODO if this errors, should we remove the peer?
                    let _result = peer.outbox.send(Message::NewPeer(info.clone()));
                }
                // If this peer has any values we have subscriptions for, we
                // need to subscribe.
                if let Some(subscribing_to) = state.subscribing_to.get(&info.name) {
                    for name in subscribing_to.keys() {
                        let _result = state.peers[peer_id]
                            .outbox
                            .send(Message::Subscribe(Filter::Named(name.clone())));
                    }
                }
                let name = info.name.clone();
                state.peers[peer_id].info = Some(info);
                name
            }
            other => anyhow::bail!("expected Hello, got {other:?}"),
        };

        tokio::spawn(Self::send_messages(messages_receiver, tx));

        self.process_incoming_messages(peer_name, peer_id, rx).await
    }

    async fn process_incoming_messages(
        &self,
        peer_name: String,
        peer_id: LotId,
        mut rx: OwnedReadHalf,
    ) -> anyhow::Result<()> {
        loop {
            let message = Message::read(&mut rx).await?;
            match message {
                Message::Hello(_) => {
                    anyhow::bail!("unexpected Hello after initialization")
                }
                Message::NewPeer(info) => {
                    let state = self.state();
                    if !state
                        .peers
                        .iter()
                        .any(|peer| peer.info.as_ref() == Some(&info))
                    {
                        drop(state);
                        self.connect_to(info.public_addr);
                    }
                }
                Message::Subscribe(filter) => {
                    let mut state = self.state();
                    match &filter {
                        Filter::Any => todo!("send all currently stored values"),
                        Filter::Named(name) => {
                            if let Some(value) = state.values.get_mut(name) {
                                if let Some(data) = value.serialized.upgrade().and_then(|d| d.get())
                                {
                                    value.subscribers.insert(peer_id);
                                    let _result =
                                        state.peers[peer_id].outbox.send(Message::Value {
                                            name: name.clone(),
                                            data: serde_bytes::ByteBuf::from(data),
                                        });
                                } else {
                                    state.values.remove(name);
                                }
                            }
                        }
                    }
                    state.peers[peer_id].subscriptions.insert(filter);
                }
                Message::Value { name, data } => {
                    println!("{}: Received {peer_name}.{name}", self.data.name);
                    let state = self.state();
                    if let Some(value) = state
                        .subscribing_to
                        .get(&peer_name)
                        .and_then(|values| values.get(&name))
                    {
                        value.input.set(data.into_vec());
                    }
                }
                Message::Bye => return Ok(()),
            }
        }
    }

    async fn send_messages(
        mut from: UnboundedReceiver<Message>,
        mut to: OwnedWriteHalf,
    ) -> anyhow::Result<()> {
        while let Some(message) = from.recv().await {
            message.write(&mut to).await?;
        }

        Ok(())
    }

    async fn connect_to_peer(self, connect_to: impl ToSocketAddrs) -> anyhow::Result<()> {
        let Ok(stream) = TcpStream::connect(connect_to).await else {
            return Ok(());
        };

        self.initialize_peer(stream).await
    }

    /// Attempt to connect to `addr` on the bus.
    ///
    /// If a connection is already established with the bus node at `addr`, the
    /// connection will not be established.
    pub fn connect_to(&self, addr: impl ToSocketAddrs + Send + 'static) {
        self.data.tokio.spawn(self.clone().connect_to_peer(addr));
    }

    /// Subscribes to a [`Dynamic<T>`] with a given `name` from a `peer` on the
    /// bus.
    ///
    /// If a peer with that name is already connected, a subscription request is
    /// made immediately. When a new peer connects with the given peer name, a
    /// subscription will be requested.
    ///
    /// The returned `Dynamic` will automatically be reconnected if the peer
    /// disconnects and reconnects.
    #[must_use]
    pub fn subscribe_to<T>(&self, peer: &str, name: &str) -> Dynamic<T>
    where
        T: Default + Debug + DeserializeOwned + Send + PartialEq + 'static,
    {
        let mut state = self.state();

        // If we're already connected, send a subscribe message to the peer.
        if let Some(peer) = state
            .peers
            .iter()
            .find(|p| p.info.as_ref().map_or(false, |info| info.name == peer))
        {
            let _result = peer
                .outbox
                .send(Message::Subscribe(Filter::Named(name.to_string())));
        }

        let subscribing_to = state
            .subscribing_to
            .entry(peer.to_string())
            .or_default()
            .entry(name.to_string())
            .or_insert_with(|| SubscribingTo {
                input: Dynamic::default(),
                deserialized: HashMap::new(),
            });

        subscribing_to
            .deserialized
            .entry(TypeId::of::<T>())
            .or_insert_with(|| {
                let deserialized = Dynamic::<T>::default();

                subscribing_to
                    .input
                    .for_each({
                        let deserialized = deserialized.clone();
                        move |data| {
                            if let Ok(value) = pot::from_slice::<T>(data) {
                                deserialized.set(value);
                            }
                        }
                    })
                    // TODO this handle should probably belong to a wrapper
                    // returned from this function, or we need to make
                    // set_source public.
                    .persist();

                Box::new(deserialized)
            })
            .as_any()
            .downcast_ref::<Dynamic<T>>()
            .assert("type mismatch")
            .clone()
    }

    /// Publishes `value` as `name` on the bus.
    ///
    /// Any bus nodes will be able to subscribe to and observe this value's
    /// changes.
    pub fn publish<T>(&self, name: impl Into<String>, value: &Dynamic<T>)
    where
        T: Serialize + Send + 'static,
    {
        let serialized = value.map_each(|value| pot::to_vec(value).ok());
        let name = name.into();

        serialized
            .for_each({
                let name = name.clone();
                let this = self.clone();
                move |data| {
                    if let Some(data) = data {
                        let state = this.state();
                        let Some(value) = state.values.get(&name) else {
                            return;
                        };

                        let message = Message::Value {
                            name: name.clone(),
                            data: serde_bytes::ByteBuf::from(data.clone()),
                        };

                        for subscriber in &value.subscribers {
                            if let Some(info) = &state.peers[*subscriber].info {
                                println!("{}: Sending {name} to {}", this.data.name, info.name);
                            }
                            let _result = state.peers[*subscriber].outbox.send(message.clone());
                        }
                    }
                }
            })
            .persist();
        // The value.map_each function holds a reference to the Dynamic<T>.
        // Storing a weak reference, we ensure that we can detect and clean up
        // dropped values.
        self.state().values.insert(
            name,
            GooValue {
                serialized: serialized.downgrade(),
                subscribers: HashSet::new(),
            },
        );
    }
}

#[derive(Debug)]
struct GooBusData {
    name: String,
    listening_on: SocketAddr,
    tokio: runtime::Handle,
    state: Dynamic<GooBusState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Message {
    /// Exchanged by both peers as their first message.
    Hello(PeerInfo),
    NewPeer(PeerInfo),
    Subscribe(Filter),
    Value {
        name: String,
        data: serde_bytes::ByteBuf,
    },
    Bye,
}

impl Message {
    pub async fn read<R>(from: &mut R) -> anyhow::Result<Self>
    where
        R: tokio::io::AsyncReadExt + Unpin,
    {
        let mut length = [0; 4];
        from.read_exact(&mut length).await?;
        let length = usize::try_from(u32::from_le_bytes(length))?;
        let mut buffer = vec![0; length];
        from.read_exact(&mut buffer).await?;
        pot::from_slice(&buffer).map_err(anyhow::Error::from)
    }

    pub async fn write<W>(&self, to: &mut W) -> anyhow::Result<()>
    where
        W: tokio::io::AsyncWriteExt + Unpin,
    {
        // Allocate 4 bytes for the length
        let mut bytes = vec![0; 4];
        // Write the actual message
        pot::to_writer(self, &mut bytes)?;

        // Patch in the actual length
        let payload_size = u32::try_from(bytes.len() - 4)?;
        bytes[0..4].copy_from_slice(&payload_size.to_le_bytes());

        // Send the message
        to.write_all(&bytes).await?;

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash, Eq, PartialEq)]
enum Filter {
    Any,
    Named(String),
}

type SubscribingToByName = HashMap<String, SubscribingTo>;

#[derive(Debug)]
struct SubscribingTo {
    input: Dynamic<Vec<u8>>,
    deserialized: HashMap<TypeId, Box<dyn AnyDynamicDeserializer>>,
}

trait AnyDynamicDeserializer: Send + Debug {
    fn as_any(&self) -> &dyn Any;
    fn update(&self, data: &[u8]);
}

impl<T> AnyDynamicDeserializer for Dynamic<T>
where
    T: DeserializeOwned + PartialEq + Send + Debug + 'static,
{
    fn update(&self, data: &[u8]) {
        match pot::from_slice(data) {
            Ok(value) => {
                self.set(value);
            }
            Err(err) => {
                eprintln!("Error deserializing value: {err}");
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The internal state of a [`GooBus`] connection.
#[derive(Debug, Default)]
pub struct GooBusState {
    peers: Lots<Peer>,
    values: HashMap<String, GooValue>,
    subscribing_to: HashMap<String, SubscribingToByName>,
}

impl GooBusState {
    /// Returns an iterator over all of the connected peers.
    pub fn peers(&self) -> impl Iterator<Item = &PeerInfo> {
        self.peers.iter().filter_map(|peer| peer.info.as_ref())
    }
}

#[derive(Debug)]
struct GooValue {
    serialized: WeakDynamic<Option<Vec<u8>>>,
    subscribers: HashSet<LotId>,
}

#[derive(Debug)]
struct Peer {
    info: Option<PeerInfo>,
    address: SocketAddr,
    outbox: mpsc::UnboundedSender<Message>,
    subscriptions: HashSet<Filter>,
}

/// Information about a peer of a [`GooBus`] connection.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct PeerInfo {
    /// The name of the peer.
    pub name: String,
    /// The address the peer accepts connections on.
    pub public_addr: SocketAddr,
}

#[test]
fn value_observing() {
    let a = GooBus::new("a", None::<SocketAddr>).unwrap();
    let a_value = Dynamic::new(1_i32);
    a.publish("value", &a_value);
    let b = GooBus::new("b", None::<SocketAddr>).unwrap();
    a.connect_to(b.data.listening_on);
    let mut b_observing_a = b.subscribe_to::<i32>("a", "value").into_reader();

    println!("Waiting to see a.value on b");
    while b_observing_a.get() != 1 {
        b_observing_a.block_until_updated();
    }

    println!("Updating a, waiting to see it on b");
    a_value.set(2);
    b_observing_a.block_until_updated();
    assert_eq!(b_observing_a.get(), 2);

    let c = GooBus::new("c", None::<SocketAddr>).unwrap();
    c.connect_to(a.data.listening_on);

    let mut c_observing_a = c.subscribe_to::<i32>("a", "value").into_reader();
    println!("Waiting to see a.value on c");
    while c_observing_a.get() != 2 {
        c_observing_a.block_until_updated();
    }
    a_value.set(3);

    println!("Waiting for a.value on b");
    b_observing_a.block_until_updated();
    assert_eq!(b_observing_a.get(), 3);
    println!("Waiting for a.value on c");
    c_observing_a.block_until_updated();
    assert_eq!(c_observing_a.get(), 3);
}
