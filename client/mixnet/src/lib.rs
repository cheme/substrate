// This file is part of Substrate.

// Copyright (C) 2022 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

#![warn(unused_extern_crates)]

//! Substrate-specific mixnet usage.
//!
//! Topology specific to substrate and utils to link to network.

use mixnet::{Error, MixPeerId, MixPublicKey, Topology};

pub use mixnet::{Config, SinkToWorker, StreamFromWorker};
use sp_application_crypto::key_types;
use sp_keystore::SyncCryptoStore;

use codec::{Compact, Decode, Encode, Input, Output};
use futures::{future, FutureExt, StreamExt};
use futures_timer::Delay;
use log::{debug, error, trace, warn};
use sc_authority_discovery::{ExpIncInterval, NetworkProvider};
use sc_client_api::{BlockchainEvents, FinalityNotification, UsageProvider};
use sc_network::{DhtEvent, MixnetCommand, PeerId as NetworkPeerId};
use sc_utils::mpsc::tracing_unbounded;
use sp_api::ProvideRuntimeApi;
use sp_core::crypto::CryptoTypePublicPair;
pub use sp_finality_grandpa::{AuthorityId, AuthorityList, SetId};
use sp_runtime::traits::{Block as BlockT, Header, NumberFor};
use sp_session::CurrentSessionKeys;
use std::{
	collections::{BTreeMap, BTreeSet, HashMap, HashSet},
	sync::Arc,
	time::Duration,
};

/// Minimal number of node for accepting to add new message.
const LOW_MIXNET_THRESHOLD: usize = 5;

/// Minimal number of paths for sending to recipient.
const LOW_MIXNET_PATHS: usize = 2;

// Buffer size for mixnet command channel.
const COMMAND_BUFFER_SIZE: usize = 25;

/// Number of blocks before seen as synched
/// (do not turn mixnet off every time we are a few block late).
const UNSYNCH_FINALIZED_MARGIN: u32 = 10;

/// Delay in seconds after which if no finalization occurs,
/// we switch back to synching state.
const DELAY_NO_FINALISATION_S: u64 = 60;

/// Maximum number of in-flight DHT lookups at any given point in time.
const MAX_IN_FLIGHT_LOOKUPS: usize = 8;

/// Mixnet running worker.
pub struct MixnetWorker<B: BlockT, C, N> {
	// current node authority_id if validating.
	authority_id: Option<AuthorityId>,
	worker: mixnet::MixnetWorker<AuthorityTopology>,
	// Finality notification stream, for each new final block
	// we look for an authority set update.
	finality_stream: sc_client_api::FinalityNotifications<B>,
	shared_authority_set:
		sc_finality_grandpa::SharedAuthoritySet<<B as BlockT>::Hash, NumberFor<B>>,
	// last set id form shared authority set.
	session: Option<u64>,
	client: Arc<C>,
	state: State,
	// External command.
	command_stream: futures::channel::mpsc::Receiver<MixnetCommand>,
	key_store: Arc<dyn SyncCryptoStore>,
	default_limit_config: Option<usize>,

	// DHT related fields. TODO rename dht_...
	network: Arc<N>,
	dht_event_rx: Box<dyn futures::Stream<Item = Arc<DhtEvent>> + Unpin + Send>,
	publish_interval: ExpIncInterval,
	publish_if_changed_interval: ExpIncInterval,
	// if None don't publish at all (eg not in topology).
	latest_published_payload: Option<Vec<u8>>,
	query_interval: ExpIncInterval, /* TODO exp interval is questionable here, at least need to
	                                 * reset on session id change */
	in_flight_lookups: HashMap<sc_network::KademliaKey, AuthorityId>,
	current_authorities_keys: Vec<(sc_network::KademliaKey, AuthorityId)>,
	pending_lookup_at: Option<usize>,
}

type WorkerChannels = (mixnet::WorkerChannels, futures::channel::mpsc::Receiver<MixnetCommand>);

#[derive(PartialEq, Eq)]
enum State {
	Synching,
	WaitingMorePeers,
	Running,
}

/// Instantiate channels needed to spawn and communicate with the mixnet worker.
pub fn new_channels(
) -> (WorkerChannels, (SinkToWorker, StreamFromWorker, futures::channel::mpsc::Sender<MixnetCommand>))
{
	let (to_worker_sink, to_worker_stream) = tracing_unbounded("mpsc_mixnet_in");
	let (from_worker_sink, from_worker_stream) = tracing_unbounded("mpsc_mixnet_out");
	let (command_sink, command_stream) = futures::channel::mpsc::channel(COMMAND_BUFFER_SIZE);
	(
		((Box::new(from_worker_sink), Box::new(to_worker_stream)), command_stream),
		(Box::new(to_worker_sink), Box::new(from_worker_stream), command_sink),
	)
}

