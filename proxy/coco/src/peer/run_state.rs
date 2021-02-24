//! State machine to manage the current mode of operation during peer lifecycle.

use std::{
    collections::HashSet,
    net::SocketAddr,
    time::{Duration, SystemTime},
};

use serde::Serialize;

use librad::{
    identities::Urn,
    net::{
        self,
        peer::{PeerInfo, ProtocolEvent},
        protocol::{
            broadcast::PutResult,
            event::{downstream, upstream},
            gossip::Payload,
        },
    },
    peer::PeerId,
};

use crate::{
    convert::MaybeFrom,
    peer::{announcement, control},
    request::waiting_room::{self, WaitingRoom},
};

pub mod command;
pub use command::Command;

pub mod config;
pub use config::Config;

pub mod input;
pub use input::Input;

/// Events external subscribers can observe for internal peer operations.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum Event {
    /// Announcement subroutine completed and emitted the enclosed updates.
    Announced(announcement::Updates),
    /// A fetch originated by a gossip message succeeded
    GossipFetched {
        /// Provider of the fetched update.
        provider: PeerInfo<SocketAddr>,
        /// Cooresponding gossip message.
        gossip: Payload,
        /// Result of the storage fetch.
        result: PutResult<Payload>,
    },
    /// An event from the underlying coco network stack.
    /// FIXME(xla): Align variant naming to indicate observed occurrences.
    Protocol(ProtocolEvent),
    /// Sync with a peer completed.
    PeerSynced(PeerId),
    /// Request fullfilled with a successful clone.
    RequestCloned(Urn, PeerId),
    /// Request is being cloned from a peer.
    RequestCloning(Urn, PeerId),
    /// Request for the URN was created and is pending submission to the network.
    RequestCreated(Urn),
    /// Request for the URN was submitted to the network.
    RequestQueried(Urn),
    /// Waiting room interval ticked.
    RequestTick,
    /// The request for [`Urn`] timed out.
    RequestTimedOut(Urn),
    /// The [`Status`] of the peer changed.
    StatusChanged(Status, Status),
}

impl MaybeFrom<&Input> for Event {
    fn maybe_from(input: &Input) -> Option<Self> {
        match input {
            Input::Announce(input::Announce::Succeeded(updates)) => {
                Some(Self::Announced(updates.clone()))
            },
            Input::PeerSync(input::Sync::Succeeded(peer_id)) => Some(Self::PeerSynced(*peer_id)),
            Input::Protocol(protocol_event) => Some(Self::Protocol(protocol_event.clone())),
            Input::Request(input::Request::Cloned(urn, remote_peer)) => {
                Some(Self::RequestCloned(urn.clone(), *remote_peer))
            },
            Input::Request(input::Request::Cloning(urn, remote_peer)) => {
                Some(Self::RequestCloning(urn.clone(), *remote_peer))
            },
            Input::Request(input::Request::Queried(urn)) => Some(Self::RequestQueried(urn.clone())),
            Input::Request(input::Request::Tick) => Some(Self::RequestTick),
            Input::Request(input::Request::TimedOut(urn)) => {
                Some(Self::RequestTimedOut(urn.clone()))
            },
            _ => None,
        }
    }
}

/// The current status of the local peer and its relation to the network.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum Status {
    /// Nothing is setup, not even a socket to listen on.
    Stopped,
    /// Local peer is listening on a socket but has not connected to any peers yet.
    Started,
    /// The local peer lost its connections to all its peers.
    Offline,
    /// Phase where the local peer tries get up-to-date.
    #[serde(rename_all = "camelCase")]
    Syncing {
        failed: HashSet<PeerId>,
        /// Number of completed syncs.
        succeeded: HashSet<PeerId>,
        /// Number of synchronisation underway.
        syncs: HashSet<PeerId>,
    },
    /// The local peer is operational and is able to interact with the peers it has connected to.
    #[serde(rename_all = "camelCase")]
    Online {
        /// Number of connected peers.
        connected: usize,
    },
}

