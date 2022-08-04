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

use codec::{Decode, Encode, Input, Output};
use futures::{future, FutureExt, StreamExt};
use futures_timer::Delay;
use log::{debug, error, trace, warn};
use sc_authority_discovery::{ExpIncInterval, NetworkProvider};
use sc_client_api::{BlockchainEvents, FinalityNotification, UsageProvider};
use sc_network::{DhtEvent, MixnetCommand, PeerId};
use sc_utils::mpsc::{tracing_unbounded, TracingUnboundedReceiver, TracingUnboundedSender};
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
	worker: mixnet::MixnetWorker<AuthorityStar>,
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
	pending_lookups: Vec<AuthorityId>,
	in_flight_lookups: HashMap<sc_network::KademliaKey, AuthorityId>,
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

		// TODO switch to config with generated key only.
		let config_tmp = mixnet::Config::new(local_public_key);

		let mixnet_config = if let Some((pub_key, priv_key)) = Self::get_mixnet_keys(&*key_store) {
			mixnet::Config::new_with_keys(local_public_key, pub_key, priv_key)
		} else {
			log::error!(target: "mixnet", "Not using grandpa key");
			mixnet::Config::new(local_public_key)
		};

		let finality_stream = client.finality_notification_stream();

		let topology = AuthorityStar::new(
			mixnet_config.local_id.clone(),
			PeerId::from_public_key(&network_identity.public()),
			mixnet_config.public_key.clone(),
			key_store.clone(),
			config_tmp.public_key,
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
			pending_lookups: Vec::new(),
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
		})
	}

	fn get_mixnet_keys(
		key_store: &dyn SyncCryptoStore,
	) -> Option<(MixPublicKey, mixnet::MixSecretKey)> {
		// get last key, if it is not the right one, node will restart on next
		// handle_new_authority call.
		let mut grandpa_key = None;
		for key in SyncCryptoStore::ed25519_public_keys(&*key_store, key_types::GRANDPA)
			.into_iter()
			.rev()
		{
			if SyncCryptoStore::has_keys(&*key_store, &[(key.0.into(), key_types::GRANDPA)]) {
				grandpa_key = Some(key);
				break
			} else {
				log::error!(target: "mixnet", "No private key for grandpa key");
			}
		}

		if let Some(grandpa_key) = grandpa_key {
			let mut p = [0u8; 32];
			p.copy_from_slice(grandpa_key.as_ref());
			let pub_key = mixnet::public_from_ed25519(p);

			let priv_key = SyncCryptoStore::mixnet_secret_from_ed25519(
				&*key_store,
				key_types::GRANDPA,
				&grandpa_key,
			)
			.ok()?;
			Some((pub_key, priv_key))
		} else {
			None
		}
	}

	fn start_new_lookups(&mut self) {
		while self.in_flight_lookups.len() < MAX_IN_FLIGHT_LOOKUPS {
			let authority_id = match self.pending_lookups.pop() {
				Some(authority) => authority,
				None => return,
			};
			let hash = hash_authority_id(authority_id.as_ref());
			self.network.get_value(&hash);
			self.in_flight_lookups.insert(hash, authority_id);

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
		let topology = &mut self.worker.mixnet_mut().topology;
		if !topology.routing {
			return
		}
		let (payload, has_changed) = topology.published_table();
		if only_if_changed && !has_changed {
			return
		}

		if let Some(authority_id) = self.authority_id.as_ref() {
			let key = hash_authority_id(authority_id.as_ref());
			debug!(target: "mixnet", "Publishing topology at {:?} on Dht.", key);
			self.network.put_value(key, payload.clone());
			self.latest_published_payload = Some(payload.clone());
		}
	}

	async fn refill_pending_lookups_queue(&mut self) {
		let topology = &mut self.worker.mixnet_mut().topology;
		// TODO filter recently fetched?
		let mut authorities: Vec<AuthorityId> =
			topology.authorities.values().map(|(_, id)| id.clone()).collect();

		use rand::seq::SliceRandom;
		authorities.shuffle(&mut rand::thread_rng());
		self.pending_lookups = authorities;
		// Ignore all still in-flight lookups. Those that are still in-flight are likely stalled as
		// query interval ticks are far enough apart for all lookups to succeed.
		self.in_flight_lookups.clear();
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
			self.start_new_lookups();
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
								self.refill_pending_lookups_queue().await;
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
							error!(target: "mixnet", "Could send transaction in mixnet {:?}", e);
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

		self.session = Some(session);
		self.fetch_new_session_keys(at, session);
		self.update_own_public_key_within_authority_set(&set);
		let mut remove_limit = Vec::new();
		let current_local_id = self.worker.local_id().clone();
		let current_public_key = self.worker.public_key().clone();
		let topology = &mut self.worker.mixnet_mut().topology;
		topology.current_session = session;
		topology.routing_table.session = session;
		topology.routing_table.revision = 1; // TODO load from persistence in case it is first call here (may be ongoing session
									 // of a restarted node).
		debug!(target: "mixnet", "Change authorities {:?}", set);
		let mut old_authority = std::mem::take(&mut topology.authorities);

		topology.routing = false;

		let mut restart = None;
		for (auth, _) in set.into_iter() {
			use sp_application_crypto::Public;
			let auth_pub_pair = auth.clone().to_public_crypto_pair(); // TODO change key type in map?
			if let Some(key) = topology.sessions.get(&auth_pub_pair) {
				let mut peer_id = [0u8; 32];
				peer_id.copy_from_slice(&key.1[..]);
				if old_authority.remove(&peer_id).is_none() {
					remove_limit.push(peer_id.clone());
				}
				// derive from grandpa one
				let mut p = [0u8; 32];
				p.copy_from_slice(auth.as_ref());
				let public_key = mixnet::public_from_ed25519(p);

				if self.authority_id.as_ref() == Some(&auth) {
					debug!(target: "mixnet", "In new authority set, routing.");
					topology.routing = true;
					let new_id = (current_local_id != peer_id).then(|| {
						topology.node_id = peer_id.clone();
						peer_id.clone()
					});
					let new_key = (current_public_key != public_key)
						.then(|| {
							let secret_key = SyncCryptoStore::mixnet_secret_from_ed25519(
								&*self.key_store,
								key_types::GRANDPA,
								&auth.into(),
							)
							.ok()?;
							topology.node_public_key = public_key.clone();

							Some((public_key.clone(), secret_key))
						})
						.flatten();
					if new_id.is_some() || new_key.is_some() {
						restart = Some((new_id, new_key));
					}
				} else {
					debug!(target: "mixnet", "Insert auth {:?}", peer_id);
					topology.authorities.insert(peer_id, (public_key, auth));
				}
			} else {
				error!(target: "mixnet", "Missing imonline key for authority {:?}, not adding it to topology.", auth);
			}
		}
		topology.should_connect_to = topology.routing_table.should_connect_to(
			&topology.node_id,
			&topology.authorities,
			usize::MAX,
		);
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
				if !self.worker.mixnet().topology.has_enough_nodes() {
					self.state = State::WaitingMorePeers;
				},
			State::WaitingMorePeers =>
				if self.worker.mixnet().topology.has_enough_nodes() {
					debug!(target: "mixnet", "Mixnet running.");
					self.state = State::Running;
				},
			State::Synching if synched =>
				if self.worker.mixnet().topology.has_enough_nodes() {
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
pub struct AuthorityStar {
	node_id: MixPeerId,
	network_id: PeerId,
	node_public_key: MixPublicKey,
	key_store: Arc<dyn SyncCryptoStore>,
	// true when we are in authorities set.
	routing: bool,
	// All authorities are considered connected (when building message except first hop).
	// TODO rem pub key
	authorities: BTreeMap<MixPeerId, (MixPublicKey, AuthorityId)>,
	// The connected nodes (for first hop use `authorities` joined `connected_nodes`).
	connected_nodes: HashMap<MixPeerId, MixPublicKey>,
	// Current session mapping of Grandpa key to IMonline key.
	// TODO switch to MixPeerId to imonline key.
	sessions: HashMap<CryptoTypePublicPair, CryptoTypePublicPair>,

	current_session: SetId,
	routing_table: AuthorityTable,
	last_signed: (Vec<u8>, SetId, u32),
	authorities_tables: BTreeMap<MixPeerId, AuthorityRouting>,
	should_connect_to: Vec<MixPeerId>,
	// TODO every N delay put a peer from our `should_connect_to` list
	// as off and allows connection with the next one.
	not_connected_or_refused: HashSet<MixPeerId>,
}

#[derive(Debug)]
struct AuthorityRouting {
	session_index: SetId,
	peer_id: MixPeerId,
	authority_id: AuthorityId, /* TODO authority id redundant with mixpeerid as we use it as
	                            * peer id. -> may make peer id a blake hash of it so we ensure
	                            * our random works. */

	routing_table: AuthorityTable,
}

/// Current published view of an authority routing table.
///
/// Table is currently signed by ImOnline key.
#[derive(Encode, Decode, Debug)]
pub struct AuthorityTable {
	session: SetId,
	// TODO this info should be stored in case of node restart (otherwhise until session change, no
	// consensus with peer routing table will be find and only active for next setid.
	revision: u32, // TODO compact
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
	connected_to: BTreeSet<MixPeerId>,
	receive_from: BTreeSet<MixPeerId>, /* incoming peer needed to check if published information
	                                    * is fresh. */
}

#[derive(Debug)]
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
const NUMBER_CONNECTED_FORWARD: usize = 2;

impl AuthorityTable {
	fn should_connect_to(
		&self,
		from: &MixPeerId,
		authorities: &BTreeMap<MixPeerId, (MixPublicKey, AuthorityId)>,
		nb: usize,
	) -> Vec<MixPeerId> {
		// return next peer we want to established connection with.
		// The order is given by hashing two peer id (from and destination) and
		// take the first n peers.

		Vec::new() // TODO implement
		   // TODO cache this function result between authorities update.
	}
	fn allow_receive(
		&self,
		from: &MixPeerId,
		authorities: &BTreeMap<MixPeerId, (MixPublicKey, AuthorityId)>,
	) -> bool {
		// from is supposed to be verified by handshake.
		// we allow any authorities: as we cannot say if they fail to reach their prioritized peers.
		// TODO One could still for big difference in the priority list try to check if online and
		// at some point reject the peer as not being connected enough (globaly with other
		// authorities). Number can be big initially as authority may disable mixnet a lot.
		let mut from_peer_id = [0u8; 32];
		from_peer_id.copy_from_slice(&from[..]);
		authorities.contains_key(&from_peer_id)
	}
}

#[derive(Clone)]
pub struct AuthorityInfo {
	pub grandpa_id: AuthorityId,
	pub authority_discovery_id: CryptoTypePublicPair,
}

impl AuthorityStar {
	/// Instantiate a new topology.
	pub fn new(
		node_id: MixPeerId,
		network_id: PeerId,
		node_public_key: MixPublicKey,
		key_store: Arc<dyn SyncCryptoStore>,
		node_public_key_new: MixPublicKey,
	) -> Self {
		let routing_table = AuthorityTable {
			public_key: EncodableMixPublicKey(node_public_key_new),
			connected_to: BTreeSet::new(),
			receive_from: BTreeSet::new(),
			revision: 0, // TODO load from persistent storage on handle_new authority.
			session: 0,
		};
		AuthorityStar {
			node_id,
			network_id,
			node_public_key,
			authorities: BTreeMap::new(),
			connected_nodes: HashMap::new(),
			sessions: HashMap::new(),
			routing: false,
			key_store,
			authorities_tables: BTreeMap::new(),
			routing_table,
			current_session: 0,
			not_connected_or_refused: Default::default(),
			should_connect_to: Default::default(),
			last_signed: Default::default(),
		}
	}

	fn has_enough_nodes(&self) -> bool {
		self.authorities.len() >= LOW_MIXNET_THRESHOLD
	}

	fn add_connected_peer(&mut self, peer_id: MixPeerId, key: MixPublicKey) {
		debug!(target: "mixnet", "Connected to mixnet {:?} {:?}", peer_id, key);
		if let Some(public_key) = self.connected_nodes.get_mut(&peer_id) {
			*public_key = key;
			return
		}

		self.connected_nodes.insert(peer_id, key);
	}

	fn add_disconnected_peer(&mut self, peer_id: &MixPeerId) {
		debug!(target: "mixnet", "Disconnected from mixnet {:?}", peer_id);
		let _ = self.connected_nodes.remove(peer_id);
	}
}

impl AuthorityStar {
	fn random_connected(
		&self,
		skip: impl Fn(&MixPeerId) -> bool,
	) -> Option<(MixPeerId, MixPublicKey)> {
		use rand::RngCore;
		if !self.has_enough_nodes() {
			debug!(target: "mixnet", "Not enough routing nodes for path.");
			return None
		}
		// Warning this assume that PeerId is a randomly distributed value.
		let mut ix = [0u8; 32];
		rand::thread_rng().fill_bytes(&mut ix[..]);

		trace!(target: "mixnet", "routing {:?}, ix {:?}", self.authorities, ix);
		for key in self.authorities.range(ix..) {
			if !skip(&key.0) {
				debug!(target: "mixnet", "Random route node");
				return Some((key.0.clone(), (key.1).0.clone()))
			}
		}
		for key in self.authorities.range(..ix).rev() {
			if !skip(&key.0) {
				debug!(target: "mixnet", "Random route node");
				return Some((key.0.clone(), (key.1).0.clone()))
			}
		}
		None
	}
}

impl Topology for AuthorityStar {
	fn first_hop_nodes(&self) -> Vec<(MixPeerId, MixPublicKey)> {
		self.authorities.iter().map(|(k, v)| (k.clone(), v.0.clone())).collect()
	}

	fn first_hop_nodes_external(
		&self,
		from: &MixPeerId,
		to: &MixPeerId,
	) -> Vec<(MixPeerId, MixPublicKey)> {
		// allow for all
		let mut keys: Vec<_> = self
			.first_hop_nodes()
			.into_iter()
			.filter(|(id, _key)| self.connected_nodes.contains_key(id))
			.filter(|(id, _key)| from != id)
			.filter(|(id, _key)| to != id)
			.collect();

		use rand::prelude::SliceRandom;
		keys.shuffle(&mut rand::thread_rng());
		keys
	}

	fn is_first_node(&self, id: &MixPeerId) -> bool {
		// allow for all
		self.is_routing(id)
	}

	fn random_recipient(&self, _from: &MixPeerId) -> Option<(MixPeerId, MixPublicKey)> {
		self.random_connected(|_| false)
	}

	/// For a given peer return a list of peers it is supposed to be connected to.
	/// Return `None` if peer is unknown to the topology.
	fn neighbors(&self, from: &MixPeerId) -> Option<Vec<(MixPeerId, MixPublicKey)>> {
		if self.authorities.contains_key(from) || (&self.node_id == from && self.routing) {
			Some(
				self.authorities
					.iter()
					.filter_map(|id| {
						if id.0 == from {
							None
						} else {
							Some((id.0.clone(), (id.1).0.clone()))
						}
					})
					.collect(),
			)
		} else {
			None
		}
	}

	fn routing_to(&self, from: &MixPeerId, to: &MixPeerId) -> bool {
		(self.authorities.contains_key(from) || (&self.node_id == from && self.routing)) &&
			(self.authorities.contains_key(to) || (&self.node_id == to && self.routing))
	}

	fn random_path(
		&self,
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
		let mut result = Vec::with_capacity(nb_chunk);
		while result.len() < nb_chunk {
			let mut ids = BTreeSet::new();
			ids.insert(start.clone());
			ids.insert(recipient.clone());
			while ids.len() - 2 < num_hops - 1 {
				if let Some(key) = self.random_connected(|k| ids.contains(k)) {
					trace!(target: "mixnet", "Add hop {:?}.", key);
					ids.insert(key.0);
				} else {
					debug!(target: "mixnet", "No random connected {:?}.", ids.len() - 2);
					return Err(Error::NotEnoughRoutingPeers)
				}
			}

			let mut path = Vec::with_capacity(num_hops + 1);
			if let Some((peer, key)) = add_start {
				debug!(target: "mixnet", "Add first ,nexts {:?}.", ids.len());
				path.push((peer.clone(), key.clone()));
			}

			ids.remove(&start);
			ids.remove(&recipient);
			for peer_id in ids.into_iter() {
				if let Some(public_key) = self.authorities.get(&peer_id) {
					path.push((peer_id, public_key.0.clone()));
				} else {
					error!(target: "mixnet", "node in routing_nodes must also be in connected_nodes");
					unreachable!("node in routing_nodes must also be in connected_nodes");
				}
			}
			if let Some(public_key) = self.connected_nodes.get(&recipient) {
				path.push((recipient.clone(), public_key.clone()));
			} else {
				if self.node_id == recipient {
					// surb reply
					path.push((self.node_id.clone(), self.node_public_key.clone()));
				} else {
					error!(target: "mixnet", "Unknown recipient");
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
		if id == &self.node_id {
			self.routing
		} else {
			self.authorities.contains_key(id)
		}
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
		// 10% TODO just limit nb connection (on check_handshake success)
		Some((1, 10))
	}

	fn handshake_size(&self) -> usize {
		32 + 32 + 64
	}

	fn check_handshake(
		&mut self,
		payload: &[u8],
		_from: &PeerId,
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
			if !self.accept_peer(&self.node_id, &peer_id) {
				return None
			}
			let pk = MixPublicKey::from(pk);
			Some((peer_id, pk))
		} else {
			None
		}
	}

	fn handshake(&mut self, with: &PeerId, public_key: &MixPublicKey) -> Option<Vec<u8>> {
		let mut result = self.node_id.to_vec();
		result.extend_from_slice(&public_key.as_bytes()[..]);
		let mut message = with.to_bytes().to_vec();
		message.extend_from_slice(&public_key.as_bytes()[..]);
		match SyncCryptoStore::sign_with(
			&*self.key_store,
			key_types::IM_ONLINE,
			&CryptoTypePublicPair(sp_core::sr25519::CRYPTO_ID, self.node_id.to_vec()),
			&message[..],
		) {
			Ok(Some(signature)) => {
				result.extend_from_slice(&signature[..]);
				trace!(target: "mixnet", "create handshake: {:?}, {:?}, {:?} with {:?}", self.node_id, message, signature, with);
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

impl AuthorityStar {
	fn published_table(&mut self) -> (&Vec<u8>, bool) {
		let mut has_changed = false;
		if !(self.last_signed.1 == self.routing_table.session &&
			self.last_signed.2 == self.routing_table.revision)
		{
			has_changed = true;
			let mut to_sign = self.node_id.to_vec();
			self.routing_table.encode_to(&mut to_sign);
			match SyncCryptoStore::sign_with(
				&*self.key_store,
				key_types::IM_ONLINE,
				&CryptoTypePublicPair(sp_core::sr25519::CRYPTO_ID, self.node_id.to_vec()),
				&to_sign[..],
			) {
				Ok(Some(signature)) => {
					trace!(target: "mixnet", "Signing done: {:?} {:?} {:?}", self.node_id.to_vec(), signature, to_sign);
					to_sign.extend_from_slice(&signature[..]);
					trace!(target: "mixnet", "create signed routing: {:?}", self.node_id);
					self.last_signed.0 = to_sign;
					self.last_signed.1 = self.routing_table.session;
					self.last_signed.2 = self.routing_table.revision;
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

		let (key, authority_id) = if let Some((gr_key, auth_id)) = self.authorities.get(&with) {
			use sp_application_crypto::Public;
			if let Some(key) = self.sessions.get(&auth_id.clone().to_public_crypto_pair()) {
				let mut p = [0u8; 32];
				p.copy_from_slice(&key.1[..]);
				(sp_application_crypto::sr25519::Public(p), auth_id.clone())
			} else {
				trace!(target: "mixnet", "Imonline key not found");
				return false
			}
		} else {
			trace!(target: "mixnet", "Payload for authority not in session: {:?}", payload);
			return false
		};
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
				if payload.session < table.routing_table.session ||
					(payload.session == table.routing_table.session &&
						payload.revision <= table.routing_table.revision)
				{
					log::debug!(target: "mixnet", "not updated routes: {:?}", self.authorities_tables);
					insert = false;
				}
			}
			if insert {
				// TODO sanity check of table (not connected to too many peers), ~ consistent with
				// peer neighbors (at least from the connected one we know)...
				self.authorities_tables.insert(
					with.clone(),
					AuthorityRouting {
						session_index: payload.session,
						peer_id: with.clone(),
						authority_id,
						routing_table: payload,
					},
				);
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