impl<B, C, N> MixnetWorker<B, C, N>
where
	B: BlockT,
	C: UsageProvider<B> + BlockchainEvents<B> + ProvideRuntimeApi<B>,
	C::Api: CurrentSessionKeys<B>,
	N: NetworkProvider,
{
	/// Instantiate worker. Should be call after imonline and
	/// grandpa as it reads their keystore.
	pub fn new(
		inner_channels: WorkerChannels,
		network_identity: &libp2p::core::identity::Keypair,
		client: Arc<C>,
		network: Arc<N>,
		dht_event_rx: Box<dyn futures::Stream<Item = Arc<DhtEvent>> + Unpin + Send>,
		shared_authority_set: sc_finality_grandpa::SharedAuthoritySet<
			<B as BlockT>::Hash,
			NumberFor<B>,
		>,
		key_store: Arc<dyn SyncCryptoStore>,
	) -> Option<Self> {
		let mut local_public_key = None;
		// get the peer id, could be another one than the one define in session: in this
		// case node will restart.
		for key in SyncCryptoStore::sr25519_public_keys(&*key_store, key_types::IM_ONLINE)
			.into_iter()
			.rev()
		{
			if SyncCryptoStore::has_keys(&*key_store, &[(key.0.into(), key_types::IM_ONLINE)]) {
				local_public_key = Some(key);
				break
			} else {
				log::error!(target: "mixnet", "No private key for imonline key");
			}
		}

		let local_public_key: [u8; 32] = if let Some(key) = local_public_key {
			key.0
		} else {
			log::trace!(target: "mixnet", "Generating new ImOnline key.");
			SyncCryptoStore::sr25519_generate_new(&*key_store, key_types::IM_ONLINE, None)
				.ok()?
				.0
		};

		let mut mixnet_config = mixnet::Config::new(local_public_key);

		mixnet_config.num_hops = DEFAULT_NUM_HOPS;

		let finality_stream = client.finality_notification_stream();

		let topology = AuthorityTopology::new(
			mixnet_config.local_id.clone(),
			NetworkPeerId::from_public_key(&network_identity.public()),
			mixnet_config.public_key.clone(),
			key_store.clone(),
		);
		let default_limit_config = mixnet_config.limit_per_window.clone();

		let worker = mixnet::MixnetWorker::new(mixnet_config, topology, inner_channels.0);
		let state = State::Synching;
		let query_interval_secs = 2; // TODO from conf
		let max_query_interval_secs = 5; // TODO from conf
		let publish_interval_secs = 2; // TODO from conf
		let max_publish_interval_secs = 60; // TODO from conf
		let publish_if_changed_interval_msecs = 100; // TODO from conf
		Some(MixnetWorker {
			authority_id: None,
			worker,
			finality_stream,
			shared_authority_set,
			session: None,
			client,
			state,
			command_stream: inner_channels.1,
			key_store,
			default_limit_config,
			network,
			dht_event_rx,
			in_flight_lookups: HashMap::new(),
			latest_published_payload: None,
			publish_interval: ExpIncInterval::new(
				Duration::from_secs(publish_interval_secs),
				Duration::from_secs(max_publish_interval_secs),
			),
			publish_if_changed_interval: ExpIncInterval::new(
				Duration::from_millis(publish_if_changed_interval_msecs),
				Duration::from_secs(max_publish_interval_secs),
			),
			query_interval: ExpIncInterval::new(
				Duration::from_secs(query_interval_secs),
				Duration::from_secs(max_query_interval_secs),
			),
			current_authorities_keys: Default::default(),
			pending_lookup_at: None,
		})
	}

	fn start_new_lookups(&mut self) {
		let nb_authorities = self.current_authorities_keys.len();
		// Ignore all still in-flight lookups. Those that are still in-flight are likely stalled as
		// query interval ticks are far enough apart for all lookups to succeed.
		self.in_flight_lookups.clear();

		while self.in_flight_lookups.len() < std::cmp::min(MAX_IN_FLIGHT_LOOKUPS, nb_authorities) {
			if let Some(at) = self.pending_lookup_at.clone() {
				let (hash, authority_id) = match self.current_authorities_keys.get(at) {
					Some(authority) => authority,
					None => {
						self.pending_lookup_at = None;
						continue
					},
				};
				self.network.get_value(&hash);
				self.in_flight_lookups.insert(hash.clone(), authority_id.clone());
				self.pending_lookup_at = Some(at + 1);
			} else {
				use rand::seq::SliceRandom;
				self.current_authorities_keys.shuffle(&mut rand::thread_rng());
				self.pending_lookup_at = Some(0);
			}
			/*			if let Some(metrics) = &self.metrics {
				metrics.requests.inc();
				metrics
					.requests_pending
					.set(self.pending_lookups.len().try_into().unwrap_or(std::u64::MAX));
			}*/
		}
	}

	/// Handle incoming Dht events.
	async fn handle_dht_event(&mut self, event: Arc<DhtEvent>) {
		match &*event {
			DhtEvent::ValueFound(v) => {
				debug!(target: "mixnet", "Dht value found: {:?}", v);
				for (hash, value) in v.into_iter() {
					if self.in_flight_lookups.remove(hash).is_some() {
						debug!(target: "mixnet", "Dht value found for key: {:?}", hash);
						let topology = &mut self.worker.mixnet_mut().topology;
						topology.import_table(&value[..]);
					} else {
						warn!(target: "mixnet", "Dht unexpected value found for key: {:?}", hash);
					}
				}
			},
			DhtEvent::ValueNotFound(hash) =>
				if self.in_flight_lookups.remove(hash).is_some() {
					debug!(target: "mixnet", "Dht value for hash not found: {:?}", hash);
				} else {
					debug!(target: "mixnet", "Dht value for unexepected hash not found: {:?}", hash);
				},
			DhtEvent::ValuePut(hash) => {
				// Fast forward the exponentially increasing interval to the configured maximum. In
				// case this was the first successful address publishing there is no need for a
				// timely retry.
				self.publish_interval.set_to_max();
				debug!(target: "mixnet", "Successfully put hash '{:?}' on Dht.", hash)
			},
			DhtEvent::ValuePutFailed(hash) => {
				debug!(target: "mixnet", "Failed to put hash '{:?}' on Dht.", hash)
			},
		}
	}

	// TODO need async ??
	async fn publish_topology(&mut self, only_if_changed: bool) {
		debug!(target: "mixnet", "In Publish topology.");
		let topology = &mut self.worker.mixnet_mut().topology;
		if !topology.routing {
			debug!(target: "mixnet", "Skip pub, not routing.");
			return
		}
		let (payload, has_changed) = topology.publish_table();
		if only_if_changed && !has_changed {
			debug!(target: "mixnet", "Skip pub, unchanged.");
			return
		}

		if let Some(authority_id) = self.authority_id.as_ref() {
			let key = hash_authority_id(authority_id.as_ref());
			debug!(target: "mixnet", "Publishing topology at {:?} on Dht.", key);
			self.network.put_value(key, payload.clone());
			self.latest_published_payload = Some(payload.clone());
		}
	}

	pub async fn run(mut self) {
		let info = self.client.usage_info().chain;
		if info.finalized_number == 0u32.into() {
			let authority_set = self.shared_authority_set.current_authority_list();
			let session = self.shared_authority_set.set_id();
			self.handle_new_authority(authority_set, session, info.finalized_number);
		}
		let mut delay_finalized = Delay::new(Duration::from_secs(DELAY_NO_FINALISATION_S));
		let delay_finalized = &mut delay_finalized;
		loop {
			futures::select! {
							event = self.dht_event_rx.next().fuse() => {
								if let Some(event) = event {
									self.handle_dht_event(event).await;
								} else {
									// This point is reached if the network has shut down, at which point there is not
									// much else to do than to shut down the authority discovery as well.
									return;
								}
							},
							only_if_changed = future::select(
								self.publish_interval.next().map(|_| false),
								self.publish_if_changed_interval.next().map(|_| true)
							).map(|e| e.factor_first().0).fuse() => {
								self.publish_topology(only_if_changed).await;
			/*					if let Err(e) = self.publish_topology(only_if_changed).await {
									error!(target: "mixnet", "Failed to publish topology: {}", e);
								}*/
							},
							_ = self.query_interval.next().fuse() => {
								self.start_new_lookups();
			/*					if let Err(e) = self.refill_pending_lookups_queue().await {
									error!(
										target: "mixnet",
										"Failed to request addresses of authorities: {}", e,
									);
								}
			*/
							},
							notif = self.finality_stream.next() => {
								// TODO try accessing last of finality stream (possibly skipping some block)
								if let Some(notif) = notif {
									delay_finalized.reset(Duration::from_secs(DELAY_NO_FINALISATION_S));
									self.handle_new_finalize_block(notif);
								} else {
									// This point is reached if the other component did shutdown.
									debug!(target: "mixnet", "Mixnet, shutdown.");
									return;
								}
							},
							command = self.command_stream.next() => {
								if let Some(command) = command {
									self.handle_command(command);
								} else {
									// This point is reached if the other component did shutdown.
									// Shutdown as well.
									debug!(target: "mixnet", "Mixnet, shutdown.");
									return;
								}
							},
							success = future::poll_fn(|cx| self.worker.poll(cx)).fuse() => {
								if !success {
									debug!(target: "mixnet", "Mixnet, shutdown.");
									return;
								}
							},
							_ = delay_finalized.fuse() => {
								self.state = State::Synching;
								delay_finalized.reset(Duration::from_secs(DELAY_NO_FINALISATION_S));
							},
						}
		}
	}

	/// Can mixnet be use?
	pub fn is_ready(&self) -> bool {
		self.state == State::Running
	}

	fn handle_new_finalize_block(&mut self, notif: FinalityNotification<B>) {
		let info = self.client.usage_info().chain; // these could be part of finality stream info?
		let best_finalized = info.finalized_number;
		let basis = if best_finalized > UNSYNCH_FINALIZED_MARGIN.into() {
			best_finalized - UNSYNCH_FINALIZED_MARGIN.into()
		} else {
			0u32.into()
		};
		if notif.header.number() < &basis {
			debug!(target: "mixnet", "Synching, mixnet suspended {:?}.", (notif.header.number(), &basis));
			self.state = State::Synching;
			return
		} else {
			self.update_state(true);
		}

		let new_session = self.shared_authority_set.set_id();
		if self.session.map(|session| new_session != session).unwrap_or(true) {
			let authority_set = self.shared_authority_set.current_authority_list();
			self.handle_new_authority(authority_set, new_session, *notif.header.number());
		}
	}

	fn handle_command(&mut self, command: MixnetCommand) {
		match command {
			MixnetCommand::TransactionImportResult(surb, result) => {
				debug!(target: "mixnet", "Mixnet, received transaction import result.");
				if let Err(e) = self.worker.mixnet_mut().register_surb(result.encode(), *surb) {
					error!(target: "mixnet", "Could not register surb {:?}", e);
				}
			},
			MixnetCommand::SendTransaction(message, send_options, reply) =>
				if self.is_ready() {
					match self.worker.mixnet_mut().register_message(
						None,
						None,
						message,
						send_options,
					) {
						Ok(()) => {
							let _ = reply.send(Ok(()));
						},
						Err(e) => {
							error!(target: "mixnet", "Could not send transaction in mixnet {:?}", e);
							let _ = reply.send(Err(e));
						},
					}
				} else {
					let _ = reply.send(Err(mixnet::Error::NotReady));
				},
		}
	}

	fn handle_new_authority(&mut self, set: AuthorityList, session: SetId, at: NumberFor<B>) {
		let publish_interval_secs = 2; // TODO from conf
		let max_publish_interval_secs = 60; // TODO from conf
		let publish_if_changed_interval_msecs = 100; // TODO from conf
		let query_interval_secs = 2; // TODO from conf
		let max_query_interval_secs = 5; // TODO from conf
		self.query_interval = ExpIncInterval::new(
			Duration::from_secs(query_interval_secs),
			Duration::from_secs(max_query_interval_secs),
		);
		self.publish_interval = ExpIncInterval::new(
			Duration::from_secs(publish_interval_secs),
			Duration::from_secs(max_publish_interval_secs),
		);
		self.publish_if_changed_interval = ExpIncInterval::new(
			Duration::from_millis(publish_if_changed_interval_msecs),
			Duration::from_secs(max_publish_interval_secs),
		);

		if self.session.as_ref().map(|s| s < &session).unwrap_or(false) {
			error!(target: "mixnet", "Handling outdated authority set.");
			return
		}
		self.session = Some(session);

		self.fetch_new_session_keys(at, session);
		self.update_own_public_key_within_authority_set(&set);
		let mut remove_limit = Vec::new();
		let current_local_id = self.worker.local_id().clone();
		let topology = &mut self.worker.mixnet_mut().topology;
		topology.current_session = session;
		topology.routing_table.session = session;
		topology.routing_table.revision = Compact(1); // TODO load from persistence in case it is first call here (may be ongoing session
											  // of a restarted node).
		debug!(target: "mixnet", "Change authorities {:?}", set);
		let mut old_authority = std::mem::take(&mut topology.authorities);
		old_authority.remove(&topology.local_id);

		topology.routing = false;

		self.current_authorities_keys.clear();
		self.pending_lookup_at = None;
		let mut restart = None;
		for (auth, _) in set.into_iter() {
			self.current_authorities_keys
				.push((hash_authority_id(auth.as_ref()), auth.clone()));
			// TODO handle switch of myonline key (maybe not here).
			// TODO rotate mixnet key (anyway we got change of route table already
			// TODO persist old mixnet for a gracefull switch period.
			use sp_application_crypto::Public;
			let auth_pub_pair = auth.clone().to_public_crypto_pair(); // TODO change key type in map?
			if let Some(key) = topology.sessions.get(&auth_pub_pair) {
				let mut peer_id = [0u8; 32];
				peer_id.copy_from_slice(&key.1[..]);
				if old_authority.remove(&peer_id).is_none() {
					remove_limit.push(peer_id.clone());
				}

				if self.authority_id.as_ref() == Some(&auth) {
					debug!(target: "mixnet", "Insert self {:?}", peer_id);
					topology.authorities.insert(peer_id, auth.clone());
					debug!(target: "mixnet", "In new authority set, routing.");
					topology.routing = true;
					// Use ImOnline for the current session.
					let new_id = (current_local_id != peer_id).then(|| {
						topology.local_id = peer_id.clone();
						peer_id.clone()
					});
					// change session key on new ImOnline id.
					// TODO may do it more often (not doing it keeps
					// mixnet connection open, still need to republish
					// tables which may lead to all disconnected anyway).
					let rollkey = new_id.is_some();
					let new_key = rollkey.then(|| {
						let new_key = mixnet::generate_new_keys();
						topology.routing_table.public_key.0 = new_key.0.clone();
						new_key
					});
					if new_id.is_some() || new_key.is_some() {
						restart = Some((new_id, new_key));
					}
				} else {
					debug!(target: "mixnet", "Insert auth {:?}", peer_id);
					topology.authorities.insert(peer_id, auth);
				}
			} else {
				error!(target: "mixnet", "Missing imonline key for authority {:?}, not adding it to topology.", auth);
			}
		}
		topology.should_connect_to = AuthorityTable::should_connect_to(
			&topology.local_id,
			&topology.authorities,
			usize::MAX,
		);
		for (index, id) in topology.should_connect_to.iter().enumerate() {
			topology.should_connect_pending.insert(id.clone(), (index, true));
		}
		debug!(target: "mixnet", "should connect to {:?}", topology.should_connect_to);
		if let Some((id, key)) = restart {
			debug!(target: "mixnet", "Restarting");
			self.worker.restart(id, key);
		}
		self.update_state(false);
		for peer in remove_limit.into_iter() {
			debug!(target: "mixnet", "Remove limit for {:?}.", peer);
			self.worker.change_peer_limit_window(&peer, None);
		}
		for (peer, _) in old_authority.into_iter() {
			debug!(target: "mixnet", "Restore limit for {:?}.", peer);
			self.worker.change_peer_limit_window(&peer, self.default_limit_config.clone());
		}
	}

	fn fetch_new_session_keys(&mut self, mut at: NumberFor<B>, session: SetId) {
		let mut block_id = sp_runtime::generic::BlockId::number(at);
		// find first block with previous session id
		let runtime_api = self.client.runtime_api();
		if session == 0 {
			at = 0u32.into();
			block_id = sp_runtime::generic::BlockId::number(at);
		} else {
			let mut nb = 0;
			let target = match runtime_api.session_index(&block_id) {
				Ok(at) => at - 1,
				Err(e) => {
					error!(target: "mixnet", "Could not fetch session index {:?}, no peer id fetching.", e);
					return
				},
			};
			loop {
				at -= 1u32.into();
				nb += 1;
				block_id = sp_runtime::generic::BlockId::number(at);
				let session_at = match runtime_api.session_index(&block_id) {
					Ok(at) => at,
					Err(e) => {
						error!(target: "mixnet", "Could not fetch session index {:?}, no peer id fetching.", e);
						return
					},
				};
				if session_at == target {
					break
				} else if session_at < target {
					error!(target: "mixnet", "Could not fetch previous session index, no peer id fetching.");
					return
				}
			}

			if nb > 3 {
				warn!(target: "mixnet", "{:?} query to fetch previous session index.", nb);
			}
		}
		let sessions = match runtime_api.queued_keys(&block_id) {
			Ok(at) => at,
			Err(e) => {
				error!(target: "mixnet", "Could not fetch queued session keys {:?}, no peer id fetching.", e);
				return
			},
		};
		debug!(target: "mixnet", "Fetched session keys {:?}, at {:?}", sessions, block_id);
		self.worker.mixnet_mut().topology.sessions = sessions
			.into_iter()
			.flat_map(|(_, keys)| {
				let mut grandpa = None;
				let mut imonline = None;
				for pair in keys {
					if pair.0 == sp_application_crypto::key_types::GRANDPA {
						grandpa = Some(pair.1);
					} else if pair.0 == sp_application_crypto::key_types::IM_ONLINE {
						imonline = Some(pair.1);
					}
				}
				if let (Some(g), Some(a)) = (grandpa, imonline) {
					Some((g, a))
				} else {
					None
				}
			})
			.collect();
	}

	fn update_own_public_key_within_authority_set(&mut self, set: &AuthorityList) {
		self.authority_id = None;
		let local_pub_keys =
			&SyncCryptoStore::ed25519_public_keys(&*self.key_store, key_types::GRANDPA)
				.into_iter()
				.collect::<HashSet<_>>();

		for authority in set.iter() {
			let auth_id: AuthorityId = authority.0.clone().into();
			if local_pub_keys.contains(&auth_id.clone().into()) {
				debug!("found self in authority set, will route");
				self.authority_id = Some(auth_id);
				return
			}
		}
	}

	fn update_state(&mut self, synched: bool) {
		match &self.state {
			State::Running =>
				if !self.worker.mixnet().topology.has_enough_nodes_to_proxy() {
					self.state = State::WaitingMorePeers;
				},
			State::WaitingMorePeers =>
				if self.worker.mixnet().topology.has_enough_nodes_to_proxy() {
					debug!(target: "mixnet", "Mixnet running.");
					self.state = State::Running;
				},
			State::Synching if synched =>
				if self.worker.mixnet().topology.has_enough_nodes_to_proxy() {
					debug!(target: "mixnet", "Mixnet running.");
					self.state = State::Running;
				} else {
					self.state = State::WaitingMorePeers;
				},
			State::Synching => (),
		}
	}
}