/// State kept for a running local peer.
pub struct RunState {
    /// Confiugration to change how input [`Input`]s are interpreted.
    config: Config,
    /// Tracking remote peers that have an active connection.
    ///
    /// As a peer known by [`PeerId`] can be connected multiple times, e.g. when opening a git
    /// connection to clone and fetch, tracking the connection count per peer is paramount to not
    /// falsely end up in an unconnected state despite the fact the protocol is connected, alive
    /// and kicking. The following scenario led to an offline state when a `HashSet` was used in
    /// the past:
    ///
    /// `Connected(Peer1) -> Connected(Peer1) -> Disconnecting(Peer1)`
    //
    // FIXME(xla): Use a `Option<NonEmpty>` here to express the invariance.
    connected_peers: HashSet<PeerId>,
    /// Current internal status.
    pub status: Status,
    stats: net::protocol::event::downstream::Stats,
    /// Timestamp of last status change.
    status_since: SystemTime,
    /// Current set of requests.
    waiting_room: WaitingRoom<SystemTime, Duration>,
}

impl RunState {
    /// Constructs a new state.
    #[cfg(test)]
    fn construct(
        config: Config,
        connected_peers: HashSet<PeerId>,
        status: Status,
        status_since: SystemTime,
    ) -> Self {
        Self {
            config,
            connected_peers,
            stats: downstream::Stats::default(),
            status,
            status_since,
            waiting_room: WaitingRoom::new(waiting_room::Config::default()),
        }
    }

    /// Creates a new `RunState` initialising it with the provided `config` and `waiting_room`.
    pub fn new(config: Config, waiting_room: WaitingRoom<SystemTime, Duration>) -> Self {
        Self {
            config,
            connected_peers: HashSet::new(),
            stats: downstream::Stats::default(),
            status: Status::Stopped,
            status_since: SystemTime::now(),
            waiting_room,
        }
    }

    /// Applies the `input` and based on the current state, transforms to the new state and in some
    /// cases produes commands which should be executed in the appropriate subroutines.
    pub fn transition(&mut self, input: Input) -> Vec<Command> {
        log::trace!("TRANSITION START: {:?} {:?}", input, self.status);

        let cmds = match input {
            Input::Announce(announce_input) => self.handle_announce(announce_input),
            Input::Control(control_input) => self.handle_control(control_input),
            Input::Protocol(protocol_event) => self.handle_protocol(protocol_event),
            Input::PeerSync(peer_sync_input) => self.handle_peer_sync(&peer_sync_input),
            Input::Request(request_input) => self.handle_request(request_input),
            Input::Stats(stats_input) => self.handle_stats(stats_input),
            Input::Timeout(timeout_input) => self.handle_timeout(timeout_input),
        };

        log::trace!("TRANSITION END: {:?} {:?}", self.status, cmds);

        cmds
    }

    /// Handle [`input::Announce`]s.
    fn handle_announce(&mut self, input: input::Announce) -> Vec<Command> {
        match (&self.status, input) {
            // Announce new updates while the peer is online.
            (
                Status::Online { .. } | Status::Started { .. } | Status::Syncing { .. },
                input::Announce::Tick,
            ) => vec![Command::Announce],
            _ => vec![],
        }
    }

    /// Handle [`input::Control`]s.
    fn handle_control(&mut self, input: input::Control) -> Vec<Command> {
        match input {
            input::Control::CancelRequest(urn, timestamp, sender) => {
                let request = self
                    .waiting_room
                    .canceled(&urn, timestamp)
                    .map(|()| self.waiting_room.remove(&urn));
                vec![
                    Command::Control(command::Control::Respond(control::Response::CancelSearch(
                        sender, request,
                    ))),
                    Command::PersistWaitingRoom(self.waiting_room.clone()),
                ]
            },
            input::Control::CreateRequest(urn, time, sender) => {
                let request = self.waiting_room.request(&urn, time);
                vec![
                    Command::Control(command::Control::Respond(control::Response::StartSearch(
                        sender, request,
                    ))),
                    Command::EmitEvent(Event::RequestCreated(urn)),
                ]
            },
            input::Control::GetRequest(urn, sender) => {
                vec![Command::Control(command::Control::Respond(
                    control::Response::GetSearch(sender, self.waiting_room.get(&urn).cloned()),
                ))]
            },
            input::Control::ListRequests(sender) => vec![Command::Control(
                command::Control::Respond(control::Response::ListSearches(
                    sender,
                    self.waiting_room
                        .iter()
                        .map(|pair| pair.1.clone())
                        .collect::<Vec<_>>(),
                )),
            )],
            input::Control::Status(sender) => vec![Command::Control(command::Control::Respond(
                control::Response::CurrentStatus(sender, self.status.clone()),
            ))],
        }
    }

