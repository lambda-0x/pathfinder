use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use libp2p::autonat;
use libp2p::dcutr;
use libp2p::gossipsub::{self, IdentTopic, MessageAuthenticity, MessageId};
use libp2p::identify;
use libp2p::kad::{record::store::MemoryStore, Kademlia, KademliaConfig, KademliaEvent};
use libp2p::ping;
use libp2p::relay::client as relay_client;
use libp2p::request_response::{self, ProtocolSupport};
use libp2p::swarm::NetworkBehaviour;
use libp2p::{identity, kad};

#[derive(NetworkBehaviour)]
#[behaviour(out_event = "Event", event_process = false)]
pub struct Behaviour {
    relay: relay_client::Behaviour,
    autonat: autonat::Behaviour,
    dcutr: dcutr::Behaviour,
    ping: ping::Behaviour,
    identify: identify::Behaviour,
    pub kademlia: Kademlia<MemoryStore>,
    pub gossipsub: gossipsub::Behaviour,
    pub block_sync: request_response::Behaviour<super::sync::BlockSyncCodec>,
}

pub const KADEMLIA_PROTOCOL_NAME: &[u8] = b"/pathfinder/kad/1.0.0";
// FIXME: clarify what version number should be
// FIXME: we're also missing the starting '/'
const PROTOCOL_VERSION: &str = "starknet/0.9.1";

impl Behaviour {
    pub fn new(identity: &identity::Keypair) -> (Self, relay_client::Transport) {
        const PROVIDER_PUBLICATION_INTERVAL: Duration = Duration::from_secs(600);

        let mut kademlia_config = KademliaConfig::default();
        kademlia_config.set_record_ttl(Some(Duration::from_secs(0)));
        kademlia_config.set_provider_record_ttl(Some(PROVIDER_PUBLICATION_INTERVAL * 3));
        kademlia_config.set_provider_publication_interval(Some(PROVIDER_PUBLICATION_INTERVAL));
        // This makes sure that the DHT we're implementing is incompatible with the "default" IPFS
        // DHT from libp2p.
        kademlia_config
            .set_protocol_names(vec![std::borrow::Cow::Borrowed(KADEMLIA_PROTOCOL_NAME)]);

        let peer_id = identity.public().to_peer_id();

        let kademlia = Kademlia::with_config(peer_id, MemoryStore::new(peer_id), kademlia_config);

        // FIXME: find out how we should derive message id
        let message_id_fn = |message: &gossipsub::Message| {
            let mut s = DefaultHasher::new();
            message.data.hash(&mut s);
            MessageId::from(s.finish().to_string())
        };
        let gossipsub_config = libp2p::gossipsub::ConfigBuilder::default()
            .message_id_fn(message_id_fn)
            .build()
            .expect("valid gossipsub config");

        let gossipsub = gossipsub::Behaviour::new(
            MessageAuthenticity::Signed(identity.clone()),
            gossipsub_config,
        )
        .expect("valid gossipsub params");

        let block_sync = request_response::Behaviour::new(
            super::sync::BlockSyncCodec(),
            std::iter::once((super::sync::BlockSyncProtocol(), ProtocolSupport::Full)),
            Default::default(),
        );

        let (relay_transport, relay) = relay_client::new(peer_id);

        (
            Self {
                relay,
                autonat: autonat::Behaviour::new(peer_id, Default::default()),
                dcutr: dcutr::Behaviour::new(peer_id),
                ping: ping::Behaviour::new(ping::Config::new()),
                identify: identify::Behaviour::new(
                    identify::Config::new(PROTOCOL_VERSION.to_string(), identity.public())
                        .with_agent_version(format!("pathfinder/{}", env!("CARGO_PKG_VERSION"))),
                ),
                kademlia,
                gossipsub,
                block_sync,
            },
            relay_transport,
        )
    }

    pub fn provide_capability(&mut self, capability: &str) -> anyhow::Result<()> {
        let key = string_to_key(capability);
        self.kademlia.start_providing(key)?;
        Ok(())
    }

    pub fn get_capability_providers(&mut self, capability: &str) -> kad::QueryId {
        let key = string_to_key(capability);
        self.kademlia.get_providers(key)
    }

    pub fn subscribe_topic(&mut self, topic: &IdentTopic) -> anyhow::Result<()> {
        self.gossipsub.subscribe(topic)?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum Event {
    Relay(relay_client::Event),
    Autonat(autonat::Event),
    Dcutr(dcutr::Event),
    Ping(ping::Event),
    Identify(Box<identify::Event>),
    Kademlia(KademliaEvent),
    Gossipsub(gossipsub::Event),
    BlockSync(request_response::Event<p2p_proto::sync::Request, p2p_proto::sync::Response>),
}

impl From<relay_client::Event> for Event {
    fn from(event: relay_client::Event) -> Self {
        Event::Relay(event)
    }
}

impl From<autonat::Event> for Event {
    fn from(event: autonat::Event) -> Self {
        Event::Autonat(event)
    }
}

impl From<dcutr::Event> for Event {
    fn from(event: dcutr::Event) -> Self {
        Event::Dcutr(event)
    }
}

impl From<ping::Event> for Event {
    fn from(event: ping::Event) -> Self {
        Event::Ping(event)
    }
}

impl From<identify::Event> for Event {
    fn from(event: identify::Event) -> Self {
        Event::Identify(Box::new(event))
    }
}

impl From<KademliaEvent> for Event {
    fn from(event: KademliaEvent) -> Self {
        Event::Kademlia(event)
    }
}

impl From<gossipsub::Event> for Event {
    fn from(event: gossipsub::Event) -> Self {
        Event::Gossipsub(event)
    }
}

impl From<request_response::Event<p2p_proto::sync::Request, p2p_proto::sync::Response>> for Event {
    fn from(
        event: request_response::Event<p2p_proto::sync::Request, p2p_proto::sync::Response>,
    ) -> Self {
        Event::BlockSync(event)
    }
}

fn string_to_key(input: &str) -> kad::record::Key {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    kad::record::Key::new(&result.as_slice())
}