/// Topology for mixnet.
/// This restrict the nodes for routing to authorities with stake.
///
/// Other nodes can join the swarm but will not be routing node.
///
/// When sending a message, the message can only reach nodes
/// that are part of the topology.
pub struct AuthorityTopology {
	// MixPeerId is ImOnline key.
	local_id: MixPeerId,
	network_id: NetworkPeerId,
	key_store: Arc<dyn SyncCryptoStore>,
	// true when we are in authorities set.
	routing: bool,
	// Authorities, including self (if an authority).
	authorities: BTreeMap<MixPeerId, AuthorityId>,
	// The connected nodes (for first hop use `authorities` joined `connected_nodes`).
	// (note it is from handshake and note updated from routing tables).
	connected_nodes: HashSet<MixPeerId>,
	// Current session mapping of Grandpa key to IMonline key.
	// TODO switch to MixPeerId to imonline key.
	sessions: HashMap<CryptoTypePublicPair, CryptoTypePublicPair>,

	current_session: SetId,
	routing_table: AuthorityTable,
	last_signed: (Vec<u8>, SetId, u32),
	authorities_tables: BTreeMap<MixPeerId, AuthorityTable>,
	// all path of a given size.
	// on every change to auth table this is cleared TODO make change synchronously to
	// avoid full calc every time.
	//
	// This assume topology is balanced, so we can just run random selection at each hop.
	// TODO check topology balancing and balance if needed (keeping only peers with right incoming
	// and outgoing?)
	// TODO find something better
	// TODO could replace HashMap by vec and use indices as ptr
	paths: BTreeMap<usize, HashMap<MixPeerId, HashMap<MixPeerId, Vec<MixPeerId>>>>,
	paths_depth: usize,
	// Connection to peer that are more prioritary: attempt connect.
	should_connect_to: Vec<MixPeerId>,
	should_connect_pending: HashMap<MixPeerId, (usize, bool)>, /* TODO audit usage of
	                                                            * should_connect,
	                                                            * TODO for stats.
	                                                            * does not looks very smart
	                                                            * peers not connected to other
	                                                            * peer when they should (may not
	                                                            * be their fault
	                                                            * but if number get big it is
	                                                            * still fishy).
	                                                            * disconnected_in_routing:
	                                                            * HashMap<MixPeerId,
	                                                            * Vec<MixPeerId>>, */
}