    /// Handle [`input::Sync`]s.
    fn handle_peer_sync(&mut self, input: &input::Sync) -> Vec<Command> {
        if let Status::Syncing {
            mut failed,
            mut succeeded,
            mut syncs,
        } = &mut self.status
        {
            match input {
                input::Sync::Started(peer_id) => {
                    syncs.insert(*peer_id);
                },
                input::Sync::Failed(peer_id) => {
                    syncs.remove(peer_id);
                    failed.insert(*peer_id);
                },
                input::Sync::Succeeded(peer_id) => {
                    syncs.remove(peer_id);
                    succeeded.insert(*peer_id);
                },
            }

            if failed.len() + succeeded.len() >= self.config.sync.max_peers {
                self.status = Status::Online {
                    connected: self.stats.connected_peers,
                };
            }
        }

        vec![]
    }

    /// Handle [`ProtocolEvent`]s.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn handle_protocol(&mut self, event: ProtocolEvent) -> Vec<Command> {
        match (&self.status, event) {
            (Status::Stopped, ProtocolEvent::Endpoint(upstream::Endpoint::Up { .. })) => {
                self.status = Status::Started;
                self.status_since = SystemTime::now();

                vec![]
            },
            (_, ProtocolEvent::Endpoint(upstream::Endpoint::Down)) => {
                self.status = Status::Stopped;
                self.status_since = SystemTime::now();

                vec![]
            },
            (_, ProtocolEvent::Gossip(gossip)) => {
                let mut cmds = vec![];

                match *gossip {
                    // FIXME(xla): Find out if we care about the result variance.
                    upstream::Gossip::Put {
                        payload: Payload { urn, .. },
                        provider: PeerInfo { peer_id, .. },
                        ..
                    } => {
                        if let Err(waiting_room::Error::TimeOut { .. }) =
                            self.waiting_room.found(&urn, peer_id, SystemTime::now())
                        {
                            cmds.push(Command::Request(command::Request::TimedOut(urn)));
                        }
                    },
                }

                cmds
            },
            _ => vec![],
        }
    }

    /// Handle [`input::Request`]s.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn handle_request(&mut self, input: input::Request) -> Vec<Command> {
        match (&self.status, input) {
            // Check for new query and clone requests.
            (Status::Online { .. } | Status::Syncing { .. }, input::Request::Tick) => {
                let mut cmds = Vec::with_capacity(2);

                if let Some(urn) = self.waiting_room.next_query(SystemTime::now()) {
                    cmds.push(Command::Request(command::Request::Query(urn)));
                    cmds.push(Command::PersistWaitingRoom(self.waiting_room.clone()));
                }
                if let Some((urn, remote_peer)) = self.waiting_room.next_clone() {
                    cmds.push(Command::Request(command::Request::Clone(urn, remote_peer)));
                    cmds.push(Command::PersistWaitingRoom(self.waiting_room.clone()));
                }
                cmds
            },
            // FIXME(xla): Come up with a strategy for the results returned by the waiting room.
            (_, input::Request::Cloning(urn, remote_peer)) => self
                .waiting_room
                .cloning(&urn, remote_peer, SystemTime::now())
                .map_or_else(
                    |error| Self::handle_waiting_room_timeout(urn, &error),
                    |_| vec![Command::PersistWaitingRoom(self.waiting_room.clone())],
                ),
            (_, input::Request::Cloned(urn, remote_peer)) => self
                .waiting_room
                .cloned(&urn, remote_peer, SystemTime::now())
                .map_or_else(
                    |error| Self::handle_waiting_room_timeout(urn, &error),
                    |_| vec![Command::PersistWaitingRoom(self.waiting_room.clone())],
                ),
            (_, input::Request::Queried(urn)) => self
                .waiting_room
                .queried(&urn, SystemTime::now())
                .map_or_else(
                    |error| Self::handle_waiting_room_timeout(urn, &error),
                    |_| vec![Command::PersistWaitingRoom(self.waiting_room.clone())],
                ),
            (
                _,
                input::Request::Failed {
                    remote_peer,
                    reason,
                    urn,
                },
            ) => {
                log::warn!("Cloning failed with: {}", reason);
                self.waiting_room
                    .cloning_failed(&urn, remote_peer, SystemTime::now())
                    .map_or_else(
                        |error| Self::handle_waiting_room_timeout(urn, &error),
                        |_| vec![Command::PersistWaitingRoom(self.waiting_room.clone())],
                    )
            },
            _ => vec![],
        }
    }

    fn handle_stats(&mut self, input: input::Stats) -> Vec<Command> {
        match (&self.status, input) {
            (_, input::Stats::Tick) => vec![Command::Stats],
            (status, input::Stats::Values(connected_peers, stats)) => {
                let mut cmds = vec![];

                match status {
                    Status::Online { .. } | Status::Syncing { .. } | Status::Started
                        if stats.connected_peers == 0 =>
                    {
                        self.status = Status::Offline;
                        self.status_since = SystemTime::now();
                    }
                    // TODO(xla): Also issue sync if we come online after a certain period of
                    // being disconnected from any peer.
                    Status::Offline if stats.connected_peers > 0 => {
                        self.status = Status::Online {
                            connected: stats.connected_peers,
                        };
                    },
                    Status::Started if self.config.sync.on_startup && stats.connected_peers > 0 => {
                        self.status = Status::Syncing {
                            failed: HashSet::new(),
                            succeeded: HashSet::new(),
                            syncs: HashSet::new(),
                        };
                        self.status_since = SystemTime::now();

                        for peer in &connected_peers {
                            cmds.push(Command::SyncPeer(*peer));
                        }
                        cmds.push(Command::StartSyncTimeout(self.config.sync.period));
                    },
                    Status::Started if stats.connected_peers > 0 => {
                        self.status = Status::Online {
                            connected: stats.connected_peers,
                        };
                        self.status_since = SystemTime::now();
                    },
                    Status::Syncing { .. } => {
                        let connected =
                            connected_peers.iter().copied().collect::<HashSet<PeerId>>();
                        let diff = connected.difference(&self.connected_peers);

                        for peer in diff {
                            cmds.push(Command::SyncPeer(*peer));
                        }
                    },
                    _ => {},
                };

                self.connected_peers = connected_peers.into_iter().collect();
                self.stats = stats;

                cmds
            },
        }
    }

    /// Handle [`waiting_room::Error`]s.
    fn handle_waiting_room_timeout(urn: Urn, error: &waiting_room::Error) -> Vec<Command> {
        log::warn!("WaitingRoom::Error : {}", error);
        match error {
            waiting_room::Error::TimeOut { .. } => {
                vec![Command::Request(command::Request::TimedOut(urn))]
            },
            _ => vec![],
        }
    }

    /// Handle [`input::Timeout`]s.
    fn handle_timeout(&mut self, input: input::Timeout) -> Vec<Command> {
        match (&self.status, input) {
            // Go online if we exceed the sync period.
            (Status::Syncing { .. }, input::Timeout::SyncPeriod) => {
                self.status = Status::Online {
                    connected: self.connected_peers.len(),
                };
                self.status_since = SystemTime::now();

                vec![]
            },
            _ => vec![],
        }
    }
}

#[allow(clippy::needless_update, clippy::panic, clippy::unwrap_used)]
#[cfg(test)]
mod test {
    use std::{
        collections::{HashMap, HashSet},
        iter::FromIterator,
        net::{IpAddr, SocketAddr},
        str::FromStr,
        time::{Duration, SystemTime},
    };

    use assert_matches::assert_matches;
    use pretty_assertions::assert_eq;
    use tokio::sync::oneshot;

    use librad::{
        git_ext::Oid,
        identities::Urn,
        keys::SecretKey,
        net::{
            self,
            peer::ProtocolEvent,
            protocol::{event::upstream::Gossip, gossip::Payload},
        },
        peer::PeerId,
    };

    use super::{command, config, input, Command, Config, Input, RunState, Status};

    #[test]
    fn transition_to_started_on_listen() -> Result<(), Box<dyn std::error::Error>> {
        let addr = "127.0.0.1:12345".parse::<SocketAddr>()?;

        let status = Status::Stopped;
        let status_since = SystemTime::now();
        let mut state =
            RunState::construct(Config::default(), HashMap::new(), status, status_since);

        let cmds = state.transition(Input::Protocol(ProtocolEvent::Listening(addr)));
        assert!(cmds.is_empty());
        assert_matches!(state.status, Status::Started { .. });

        Ok(())
    }

    #[test]
    fn transition_to_online_if_sync_is_disabled() {
        let status = Status::Started;
        let status_since = SystemTime::now();
        let mut state = RunState::construct(
            Config {
                sync: config::Sync {
                    on_startup: false,
                    ..config::Sync::default()
                },
                ..Config::default()
            },
            HashMap::new(),
            status,
            status_since,
        );

        let cmds = {
            let key = SecretKey::new();
            let peer_id = PeerId::from(key);
            state.transition(Input::Protocol(ProtocolEvent::Connected(peer_id)))
        };
        assert!(cmds.is_empty());
        assert_matches!(state.status, Status::Online { .. });
    }