/// Current published view of an authority routing table.
///
/// Table is currently signed by ImOnline key.
#[derive(Encode, Decode, Debug, Eq, PartialEq, Clone)]
pub struct AuthorityTable {
	session: SetId,
	// TODO this info should be stored in case of node restart (otherwhise until session change, no
	// consensus with peer routing table will be find and only active for next setid.
	revision: Compact<u32>,
	public_key: EncodableMixPublicKey,
	// TODO could put revision but change will be more costy to converge.
	// TODO or put revision in dht key, then on handshake you also pass revision and then crawl
	// over updates -> I kinda like that.
	// Actually would be easier with a `known_latest_revision` field with all known latest revision
	// so peers can easilly resolve dht key and next dht key (if none found, can still query from
	// 0). -> requires outside a Map MixPeerId to latest received revision, so we query other key.
	// Interesting in the sense we have one key on value, but would need to change a bit the dht
	// logic. -> actually may not be proper idea, one key and subsequent update, just need to
	// ensure we are using the latest version which for signed info is easy: ord (sessionid,
	// revision).
	// TODO could replace MixPeerId by index in list of authorities for compactness.
	connected_to: BTreeSet<MixPeerId>,
	receive_from: BTreeSet<MixPeerId>, /* incoming peer needed to check if published information
	                                    * is fresh. */
}

#[derive(Debug, Eq, PartialEq, Clone)]
struct EncodableMixPublicKey(MixPublicKey);

impl Encode for EncodableMixPublicKey {
	fn encode_to<O: Output + ?Sized>(&self, output: &mut O) {
		self.0.as_bytes().encode_to(output)
	}
}

impl Decode for EncodableMixPublicKey {
	fn decode<I: Input>(input: &mut I) -> Result<Self, codec::Error> {
		let mix_public_key: [u8; 32] = Decode::decode(input)?;
		Ok(EncodableMixPublicKey(mix_public_key.into()))
	}
}

// TODO more but this is ok for testing on a 6 authority network.
// TODO put in config.
const NUMBER_CONNECTED_FORWARD: usize = 3;

const DEFAULT_NUM_HOPS: u32 = 4;

// should statically be NUMBER_CONNECTED_BACKWARD, but applying a small variation margin.
const NUMBER_CONNECTED_BACKWARD: usize = NUMBER_CONNECTED_FORWARD - 2;

impl AuthorityTable {
	fn should_connect_to<D>(
		from: &MixPeerId,
		authorities: &BTreeMap<MixPeerId, D>,
		nb: usize,
	) -> Vec<MixPeerId> {
		// TODO cache common seed when all_auth got init
		let mut common_seed = [0u8; 32];
		for auth in authorities.iter() {
			let hash = sp_core::hashing::blake2_256(auth.0);
			for i in 0..32 {
				common_seed[i] ^= hash[i];
			}
		}
		let mut hash = sp_core::hashing::blake2_256(from);
		for i in 0..32 {
			hash[i] ^= common_seed[i];
		}

		let mut auths: Vec<_> = authorities
			.iter()
			.filter_map(|a| if a.0 != from { Some(a.0) } else { None })
			.collect();
		let mut nb_auth = auths.len();
		let mut result = Vec::with_capacity(std::cmp::min(nb, nb_auth));
		let mut cursor = 0;
		while result.len() < nb && nb_auth > 0 {
			// TODO bit arith
			let mut nb_bytes = match nb_auth {
				nb_auth if nb_auth <= u8::MAX as usize => 1,
				nb_auth if nb_auth <= u16::MAX as usize => 2,
				nb_auth if nb_auth < 1usize << 24 => 3,
				nb_auth if nb_auth < u32::MAX as usize => 4,
				_ => unimplemented!(),
			};
			let mut at = 0usize;
			loop {
				if let Some(next) = hash.get(cursor) {
					nb_bytes -= 1;
					at += (*next as usize) * (1usize << (8 * nb_bytes));
					cursor += 1;
					if nb_bytes == 0 {
						break
					}
				} else {
					cursor = 0;
					hash = sp_core::hashing::blake2_256(&hash);
				}
			}
			at = at % nb_auth;
			result.push(auths.remove(at).clone());
			nb_auth = auths.len();
		}
		result
	}
}

#[derive(Clone)]
pub struct AuthorityInfo {
	pub grandpa_id: AuthorityId,
	pub authority_discovery_id: CryptoTypePublicPair,
}