    #[test]
    fn transition_to_online_after_sync_max_peers() {
        let status = Status::Syncing {
            synced: config::DEFAULT_SYNC_MAX_PEERS - 1,
            syncs: 1,
        };
        let status_since = SystemTime::now();
        let mut state =
            RunState::construct(Config::default(), HashMap::new(), status, status_since);

        let _cmds = {
            let key = SecretKey::new();
            let peer_id = PeerId::from(key);
            state.transition(Input::PeerSync(input::Sync::Succeeded(peer_id)))
        };
        assert_matches!(state.status, Status::Online { .. });
    }

    #[test]
    fn transition_to_online_after_sync_period() {
        let status = Status::Syncing {
            synced: 0,
            syncs: 3,
        };
        let status_since = SystemTime::now();
        let mut state =
            RunState::construct(Config::default(), HashMap::new(), status, status_since);

        let _cmds = state.transition(Input::Timeout(input::Timeout::SyncPeriod));
        assert_matches!(state.status, Status::Online { .. });
    }

    #[test]
    fn transition_to_offline_when_last_peer_disconnects() {
        let peer_id = PeerId::from(SecretKey::new());
        let status = Status::Online { connected: 0 };
        let status_since = SystemTime::now();
        let mut state = RunState::construct(
            Config::default(),
            HashMap::from_iter(vec![(peer_id, 1)]),
            status,
            status_since,
        );

        let _cmds = state.transition(Input::Protocol(ProtocolEvent::Disconnecting(peer_id)));
        assert_matches!(state.status, Status::Offline);
    }

    #[test]
    fn issue_sync_command_until_max_peers() {
        let max_peers = 13;
        let status = Status::Started;
        let status_since = SystemTime::now();
        let mut state = RunState::construct(
            Config {
                sync: config::Sync {
                    max_peers,
                    on_startup: true,
                    ..config::Sync::default()
                },
                ..Config::default()
            },
            HashMap::new(),
            status,
            status_since,
        );

        for _i in 0..(max_peers - 1) {
            let key = SecretKey::new();
            let peer_id = PeerId::from(key);

            // Expect to sync with the first connected peer.
            let cmds = state.transition(Input::Protocol(ProtocolEvent::Connected(peer_id)));
            assert!(!cmds.is_empty(), "expected command");
            assert_matches!(cmds.first().unwrap(), Command::SyncPeer(sync_id) => {
                assert_eq!(*sync_id, peer_id);
            });
            let _cmds = state.transition(Input::PeerSync(input::Sync::Started(peer_id)));
            assert_matches!(state.status, Status::Syncing{ syncs: syncing_peers, .. } => {
                assert_eq!(syncing_peers, 1);
            });
            let _cmds = state.transition(Input::PeerSync(input::Sync::Succeeded(peer_id)));
        }

        // Issue last sync.
        {
            let key = SecretKey::new();
            let peer_id = PeerId::from(key);
            let cmds = state.transition(Input::Protocol(ProtocolEvent::Connected(peer_id)));

            assert!(!cmds.is_empty(), "expected command");
            assert_matches!(cmds.first().unwrap(), Command::SyncPeer { .. });

            let _cmds = state.transition(Input::PeerSync(input::Sync::Started(peer_id)));
            let _cmds = state.transition(Input::PeerSync(input::Sync::Succeeded(peer_id)));
        };

        // Expect to be online at this point.
        assert_matches!(state.status, Status::Online { .. });

        // No more syncs should be expected after the maximum of peers have connected.
        let cmd = {
            let key = SecretKey::new();
            let peer_id = PeerId::from(key);
            state.transition(Input::Protocol(ProtocolEvent::Connected(peer_id)))
        };
        assert!(cmd.is_empty(), "should not emit any more commands");
    }

    #[test]
    fn issue_sync_timeout_when_transitioning_to_syncing() {
        let sync_period = Duration::from_secs(60 * 10);
        let status = Status::Started;
        let status_since = SystemTime::now();
        let mut state = RunState::construct(
            Config {
                sync: config::Sync {
                    on_startup: true,
                    period: sync_period,
                    ..config::Sync::default()
                },
                ..Config::default()
            },
            HashMap::new(),
            status,
            status_since,
        );

        let cmds = {
            let key = SecretKey::new();
            let peer_id = PeerId::from(key);
            state.transition(Input::Protocol(ProtocolEvent::Connected(peer_id)))
        };
        assert_matches!(cmds.get(1), Some(Command::StartSyncTimeout(period)) => {
            assert_eq!(*period, sync_period);
        });
    }