impl AuthorityTopology {
	/// Instantiate a new topology.
	pub fn new(
		local_id: MixPeerId,
		network_id: NetworkPeerId,
		node_public_key: MixPublicKey,
		key_store: Arc<dyn SyncCryptoStore>,
	) -> Self {
		let routing_table = AuthorityTable {
			public_key: EncodableMixPublicKey(node_public_key),
			connected_to: BTreeSet::new(),
			receive_from: BTreeSet::new(),
			revision: Compact(0), /* TODO load from persistent storage on handle_new authority ->
			                       * here on */
			// restart we publish older revision!! and cannot synch.
			session: 0,
		};
		AuthorityTopology {
			local_id,
			network_id,
			authorities: BTreeMap::new(),
			connected_nodes: HashSet::new(),
			sessions: HashMap::new(),
			routing: false,
			key_store,
			authorities_tables: BTreeMap::new(),
			routing_table,
			current_session: 0,
			paths: Default::default(),
			paths_depth: 0,
			should_connect_to: Default::default(),
			should_connect_pending: Default::default(),
			last_signed: Default::default(),
			//disconnected_in_routing: Default::default(),
		}
	}

	fn has_enough_nodes_to_send(&self) -> bool {
		self.authorities_tables
			.iter()
			.filter(|(_, table)| table.connected_to.len() >= NUMBER_CONNECTED_FORWARD)
			.count() >= LOW_MIXNET_THRESHOLD
	}

	fn has_enough_nodes_to_proxy(&self) -> bool {
		self.authorities.len() >= LOW_MIXNET_THRESHOLD
	}

	fn add_connected_peer(&mut self, peer_id: MixPeerId, key: MixPublicKey) {
		debug!(target: "mixnet", "Connected to mixnet {:?} {:?}", peer_id, key);
		self.connected_nodes.insert(peer_id);
		if let Some(info) = self.should_connect_pending.get_mut(&peer_id) {
			if info.1 == true {
				info.1 = false;
				self.refresh_connection_table();
			}
		}
	}

	fn add_disconnected_peer(&mut self, peer_id: &MixPeerId) {
		debug!(target: "mixnet", "Disconnected from mixnet {:?}", peer_id);
		let _ = self.connected_nodes.remove(peer_id);
		if let Some(info) = self.should_connect_pending.get_mut(peer_id) {
			if info.1 == false {
				info.1 = true;
				self.refresh_connection_table();
			}
		}
	}

	// TODO move to routing_table??
	fn refresh_connection_table(&mut self) {
		let past = self.routing_table.clone();
		self.routing_table.connected_to.clear(); // TODO update a bit more precise
		for peer in self.should_connect_to.iter() {
			if let Some(info) = self.should_connect_pending.get(peer) {
				if info.1 == false {
					self.routing_table.connected_to.insert(peer.clone());
				}
			}
			if self.routing_table.connected_to.len() == NUMBER_CONNECTED_FORWARD {
				break
			}
		}
		self.routing_table.receive_from.clear(); // TODO update a bit more precise
		for (peer_id, table) in self.authorities_tables.iter() {
			if table.connected_to.contains(&self.local_id) {
				self.routing_table.receive_from.insert(peer_id.clone());
			}
		}

		if past != self.routing_table {
			self.routing_table.revision.0 += 1;
			self.paths.clear();
			self.paths_depth = 0;
		}
	}
}

impl AuthorityTopology {
	fn random_dest(&self, skip: impl Fn(&MixPeerId) -> bool) -> Option<MixPeerId> {
		use rand::RngCore;
		// Warning this assume that NetworkPeerId is a randomly distributed value.
		let mut ix = [0u8; 32];
		rand::thread_rng().fill_bytes(&mut ix[..]);

		trace!(target: "mixnet", "routing {:?}, ix {:?}", self.authorities_tables, ix);
		for (key, table) in self.authorities_tables.range(ix..) {
			// TODO alert on low receive_from
			if !skip(&key) && !(table.receive_from.len() < NUMBER_CONNECTED_BACKWARD) {
				debug!(target: "mixnet", "Random route node");
				return Some(key.clone())
			} else {
				debug!(target: "mixnet", "Skip {:?}, nb {:?}, {:?}", skip(&key), table.receive_from.len(), NUMBER_CONNECTED_BACKWARD);
			}
		}
		for (key, table) in self.authorities_tables.range(..ix).rev() {
			if !skip(&key) && !(table.receive_from.len() < NUMBER_CONNECTED_BACKWARD) {
				debug!(target: "mixnet", "Random route node");
				return Some(key.clone())
			} else {
				debug!(target: "mixnet", "Skip {:?}, nb {:?}, {:?}", skip(&key), table.receive_from.len(), NUMBER_CONNECTED_BACKWARD);
			}
		}
		None
	}
}

fn random_path(
	paths: &BTreeMap<usize, HashMap<MixPeerId, HashMap<MixPeerId, Vec<MixPeerId>>>>,
	from: &MixPeerId,
	to: &MixPeerId,
	size_path: usize,
) -> Option<Vec<MixPeerId>> {
	trace!(target: "mixnet", "routing from {:?}, to {:?}, path size {:?}", from, to, size_path);
	// TODO some minimal length??
	if size_path < 3 {
		return None
	}
	let mut rng = rand::thread_rng();
	let mut at = size_path;
	let mut exclude = HashSet::new();
	exclude.insert(from.clone());
	exclude.insert(to.clone());
	let mut result = Vec::<MixPeerId>::with_capacity(size_path); // allocate two extra for case where a node is
															 // appended front or/and back.
	result.push(from.clone());
	// TODO consider Vec instead of hashset (small nb elt)
	let mut touched = Vec::<HashSet<MixPeerId>>::with_capacity(size_path - 2);
	touched.push(HashSet::new());
	while at > 2 {
		if let Some(paths) = result.last().and_then(|from| {
			paths.get(&at).and_then(|paths| paths.get(to)).and_then(|paths| paths.get(from))
		}) {
			if let Some(next) = random_path_inner(&mut rng, paths, |p| {
				exclude.contains(p) ||
					touched.last().map(|touched| touched.contains(p)).unwrap_or(false)
			}) {
				result.push(next);
				touched.last_mut().map(|touched| {
					touched.insert(next);
				});
				touched.push(HashSet::new());
				exclude.insert(next);
				at -= 1;
				continue
			}
		}
		// dead end path
		if result.len() == 1 {
			return None
		}
		if let Some(child) = result.pop() {
			exclude.remove(&child);
			touched.pop();
			at += 1;
		}
	}
	//result.push(to.clone());
	result.remove(0); // TODO rewrite to avoid it.
	Some(result)
}

fn count_paths(
	paths: &BTreeMap<usize, HashMap<MixPeerId, HashMap<MixPeerId, Vec<MixPeerId>>>>,
	from: &MixPeerId,
	to: &MixPeerId,
	size_path: usize,
) -> usize {
	let mut total = 0;
	let mut at = size_path;
	let mut exclude = HashSet::new();
	exclude.insert(from.clone());
	exclude.insert(to.clone());
	let mut result = Vec::<(MixPeerId, usize)>::with_capacity(size_path); // allocate two extra for case where a node is
																	  // appended front or/and back.
	result.push((from.clone(), 0));
	let mut touched = Vec::<HashSet<MixPeerId>>::with_capacity(size_path - 2);
	touched.push(HashSet::new());
	loop {
		if let Some((paths, at_ix)) = result.last().and_then(|(from, at_ix)| {
			paths
				.get(&at)
				.and_then(|paths| paths.get(to))
				.and_then(|paths| paths.get(from))
				.map(|p| (p, *at_ix))
		}) {
			if let Some(next) = paths.get(at_ix).cloned() {
				result.last_mut().map(|(_, at_ix)| {
					*at_ix += 1;
				});
				if !exclude.contains(&next) &&
					!touched.last().map(|touched| touched.contains(&next)).unwrap_or(false)
				{
					if at == 3 {
						total += 1;
					} else {
						result.push((next, 0));
						touched.last_mut().map(|touched| {
							touched.insert(next);
						});
						touched.push(HashSet::new());
						exclude.insert(next);
						at -= 1;
					}
					continue
				} else {
					continue
				}
			}
		}
		if result.len() == 1 {
			break
		}
		if let Some((child, _)) = result.pop() {
			exclude.remove(&child);
			touched.pop();
			at += 1;
		}
	}
	total
}

fn random_path_inner(
	rng: &mut rand::rngs::ThreadRng,
	routes: &Vec<MixPeerId>,
	skip: impl Fn(&MixPeerId) -> bool,
) -> Option<MixPeerId> {
	use rand::Rng;
	// Warning this assume that PeerId is a randomly distributed value.
	let ix: usize = match routes.len() {
		l if l <= u8::MAX as usize => rng.gen::<u8>() as usize,
		l if l <= u16::MAX as usize => rng.gen::<u16>() as usize,
		l if l <= u32::MAX as usize => rng.gen::<u32>() as usize,
		_ => rng.gen::<usize>(),
	};
	let ix = ix % routes.len();

	for key in routes[ix..].iter() {
		if !skip(&key) {
			debug!(target: "mixnet", "Random route node");
			return Some(key.clone())
		}
	}
	for key in routes[..ix].iter() {
		if !skip(key) {
			debug!(target: "mixnet", "Random route node");
			return Some(key.clone())
		}
	}
	None
}

impl Topology for AuthorityTopology {
	fn first_hop_nodes_external(
		&self,
		from: &MixPeerId,
		to: &MixPeerId,
	) -> Vec<(MixPeerId, MixPublicKey)> {
		// allow for all
		self.authorities
			.iter()
			.filter(|(id, _key)| from != *id)
			.filter(|(id, _key)| to != *id)
			.filter(|(id, _key)| &self.local_id != *id)
			.filter(|(id, _key)| self.connected_nodes.contains(*id))
			.filter_map(|(k, _v)| {
				self.authorities_tables
					.get(k)
					.map(|table| (k.clone(), table.public_key.0.clone()))
			})
			.collect()
	}

	fn is_first_node(&self, id: &MixPeerId) -> bool {
		self.is_routing(id)
	}

	// TODO make random_recipient return path directly.
	fn random_recipient(
		&mut self,
		from: &MixPeerId,
		send_options: &mixnet::SendOptions,
	) -> Option<(MixPeerId, MixPublicKey)> {
		if !self.has_enough_nodes_to_send() {
			debug!(target: "mixnet", "Not enough routing nodes for path.");
			return None
		}
		let mut bad = HashSet::new();
		loop {
			debug!(target: "mixnet", "rdest {:?}", (&from, &bad));
			if let Some(peer) = self.random_dest(|p| p == from || bad.contains(p)) {
				debug!(target: "mixnet", "Trying random dest {:?}.", peer);
				let nb_hop = send_options.num_hop.unwrap_or(DEFAULT_NUM_HOPS as usize);
				AuthorityTopology::fill_paths(
					&self.local_id,
					&self.routing_table,
					&mut self.paths,
					&mut self.paths_depth,
					&self.authorities_tables,
					nb_hop,
				);
				let from_count = if !self.is_routing(from) {
					debug!(target: "mixnet", "external {:?}", from);
					// TODO should return path directly rather than random here that could be
					// different that path one -> then the check on count could be part of path
					// building
					let firsts = self.first_hop_nodes_external(from, &peer);
					if firsts.len() == 0 {
						return None
					}
					firsts[0].0.clone()
				} else {
					from.clone()
				};
				// TODO also count surbs??
				let nb_path = count_paths(&self.paths, &from_count, &peer, nb_hop);
				debug!(target: "mixnet", "Number path for dest {:?}.", nb_path);
				debug!(target: "mixnet", "{:?} to {:?}", from_count, peer);
				if nb_path >= LOW_MIXNET_PATHS {
					return self.authorities_tables.get(&peer).map(|keys| (peer, keys.public_key.0))
				} else {
					bad.insert(peer);
				}
			} else {
				debug!(target: "mixnet", "No random dest.");
				return None
			}
		}
	}

	/// For a given peer return a list of peers it is supposed to be connected to.
	/// Return `None` if peer is unknown to the topology.
	fn neighbors(&self, _from: &MixPeerId) -> Option<Vec<(MixPeerId, MixPublicKey)>> {
		// unused, random_paths directly implement.
		// TODO remove from mixnet crate??
		unimplemented!()
	}

	fn routing_to(&self, from: &MixPeerId, to: &MixPeerId) -> bool {
		(self.authorities.contains_key(from) || (&self.local_id == from && self.routing)) &&
			(self.authorities.contains_key(to) || (&self.local_id == to && self.routing))
	}

	fn random_path(
		&mut self,
		start_node: (&MixPeerId, Option<&MixPublicKey>),
		recipient_node: (&MixPeerId, Option<&MixPublicKey>),
		nb_chunk: usize,
		num_hops: usize,
		max_hops: usize,
		last_query_if_surb: Option<&Vec<(MixPeerId, MixPublicKey)>>,
	) -> Result<Vec<Vec<(MixPeerId, MixPublicKey)>>, Error> {
		// Diverging from default implementation (random from all possible paths), as `neighbor`
		// return same result for all routing peer building all possible path is not usefull.
		let mut add_start = None;
		let mut add_end = None;
		let start = if self.is_first_node(start_node.0) {
			start_node.0.clone()
		} else {
			trace!(target: "mixnet", "External node");
			if num_hops + 1 > max_hops {
				return Err(Error::TooManyHops)
			}

			let firsts = self.first_hop_nodes_external(start_node.0, recipient_node.0);
			if firsts.len() == 0 {
				return Err(Error::NoPath(Some(recipient_node.0.clone())))
			}
			let mut rng = rand::thread_rng();
			use rand::Rng;
			let n: usize = rng.gen_range(0, firsts.len());
			add_start = Some(firsts[n].clone());
			firsts[n].0.clone()
		};

		let recipient = if self.is_routing(recipient_node.0) {
			recipient_node.0.clone()
		} else {
			trace!(target: "mixnet", "Non routing recipient");
			if num_hops + 1 > max_hops {
				return Err(Error::TooManyHops)
			}

			if let Some(query) = last_query_if_surb {
				// use again a node that was recently connected.
				if let Some(rec) = query.get(0) {
					trace!(target: "mixnet", "Surbs last: {:?}", rec);
					add_end = Some(recipient_node);
					rec.0.clone()
				} else {
					return Err(Error::NoPath(Some(recipient_node.0.clone())))
				}
			} else {
				return Err(Error::NoPath(Some(recipient_node.0.clone())))
			}
		};
		trace!(target: "mixnet", "number hop: {:?}", num_hops);
		AuthorityTopology::fill_paths(
			&self.local_id,
			&self.routing_table,
			&mut self.paths,
			&mut self.paths_depth,
			&self.authorities_tables,
			num_hops,
		);
		let nb_path = count_paths(&self.paths, &start, &recipient, num_hops);
		debug!(target: "mixnet", "Number path for dest {:?}.", nb_path);
		debug!(target: "mixnet", "{:?} to {:?}", start, recipient);
		if nb_path < LOW_MIXNET_PATHS {
			trace!(target: "mixnet", "not enough paths: {:?}", &self.paths);
			// TODO NotEnoughPath error
			return Err(Error::NoPath(Some(recipient)))
		};

		let mut result = Vec::with_capacity(nb_chunk);
		while result.len() < nb_chunk {
			let path_ids =
				if let Some(path) = random_path(&self.paths, &start, &recipient, num_hops) {
					trace!(target: "mixnet", "Got path: {:?}", &path);
					path
				} else {
					return Err(Error::NoPath(Some(recipient)))
				};
			let mut path = Vec::with_capacity(num_hops + 1);
			if let Some((peer, key)) = add_start {
				debug!(target: "mixnet", "Add first, nexts {:?}.", path_ids.len());
				path.push((peer.clone(), key.clone()));
			}

			for peer_id in path_ids.into_iter() {
				if let Some(table) = self.authorities_tables.get(&peer_id) {
					path.push((peer_id, table.public_key.0.clone()));
				} else {
					error!(target: "mixnet", "node in routing_nodes must also be in connected_nodes");
					unreachable!("node in routing_nodes must also be in connected_nodes");
				}
			}
			if let Some(table) = self.authorities_tables.get(&recipient) {
				path.push((recipient.clone(), table.public_key.0.clone()));
			} else {
				if self.local_id == recipient {
					// surb reply
					path.push((self.local_id.clone(), self.routing_table.public_key.0.clone()));
				} else {
					error!(target: "mixnet", "Unknown recipient {:?}", recipient);
					return Err(Error::NotEnoughRoutingPeers)
				}
			}

			if let Some((peer, key)) = add_end {
				if let Some(key) = key {
					path.push((peer.clone(), key.clone()));
				} else {
					return Err(Error::NoPath(Some(recipient_node.0.clone())))
				}
			}
			result.push(path);
		}
		debug!(target: "mixnet", "Path: {:?}", result);
		Ok(result)
	}