    #[test]
    fn issue_announce_while_online() {
        let status = Status::Online { connected: 0 };
        let status_since = SystemTime::now();
        let mut state =
            RunState::construct(Config::default(), HashMap::new(), status, status_since);
        let cmds = state.transition(Input::Announce(input::Announce::Tick));

        assert!(!cmds.is_empty(), "expected command");
        assert_matches!(cmds.first().unwrap(), Command::Announce);

        let status = Status::Offline;
        let status_since = SystemTime::now();
        let mut state =
            RunState::construct(Config::default(), HashMap::new(), status, status_since);
        let cmds = state.transition(Input::Announce(input::Announce::Tick));

        assert!(cmds.is_empty(), "expected no command");
    }

    #[test]
    fn issue_query_when_requested_and_online() -> Result<(), Box<dyn std::error::Error + 'static>> {
        let urn: Urn = Urn::new(Oid::from_str("7ab8629dd6da14dcacde7f65b3d58cd291d7e235")?);

        let status = Status::Online { connected: 1 };
        let status_since = SystemTime::now();
        let (response_sender, _) = oneshot::channel();
        let mut state =
            RunState::construct(Config::default(), HashMap::new(), status, status_since);
        state.transition(Input::Control(input::Control::CreateRequest(
            urn.clone(),
            SystemTime::now(),
            response_sender,
        )));

        let cmds = state.transition(Input::Request(input::Request::Tick));
        assert_matches!(
            cmds.first().unwrap(),
            Command::Request(command::Request::Query(have)) => {
                assert_eq!(*have, urn);
            }
        );

        Ok(())
    }

    #[test]
    fn issue_query_when_requested_and_syncing() -> Result<(), Box<dyn std::error::Error + 'static>>
    {
        let urn: Urn = Urn::new(Oid::from_str("7ab8629dd6da14dcacde7f65b3d58cd291d7e235")?);

        let status = Status::Syncing {
            synced: 0,
            syncs: 1,
        };
        let status_since = SystemTime::now();
        let (response_sender, _) = oneshot::channel();
        let mut state =
            RunState::construct(Config::default(), HashMap::new(), status, status_since);
        state.transition(Input::Control(input::Control::CreateRequest(
            urn.clone(),
            SystemTime::now(),
            response_sender,
        )));

        let cmds = state.transition(Input::Request(input::Request::Tick));
        let cmd = cmds.first().unwrap();
        assert_matches!(cmd, Command::Request(command::Request::Query(have)) => {
            assert_eq!(*have, urn);
        });

        Ok(())
    }

    #[test]
    fn issue_clone_when_found() -> Result<(), Box<dyn std::error::Error + 'static>> {
        let urn: Urn = Urn::new(Oid::from_str("7ab8629dd6da14dcacde7f65b3d58cd291d7e235")?);
        let peer_id = PeerId::from(SecretKey::new());

        let status = Status::Online { connected: 0 };
        let status_since = SystemTime::now();
        let (response_sender, _) = oneshot::channel();
        let mut state =
            RunState::construct(Config::default(), HashMap::new(), status, status_since);

        state.transition(Input::Control(input::Control::CreateRequest(
            urn.clone(),
            SystemTime::now(),
            response_sender,
        )));
        assert_matches!(
            state
                .transition(Input::Request(input::Request::Queried(urn.clone())))
                .first(),
            Some(Command::PersistWaitingRoom(_))
        );
        // Gossip(Box<upstream::Gossip<SocketAddr, gossip::Payload>>),
        assert!(state
            .transition(Input::Protocol(ProtocolEvent::Gossip(Box::new(
                Gossip::Put {
                    provider: librad::net::protocol::PeerInfo {
                        advertised_info: net::protocol::PeerAdvertisement::new(),
                        peer_id,
                        seen_addrs: vec![],
                    },
                    payload: Payload {
                        urn: urn.clone(),
                        origin: None,
                        rev: None
                    },
                    result: net::peer::broadcast::PutResult::Applied(),
                }
            ))))
            .is_empty());

        let cmds = state.transition(Input::Request(input::Request::Tick));
        assert_matches!(
            cmds.first().unwrap(),
            Command::Request(command::Request::Clone(remote_urn, remote_peer)) => {
                assert_eq!(remote_urn.clone(), urn);
                assert_eq!(*remote_peer, peer_id);
            }
        );

        Ok(())
    }
}