	fn is_routing(&self, id: &MixPeerId) -> bool {
		self.authorities.contains_key(id)
	}

	fn connected(&mut self, peer_id: MixPeerId, key: MixPublicKey) {
		debug!(target: "mixnet", "Connected from internal");
		self.add_connected_peer(peer_id, key)
	}

	fn disconnect(&mut self, peer_id: &MixPeerId) {
		debug!(target: "mixnet", "Disconnected from internal");
		self.add_disconnected_peer(&peer_id);
	}

	fn allowed_external(&self, _id: &MixPeerId) -> Option<(usize, usize)> {
		// 10% TODO also limit nb connection (on check_handshake success)
		Some((1, 10))
	}

	fn handshake_size(&self) -> usize {
		32 + 32 + 64
	}

	fn check_handshake(
		&mut self,
		payload: &[u8],
		_from: &NetworkPeerId,
	) -> Option<(MixPeerId, MixPublicKey)> {
		let mut peer_id = [0u8; 32];
		peer_id.copy_from_slice(&payload[0..32]);
		let mut pk = [0u8; 32];
		pk.copy_from_slice(&payload[32..64]);
		let mut signature = [0u8; 64];
		signature.copy_from_slice(&payload[64..]);
		let signature = sp_application_crypto::sr25519::Signature(signature);
		let mut message = self.network_id.to_bytes().to_vec();
		message.extend_from_slice(&pk[..]);
		let key = sp_application_crypto::sr25519::Public(peer_id.clone());
		debug!(target: "mixnet", "check handshake: {:?}, {:?}, {:?} from {:?}", peer_id, message, signature, _from);
		use sp_application_crypto::RuntimePublic;
		if key.verify(&message, &signature) {
			if !self.accept_peer(&self.local_id, &peer_id) {
				return None
			}
			if self.is_routing(&peer_id) {
				// TODO this should be checking routing table from other peer and see peers
				// connected to multiple others so we wait for their connection or ping them.
				// TODO One could still for big difference in the priority list try to check if
				// online and at some point reject the peer as not being connected enough (globaly
				// with other authorities). Number can be big initially as authority may disable
				// mixnet a lot. TODO in case we are already receiving from more than a threshould
				// we drop others connection.
			}
			let pk = MixPublicKey::from(pk);
			Some((peer_id, pk))
		} else {
			None
		}
	}

	fn handshake(&mut self, with: &NetworkPeerId, public_key: &MixPublicKey) -> Option<Vec<u8>> {
		let mut result = self.local_id.to_vec();
		result.extend_from_slice(&public_key.as_bytes()[..]);
		let mut message = with.to_bytes().to_vec();
		message.extend_from_slice(&public_key.as_bytes()[..]);
		match SyncCryptoStore::sign_with(
			&*self.key_store,
			key_types::IM_ONLINE,
			&CryptoTypePublicPair(sp_core::sr25519::CRYPTO_ID, self.local_id.to_vec()),
			&message[..],
		) {
			Ok(Some(signature)) => {
				result.extend_from_slice(&signature[..]);
				trace!(target: "mixnet", "create handshake: {:?}, {:?}, {:?} with {:?}", self.local_id, message, signature, with);
				return Some(result)
			},
			Err(e) => {
				error!(target: "mixnet", "hanshake signing error: {:?}", e);
			},
			_ => (),
		}
		log::error!(target: "mixnet", "Missing imonline key for handshake.");
		None
	}
}

impl AuthorityTopology {
	#[cfg(test)]
	fn paths_mem_size(
		paths: &BTreeMap<usize, HashMap<MixPeerId, HashMap<MixPeerId, Vec<MixPeerId>>>>,
	) -> usize {
		// approximate and slow, just to get idea in test. TODO update when switching paths to use
		// indexes as ptr.
		let mut size = 0;
		for paths in paths.iter() {
			size += 8; // usize
			for paths in paths.1.iter() {
				size += 32;
				for paths in paths.1.iter() {
					size += 32;
					for paths in paths.1.iter() {
						size += 32;
					}
				}
			}
		}
		size
	}

	// TODO Note that building this is rather brutal, could just make some
	// random selection already to reduce size (and refresh after x uses).
	fn fill_paths(
		local_id: &MixPeerId,
		local_routing: &AuthorityTable,
		paths: &mut BTreeMap<usize, HashMap<MixPeerId, HashMap<MixPeerId, Vec<MixPeerId>>>>,
		paths_depth: &mut usize,
		authorities_tables: &BTreeMap<MixPeerId, AuthorityTable>,
		depth: usize,
	) {
		if &depth <= paths_depth {
			return
		}
		// TODO not strictly needed (all in authorities_tables), convenient to exclude local id..
		//		let mut from_to = HashMap::<MixPeerId, Vec<MixPeerId>>::new();
		// TODO not strictly needed (all in authorities_tables), convenient to exclude local id.
		let mut to_from = HashMap::<MixPeerId, Vec<MixPeerId>>::new();

		for (from, table) in
			authorities_tables.iter().chain(std::iter::once((local_id, local_routing)))
		{
			/*			from_to.insert(
				from.clone(),
				table
					.routing_table
					.connected_to
					.iter()
					.filter(|n| n != &local_id)
					.cloned()
					.collect(),
			);*/
			// TODO change if limitting size of receive_from
			to_from.insert(
				from.clone(),
				table
					.receive_from
					.iter()
					//					.filter(|n| n != &local_id)
					.cloned()
					.collect(),
			);
		}

		Self::fill_paths_inner(to_from, paths, *paths_depth, depth);
		if *paths_depth < depth {
			*paths_depth = depth;
		}
	}

	fn fill_paths_inner(
		to_from: HashMap<MixPeerId, Vec<MixPeerId>>,
		paths: &mut BTreeMap<usize, HashMap<MixPeerId, HashMap<MixPeerId, Vec<MixPeerId>>>>,
		paths_depth: usize,
		depth: usize,
	) {
		let mut start_depth = std::cmp::max(2, paths_depth);
		while start_depth < depth {
			let depth = start_depth + 1;
			if start_depth == 2 {
				let at = paths.entry(depth).or_default();
				for (to, mid) in to_from.iter() {
					let depth_paths: &mut HashMap<MixPeerId, Vec<MixPeerId>> =
						at.entry(to.clone()).or_default();
					for mid in mid {
						if let Some(parents) = to_from.get(mid) {
							for from in parents.iter() {
								// avoid two identical node locally (paths still contains
								// redundant node in some of its paths but being
								// distributed in a balanced way we will just avoid those
								// on each hop random calculation.
								if from == to {
									continue
								}
								depth_paths.entry(from.clone()).or_default().push(mid.clone());
							}
						}
					}
				}
			} else {
				let at = paths.entry(start_depth).or_default();
				let mut dest_at = HashMap::<MixPeerId, HashMap<MixPeerId, Vec<MixPeerId>>>::new();
				for (to, paths_to) in at.iter() {
					let depth_paths = dest_at.entry(to.clone()).or_default();
					for (mid, _) in paths_to.iter() {
						if let Some(parents) = to_from.get(mid) {
							for from in parents.iter() {
								if from == to {
									continue
								}
								depth_paths.entry(from.clone()).or_default().push(mid.clone());
							}
						}
					}
				}
				paths.insert(depth, dest_at);
			}
			start_depth += 1;
		}
	}

	fn publish_table(&mut self) -> (&Vec<u8>, bool) {
		let mut has_changed = false;
		if self.last_signed.1 < self.routing_table.session ||
			(self.last_signed.1 == self.routing_table.session &&
				self.last_signed.2 < self.routing_table.revision.0)
		{
			has_changed = true;
			let mut to_sign = self.local_id.to_vec();
			self.routing_table.encode_to(&mut to_sign);
			match SyncCryptoStore::sign_with(
				&*self.key_store,
				key_types::IM_ONLINE,
				&CryptoTypePublicPair(sp_core::sr25519::CRYPTO_ID, self.local_id.to_vec()),
				&to_sign[..],
			) {
				Ok(Some(signature)) => {
					trace!(target: "mixnet", "Signing done: {:?} {:?} {:?}", self.local_id.to_vec(), signature, to_sign);
					to_sign.extend_from_slice(&signature[..]);
					trace!(target: "mixnet", "create signed routing: {:?}", self.local_id);
					self.last_signed.0 = to_sign;
					self.last_signed.1 = self.routing_table.session;
					self.last_signed.2 = self.routing_table.revision.0;
				},
				Err(e) => {
					error!(target: "mixnet", "Table signing error: {:?}", e);
					// TODO return error
				},
				_ => (),
			}
		}
		(&self.last_signed.0, has_changed)
	}

	fn import_table(&mut self, payload: &[u8]) -> bool {
		if payload.len() < 64 + 32 {
			// not even a signature and peer id
			return false
		}

		let mut with = [0u8; 32];
		with.copy_from_slice(&payload[..32]);
		let mut signature = [0u8; 64];
		signature.copy_from_slice(&payload[payload.len() - 64..]);
		let signature = sp_application_crypto::sr25519::Signature(signature);

		//let authority_id = if let Some(auth) = self.authorities.get(&with) {
		if !self.authorities.contains_key(&with) {
			//			auth.clone()
			//		} else {
			trace!(target: "mixnet", "No authority found for with: {:?}, dropping table.", with);
			return false
		};

		let mut p = [0u8; 32];
		p.copy_from_slice(&with[..]);
		let key = sp_application_crypto::sr25519::Public(p);

		/*		let authority_id = if let Some(table) = self.authorities_tables.get(&with) {
			use sp_application_crypto::Public;
			if let Some(key) = self.sessions.get(&table.authority_id.to_public_crypto_pair()) {
				if &with[..] != key.1.as_slice() {
					trace!(target: "mixnet", "Wrong id for table");
				}
				table.authority_id.clone()
			} else {
				trace!(target: "mixnet", "Imonline key not found");
				return false
			}
		} else {
			trace!(target: "mixnet", "Payload for authority not in session: {:?}", payload);
			return false
		};*/
		debug!(target: "mixnet", "check table: {:?}", with);
		use sp_application_crypto::RuntimePublic;
		if !key.verify(&(payload[..payload.len() - 64].to_vec()), &signature) {
			// anybody can.
			trace!(target: "mixnet", "Wrong signing received: {:?} {:?} {:?}", key, signature, &payload[..payload.len() - 64]);
			return false
		}

		if let Some(payload) = AuthorityTable::decode(&mut &payload[32..payload.len() - 64]).ok() {
			let mut insert = true;
			if let Some(table) = self.authorities_tables.get(&with) {
				if payload.session < table.session ||
					(payload.session == table.session && payload.revision <= table.revision)
				{
					log::debug!(target: "mixnet", "Not updated routes: {:?}", self.authorities_tables);
					// TODO old tables should lower dht peer prio.
					insert = false;
				}
			}
			if insert {
				// TODO sanity check of table (not connected to too many peers), ~ consistent with
				// peer neighbors (at least from the connected one we know)...
				// TODO number of non connected neighbor that should be.
				self.authorities_tables.insert(with.clone(), payload);
				self.paths.clear();
				log::debug!(target: "mixnet", "current routes: {:?}", self.authorities_tables);
			}
			return true
		}
		false
	}
}

fn hash_authority_id(auth_id: &[u8]) -> sc_network::KademliaKey {
	use libp2p::multihash::MultihashDigest;
	let mut id = b"MixnetTopologyV0".to_vec();
	id.extend_from_slice(auth_id);

	sc_network::KademliaKey::new(&libp2p::multihash::Code::Sha2_256.digest(id.as_slice()).digest())
}

#[test]
fn test_fill_paths() {
	/*let nb_peers: u16 = 1000;
	let nb_forward = 10;
	let depth = 5;*/
	let nb_peers: u16 = 5;
	let nb_forward = 3;
	let depth = 4;

	let peers: Vec<[u8; 32]> = (0..nb_peers)
		.map(|i| {
			let mut id = [0u8; 32];
			id[0] = (i % 8) as u8;
			id[1] = (i / 8) as u8;
			id
		})
		.collect();
	let local_id = [255u8; 32];
	let authorities: BTreeMap<_, _> = peers
		.iter()
		.chain(std::iter::once(&local_id))
		.map(|p| (p.clone(), ()))
		.collect();

	/*	let from_to: HashMap<MixPeerId, Vec<MixPeerId>> = vec![
		(peers[0].clone(), vec![peers[1].clone(), peers[2].clone()]),
		(peers[1].clone(), vec![peers[3].clone(), peers[4].clone()]),
		(peers[2].clone(), vec![peers[0].clone(), peers[4].clone()]),
	]
	.into_iter()
	.collect();*/

	let mut from_to: HashMap<MixPeerId, Vec<MixPeerId>> = Default::default();
	for p in peers.iter().chain(std::iter::once(&local_id)) {
		let tos = AuthorityTable::should_connect_to(p, &authorities, nb_forward);
		from_to.insert(p.clone(), tos);
	}
	let mut to_from: HashMap<MixPeerId, Vec<MixPeerId>> = Default::default();
	//	let from_to2: BTreeMap<_, _> = from_to.iter().map(|(k, v)|(k.clone(), v.clone())).collect();
	for (from, tos) in from_to.iter() {
		for to in tos.iter() {
			to_from.entry(to.clone()).or_default().push(from.clone());
		}
	}
	//		let from_to = from_to.clone();
	//		let to_from = to_from.clone();
	let mut paths = BTreeMap::new();
	let paths_depth = 0;
	AuthorityTopology::fill_paths_inner(to_from, &mut paths, paths_depth, depth);
	//	println!("{:?}", paths);
	println!("size {:?}", AuthorityTopology::paths_mem_size(&paths));

	// there is a path but cycle on 0 (depth 4)
	//	assert!(random_path(&paths, &peers[0], &peers[1], depth).is_none());
	let nb_path = count_paths(&paths, &local_id, &peers[1], depth);
	println!("nb_path {:?}", nb_path);
	let path = random_path(&paths, &local_id, &peers[1], depth);
	if path.is_none() {
		assert_eq!(nb_path, 0);
	}
	println!("{:?}", path);
	let mut nb_reachable = 0;
	let mut med_nb_con = 0;
	for i in 1..nb_peers as usize {
		let path = random_path(&paths, &peers[0], &peers[i], depth);
		if path.is_some() {
			nb_reachable += 1;
		}
		let nb_path = count_paths(&paths, &peers[0], &peers[i], depth);
		med_nb_con += nb_path;
	}
	let med_nb_con = med_nb_con as f64 / (nb_peers as f64 - 1.0);
	let reachable = nb_reachable as f64 / (nb_peers as f64 - 1.0);
	println!("Reachable {:?}, Med nb {:?}", reachable, med_nb_con);
	panic!("to print");
}
