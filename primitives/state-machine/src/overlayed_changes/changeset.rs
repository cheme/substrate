// This file is part of Substrate.

// Copyright (C) 2020-2022 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License

//! Houses the code that implements the transactional overlay storage.

use super::{StorageKey, StorageValue, Transactional};

use crate::stats::StateMachineStats;
use smallvec::SmallVec;
use sp_core::storage::BLOB_CHUNK_SIZE;
use sp_externalities::range_slice;
#[cfg(not(feature = "std"))]
use sp_std::collections::{btree_map::BTreeMap as Map, btree_set::BTreeSet as Set};
use sp_std::{collections::btree_map::BTreeMap, hash::Hash, vec::Vec};
#[cfg(feature = "std")]
use std::collections::{HashMap as Map, HashSet as Set};

const PROOF_OVERLAY_NON_EMPTY: &str = "\
	An OverlayValue is always created with at least one transaction and dropped as soon
	as the last transaction is removed; qed";

type Dirty<I> = SmallVec<[I; 5]>;
type DirtyKeysSets<K> = Dirty<Set<K>>;
type DirtyInfos = Dirty<bool>;
type DirtyBlobChunks = Dirty<Set<usize>>;
type Transactions<V> = SmallVec<[V; 5]>;
type BlobChunk = [u8; BLOB_CHUNK_SIZE];

/// Error returned when trying to commit or rollback while no transaction is open or
/// when the runtime is trying to close a transaction started by the client.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct NoOpenTransaction;

/// Error when calling `enter_runtime` when already being in runtime execution mode.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct AlreadyInRuntime;

/// Error when calling `exit_runtime` when not being in runtime exection mdde.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct NotInRuntime;

/// Describes in which mode the node is currently executing.
#[derive(Debug, Clone, Copy)]
pub enum ExecutionMode {
	/// Executing in client mode: Removal of all transactions possible.
	Client,
	/// Executing in runtime mode: Transactions started by the client are protected.
	Runtime,
}

/// An overlay that contains all versions of a value for a specific key.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub struct OverlayedEntry<V> {
	/// The individual versions of that value.
	/// One entry per transactions during that the value was actually written.
	transactions: Transactions<V>,
}

impl<V> Default for OverlayedEntry<V> {
	fn default() -> Self {
		Self { transactions: SmallVec::new() }
	}
}

/// History of value, with removal support.
pub type OverlayedValue = OverlayedEntry<Option<StorageValue>>;

/// Change set for basic key value with extrinsics index recording and removal support.
pub type OverlayedChangeSet = OverlayedMap<StorageKey, Option<StorageValue>>;

/// Change set for push only operation.
///
/// Warning api at this level expect chunk bounds
/// to be checked.
#[derive(Debug, Clone, Default)]
pub struct OverlayedChangeSetBlob {
	/// Stores the changes to the blob.
	chunks: Vec<OverlayedEntry<BlobChunk>>,

	/// Store indexes that where modified for a transaction layer
	/// and if size did change.
	dirty: DirtyBlobChunks,
}

#[derive(Debug, Default, Clone)]
pub struct OverlayedContext {
	/// The number of how many transactions beginning from the first transactions are started
	/// by the client. Those transactions are protected against close (commit, rollback)
	/// when in runtime mode.
	num_client_transactions: usize,
	/// Determines whether the node is using the overlay from the client or the runtime.
	execution_mode: ExecutionMode,
	/// Total number of transactions (same as `top.number_transaction`).
	number_transactions: usize,
}

/// Holds a set of changes with the ability modify them using nested transactions.
#[derive(Debug, Clone)]
pub struct OverlayedMap<K, V> {
	/// Stores the changes that this overlay constitutes.
	changes: BTreeMap<K, OverlayedEntry<V>>,
	/// Stores which keys are dirty per transaction. Needed in order to determine which
	/// values to merge into the parent transaction on commit. The length of this vector
	/// therefore determines how many nested transactions are currently open (depth).
	dirty_keys: DirtyKeysSets<K>,
}

impl<K, V> Default for OverlayedMap<K, V> {
	fn default() -> Self {
		Self { changes: BTreeMap::new(), dirty_keys: SmallVec::new() }
	}
}

/// Indicate a possibly different origin at start of a transaction.
#[derive(Debug, Clone)]
pub(crate) enum OriginEntry<M> {
	/// Current value is same as before.
	/// On rollback just rollback current.
	/// On commit just commit current.
	Current,
	/// Nothing before the current value.
	/// On rollback just remove current.
	/// On commit just use parent/previous
	/// origin entry (remove if none).
	Empty,
	/// Current values did overwrite this one.
	/// On rollback restore the removed state as current.
	/// On commit if previous origin is empty or current,
	/// replace by removed. If it is `MovedTo` or `Removed`, do nothing.
	/// This state should only apply when origin is not `Empty`.
	Removed(M),
	/// Current values were moved under a different
	/// name.
	/// On rollback, restore the current from the moved_from field of another entry
	/// indexed by this entry name.
	/// On commit, if previous origin is empty or current, replay by this one.
	/// If `MovedTo` or `Removed`, do nothing.
	/// This state should only apply when origin is not `Empty`.
	MovedTo,
}

#[derive(Debug, Clone)]
struct InfoEntry<I, M> {
	info: I,
	origin: OriginEntry<M>,
	moved_from_current: Option<Vec<u8>>,
	// When moving multiple times to a same name
	// we keep origin.
	others_moved_from: Option<Map<Vec<u8>, M>>,
}

impl<I, M> From<I> for InfoEntry<I, M> {
	fn from(info: I) -> Self {
		InfoEntry {
			info,
			origin: OriginEntry::Empty,
			moved_from_current: None,
			others_moved_from: None,
		}
	}
}

/// Holds history of infos attached to a storage.
#[derive(Debug, Clone)]
pub(crate) struct OverlayedInfos<I, M> {
	/// Internal infos.
	infos: OverlayedEntry<InfoEntry<I, M>>,
	/// Changes for infos.
	dirty_infos: DirtyInfos,
}

#[cfg(feature = "std")]
impl From<sp_core::storage::StorageMap> for OverlayedMap<StorageKey, Option<StorageValue>> {
	fn from(storage: sp_core::storage::StorageMap) -> Self {
		Self {
			changes: storage
				.into_iter()
				.map(|(k, v)| (k, OverlayedEntry { transactions: SmallVec::from_iter([Some(v)]) }))
				.collect(),
			// TODO add dirty_keys?
			dirty_keys: Default::default(),
		}
	}
}

impl From<Vec<u8>> for OverlayedChangeSetBlob {
	fn from(storage: Vec<u8>) -> Self {
		let mut set = OverlayedChangeSetBlob::default();

		let (chunk_end, chunk_end_offset) = super::blob_chunk_end_index(storage.len());
		if chunk_end_offset != 0 {
			let mut offset = 0;
			for i in 0..chunk_end {
				let chunk = set.modify_chunk(i);
				chunk[..].copy_from_slice(&storage[offset..offset + BLOB_CHUNK_SIZE]);
				offset += BLOB_CHUNK_SIZE;
			}
			let chunk = set.modify_chunk(chunk_end);
			chunk[..storage.len() - offset].copy_from_slice(&storage[offset..]);
		}
		set
	}
}

impl Default for ExecutionMode {
	fn default() -> Self {
		Self::Client
	}
}

impl<V> OverlayedEntry<V> {
	/// The value as seen by the current transaction.
	pub fn value_ref(&self) -> &V {
		&self.transactions.last().expect(PROOF_OVERLAY_NON_EMPTY)
	}

	/// The value as seen by the current transaction.
	pub fn into_value(mut self) -> V {
		self.transactions.pop().expect(PROOF_OVERLAY_NON_EMPTY)
	}

	/// Mutable reference to the most recent version.
	fn value_mut(&mut self) -> &mut V {
		self.transactions.last_mut().expect(PROOF_OVERLAY_NON_EMPTY)
	}

	/// Remove the last version and return it.
	fn pop_transaction(&mut self) -> V {
		self.transactions.pop().expect(PROOF_OVERLAY_NON_EMPTY)
	}

	/// Writes a new version of a value.
	///
	/// This makes sure that the old version is not overwritten and can be properly
	/// rolled back when required.
	fn set(&mut self, value: V, first_write_in_tx: bool) {
		if first_write_in_tx || self.transactions.is_empty() {
			self.transactions.push(value);
		} else {
			*self.value_mut() = value;
		}
	}
}

impl<V> OverlayedEntry<Option<V>> {
	/// The value as seen by the current transaction.
	pub fn value(&self) -> Option<&V> {
		self.value_ref().as_ref()
	}
}

/// Inserts a key into the dirty set.
///
/// Returns true iff we are currently have at least one open transaction and if this
/// is the first write to the given key that transaction.
fn insert_dirty<K: Ord + Hash>(set: &mut DirtyKeysSets<K>, key: K) -> bool {
	set.last_mut().map(|dk| dk.insert(key)).unwrap_or_default()
}

/// Same as `insert_dirty` but for indexed chunks.
fn insert_dirty_chunk(set: &mut DirtyBlobChunks, at: usize) -> bool {
	set.last_mut().map(|dk| dk.insert(at)).unwrap_or_default()
}

/// Mark info change value as dirty.
///
/// Returns true iff we are currently have at least one open transaction and if this
/// is the first write to the given key that transaction.
fn set_dirty_infos(info: &mut DirtyInfos) -> bool {
	info.last_mut()
		.map(|written| {
			if *written {
				false
			} else {
				*written = true;
				true
			}
		})
		.unwrap_or_default()
}

impl<K: Ord + Hash + Clone, V> OverlayedMap<K, V> {
	/// True if no changes at all are contained in the change set.
	pub fn is_empty(&self) -> bool {
		self.changes.is_empty()
	}

	/// Get an optional reference to the value stored for the specified key.
	pub fn get<Q>(&self, key: &Q) -> Option<&OverlayedEntry<V>>
	where
		K: sp_std::borrow::Borrow<Q>,
		Q: Ord + ?Sized,
	{
		self.changes.get(key)
	}

	/// Set a new value for the specified key.
	///
	/// Can be rolled back or committed when called inside a transaction.
	pub fn set(&mut self, key: K, value: V) {
		let overlayed = self.changes.entry(key.clone()).or_default();
		overlayed.set(value, insert_dirty(&mut self.dirty_keys, key));
	}

	/// Get a list of all changes as seen by current transaction.
	pub fn changes(&self) -> impl Iterator<Item = (&K, &OverlayedEntry<V>)> {
		self.changes.iter()
	}

	/// Get a list of all changes as seen by current transaction.
	pub fn changes_mut(&mut self) -> impl Iterator<Item = (&K, &mut OverlayedEntry<V>)> {
		self.changes.iter_mut()
	}

	/// Get a list of all changes as seen by current transaction, consumes
	/// the overlay.
	pub fn into_changes(self) -> impl Iterator<Item = (K, OverlayedEntry<V>)> {
		self.changes.into_iter()
	}

	/// Consume this changeset and return all committed changes.
	pub fn drain_committed(self) -> impl Iterator<Item = (K, V)> {
		self.changes.into_iter().map(|(k, mut v)| (k, v.pop_transaction()))
	}

	/// Returns the current nesting depth of the transaction stack.
	///
	/// A value of zero means that no transaction is open and changes are committed on write.
	pub fn transaction_depth(&self) -> usize {
		self.dirty_keys.len()
	}

	/// Start a new nested transaction.
	///
	/// This allows to either commit or roll back all changes that were made while this
	/// transaction was open. Any transaction must be closed by either `commit_transaction`
	/// or `rollback_transaction` before this overlay can be converted into storage changes.
	///
	/// Changes made without any open transaction are committed immediately.
	pub fn start_transaction(&mut self) {
		self.dirty_keys.push(Default::default());
	}

	/// Commit the last transaction started by `start_transaction`.
	///
	/// Any changes made during that transaction are committed. Returns an error if
	/// there is no open transaction that can be committed.
	pub fn commit_transaction(&mut self) -> Result<(), NoOpenTransaction> {
		self.close_transaction(false)
	}

	fn close_transaction(&mut self, rollback: bool) -> Result<(), NoOpenTransaction> {
		for key in self.dirty_keys.pop().ok_or(NoOpenTransaction)? {
			let overlayed = self.changes.get_mut(&key).expect(
				"\
				A write to an OverlayedValue is recorded in the dirty key set. Before an
				OverlayedValue is removed, its containing dirty set is removed. This
				function is only called for keys that are in the dirty set. qed\
			",
			);

			if rollback {
				overlayed.pop_transaction();

				// We need to remove the key as an `OverlayValue` with no transactions
				// violates its invariant of always having at least one transaction.
				if overlayed.transactions.is_empty() {
					self.changes.remove(&key);
				}
			} else {
				let has_predecessor = if let Some(dirty_keys) = self.dirty_keys.last_mut() {
					// Not the last tx: Did the previous tx write to this key?
					!dirty_keys.insert(key)
				} else {
					// Last tx: Is there already a value in the committed set?
					// Check against one rather than empty because the current tx is still
					// in the list as it is popped later in this function.
					overlayed.transactions.len() > 1
				};

				// We only need to merge if there is an pre-existing value. It may be a value from
				// the previous transaction or a value committed without any open transaction.
				if has_predecessor {
					let dropped_tx = overlayed.pop_transaction();
					*overlayed.value_mut() = dropped_tx;
				}
			}
		}

		Ok(())
	}
}

impl<K: Ord + Hash + Clone, V> Transactional for OverlayedMap<K, V> {
	fn rollback_transaction(&mut self) -> Result<bool, NoOpenTransaction> {
		self.close_transaction(true)?;
		Ok(self.changes.is_empty())
	}
}

impl OverlayedContext {
	/// Create a new changeset at the same transaction state but without any contents.
	///
	/// This changeset might be created when there are already open transactions.
	/// We need to catch up here so that the child is at the same transaction depth.
	pub fn spawn_child<K: Clone, V>(&self) -> OverlayedMap<K, V> {
		use sp_std::iter::repeat;
		OverlayedMap {
			changes: Default::default(),
			dirty_keys: repeat(Set::new()).take(self.transaction_depth()).collect(),
		}
	}

	/// Create a new blob chunk set at the same transaction state but without any contents.
	///
	/// This changeset might be created when there are already open transactions.
	/// We need to catch up here so that the child is at the same transaction depth.
	pub fn spawn_blob(&self) -> OverlayedChangeSetBlob {
		use sp_std::iter::repeat;
		OverlayedChangeSetBlob {
			chunks: Default::default(),
			dirty: repeat(Set::new()).take(self.transaction_depth()).collect(),
		}
	}

	/// Create a new info changeset at the same transaction state but without any contents.
	///
	/// This changeset might be created when there are already open transactions.
	/// We need to catch up here so that the child is at the same transaction depth.
	pub(crate) fn spawn_info<I, M>(&self, info: I) -> OverlayedInfos<I, M> {
		use sp_std::iter::repeat;
		let mut result = OverlayedInfos {
			infos: OverlayedEntry { transactions: SmallVec::from_iter([info.into()]) },
			dirty_infos: repeat(false).take(self.transaction_depth()).collect(),
		};
		result.dirty_infos.last_mut().map(|d| {
			*d = true;
		});
		result
	}

	/// Call this before transfering control to the runtime.
	///
	/// This protects all existing transactions from being removed by the runtime.
	/// Calling this while already inside the runtime will return an error.
	pub fn enter_runtime(&mut self) -> Result<(), AlreadyInRuntime> {
		if let ExecutionMode::Runtime = self.execution_mode {
			return Err(AlreadyInRuntime)
		}
		self.execution_mode = ExecutionMode::Runtime;
		self.num_client_transactions = self.number_transactions;
		Ok(())
	}

	/// Register start of a transaction in context.
	pub fn start_transaction(&mut self) {
		self.number_transactions += 1;
	}

	/// Rollback a transaction in context.
	pub fn rollback_transaction(&mut self) -> Result<(), NoOpenTransaction> {
		// runtime is not allowed to close transactions started by the client
		if let ExecutionMode::Runtime = self.execution_mode {
			if self.number_transactions <= self.num_client_transactions {
				return Err(NoOpenTransaction)
			}
		}
		if self.number_transactions == 0 {
			return Err(NoOpenTransaction)
		}

		self.number_transactions -= 1;
		Ok(())
	}

	/// Rollback a transaction in context.
	pub fn commit_transaction(&mut self) -> Result<(), NoOpenTransaction> {
		self.rollback_transaction()
	}

	/// Call this when control returns from the runtime.
	/// Returns number of transaction to close.
	#[must_use = "Transactions should be rollback or commited depending on logic."]
	pub fn exit_runtime(&mut self) -> Result<usize, NotInRuntime> {
		if let ExecutionMode::Client = self.execution_mode {
			return Err(NotInRuntime)
		}
		self.execution_mode = ExecutionMode::Client;
		let result = self.number_transactions - self.num_client_transactions;
		self.num_client_transactions = 0;
		Ok(result)
	}

	/// Panic if we cannot drain committed (if transactions are open).
	pub fn guard_drain_committed(&self) {
		assert!(self.number_transactions == 0, "Drain is not allowed with open transactions.");
	}

	/// Number of open transaction (including non runtime ones).
	pub fn transaction_depth(&self) -> usize {
		self.number_transactions
	}
}

impl<I: Clone, M> OverlayedInfos<I, M> {
	/// Consume this infos and return the info if it is existing.
	pub fn drain_committed(self) -> I {
		self.infos.into_value().info
	}

	/// Current value for infos.
	pub fn value_ref(&self) -> &I {
		&self.infos.value_ref().info
	}

	/// The value as seen by the current transaction.
	pub fn into_value(self) -> I {
		self.infos.into_value().info
	}

	/// Access mutable value on  the change set.
	/// This should be use to change the value as it will
	/// create a new overlay value if there is none for
	/// current transaction.
	#[must_use = "A change was registered, so this value MUST be modified."]
	#[inline]
	pub fn modify_value(&mut self) -> &mut I {
		debug_assert!(!self.infos.transactions.is_empty());
		if set_dirty_infos(&mut self.dirty_infos) {
			let infos = self.infos.value_mut();
			let mut value: InfoEntry<I, M> = infos.info.clone().into();
			value.origin = OriginEntry::Current;
			// first write, add a layer
			self.infos.transactions.push(value);
		}

		&mut self.infos.value_mut().info
	}

	/// Check if we should store origin for a possible rollback.
	pub fn need_keep_origin(&self) -> bool {
		if self.dirty_infos.last().copied().unwrap_or(true) {
			match &self.infos.value_ref().origin {
				OriginEntry::Current => true,
				_ => false,
			}
		} else {
			match &self.infos.value_ref().origin {
				OriginEntry::Empty => false,
				_ => true,
			}
		}
	}

	/// Take is previous moved from origin.
	pub fn take_is_previous_moved_from(&mut self) -> Option<Vec<u8>> {
		if self.dirty_infos.last().copied().unwrap_or(false) {
			self.infos.value_mut().moved_from_current.take()
		} else {
			None
		}
	}

	/// Take is previous moved from origin.
	pub fn take_others_previous_moved_from(&mut self) -> Option<Map<Vec<u8>, M>> {
		if self.dirty_infos.last().copied().unwrap_or(false) {
			self.infos.value_mut().others_moved_from.take()
		} else {
			None
		}
	}

	/// Take last_removed of current transaction layer.
	pub(crate) fn take_last_origin(&mut self) -> Option<OriginEntry<M>> {
		if self.dirty_infos.last().copied().unwrap_or(false) {
			let infos = self.infos.value_mut();
			Some(if let &OriginEntry::Empty = &infos.origin {
				OriginEntry::Empty
			} else {
				sp_std::mem::replace(&mut infos.origin, OriginEntry::Current)
			})
		} else {
			None
		}
	}

	/// Set last_removed in current transaction layer.
	pub fn set_last_removed(&mut self, last_removed: M) {
		if set_dirty_infos(&mut self.dirty_infos) {
			let info = self.infos.value_ref().info.clone();
			// first write, add a layer
			self.infos.transactions.push(info.into());
		}

		let infos = self.infos.value_mut();
		debug_assert!(matches!(infos.origin, OriginEntry::Current));
		infos.origin = OriginEntry::Removed(last_removed);
	}

	/// Set moved_to in current transaction layer.
	pub fn set_moved_to(&mut self) {
		if set_dirty_infos(&mut self.dirty_infos) {
			let info = self.infos.value_ref().info.clone();
			// first write, add a layer
			self.infos.transactions.push(info.into());
		}

		let infos = self.infos.value_mut();
		debug_assert!(matches!(infos.origin, OriginEntry::Current));
		infos.origin = OriginEntry::MovedTo;
	}

	/// Set moved_to in current transaction layer.
	pub fn set_moved_from(&mut self, moved_from_name: Vec<u8>, moved_from_state: Option<M>) {
		if set_dirty_infos(&mut self.dirty_infos) {
			let info = self.infos.value_ref().info.clone();
			// first write, add a layer
			self.infos.transactions.push(info.into());
		}

		let infos = self.infos.value_mut();
		if let Some(state) = moved_from_state {
			if infos.others_moved_from.is_none() {
				infos.others_moved_from = Some(Map::new());
			}
			infos
				.others_moved_from
				.as_mut()
				.expect("Init above")
				.insert(moved_from_name, state);
		} else {
			debug_assert!(infos.moved_from_current.is_none());
			infos.moved_from_current = Some(moved_from_name);
		}
	}

	/// Start a new nested transaction.
	///
	/// This allows to either commit or roll back all changes that were made while this
	/// transaction was open. Any transaction must be closed by either `commit_transaction`
	/// or `rollback_transaction` before this overlay can be converted into storage changes.
	///
	/// Changes made without any open transaction are committed immediately.
	pub fn start_transaction(&mut self) {
		self.dirty_infos.push(Default::default());
	}

	/// Commit the last transaction started by `start_transaction`.
	///
	/// Any changes made during that transaction are committed. Returns an error if
	/// there is no open transaction that can be committed.
	pub fn commit_transaction(&mut self) -> Result<(), NoOpenTransaction> {
		if self.dirty_infos.pop().ok_or(NoOpenTransaction)? {
			let need_merge = if let Some(prev) = self.dirty_infos.last_mut() {
				if *prev {
					false
				} else {
					*prev = true;
					true
				}
			} else {
				self.infos.transactions.len() > 1
			};

			// We only need to merge if there is an pre-existing value. It may be a value from
			// the previous transaction or a value committed without any open transaction.
			if need_merge {
				let dropped_tx = self.infos.pop_transaction();
				// origin of committed is not of any use, origin of previous would work,
				// same for moved from.
				// Only keep the current info.
				self.infos.value_mut().info = dropped_tx.info;
			}
		}
		Ok(())
	}
}

impl<I: Clone, M> Transactional for OverlayedInfos<I, M> {
	fn rollback_transaction(&mut self) -> Result<bool, NoOpenTransaction> {
		if self.dirty_infos.pop().ok_or(NoOpenTransaction)? {
			self.infos.pop_transaction();
		}
		Ok(self.infos.transactions.is_empty())
	}
}

impl OverlayedChangeSetBlob {
	/// Access blob storage build from multiple chunks.
	pub fn storage(
		&self,
		start: u32,
		limit: Option<u32>,
		size: usize,
		stats: &StateMachineStats,
	) -> Vec<u8> {
		let start = start as usize;
		if start >= size {
			return Vec::new()
		}
		let size_slice = size - start;
		let size_slice =
			sp_std::cmp::min(size_slice, limit.map(|l| l as usize).unwrap_or(size_slice));
		let end = start + size_slice;
		if end == start {
			return Vec::new()
		}

		let (chunk_start, chunk_start_offset) = super::blob_chunk_start_index(start);
		let (chunk_end, chunk_end_offset) = super::blob_chunk_end_index(end);
		let mut result = Vec::with_capacity(size_slice);
		if chunk_start == chunk_end {
			result.extend_from_slice(
				&self.get(chunk_start).value_ref().as_ref()[chunk_start_offset..chunk_end_offset],
			)
		} else {
			result.extend_from_slice(
				&self.get(chunk_start).value_ref().as_ref()[chunk_start_offset..],
			);
			for at in chunk_start + 1..chunk_end {
				result.extend_from_slice(self.get(at).value_ref().as_ref());
			}
			result.extend_from_slice(&self.get(chunk_end).value_ref().as_ref()[..chunk_end_offset]);
		}
		stats.tally_read_modified(result.len() as u64);
		result
	}

	/// Change blob storage, no offset bound checks.
	pub(super) fn set(&mut self, value: &[u8], offset: u32) -> bool {
		let end = offset as usize + value.len();
		if end > u32::MAX as usize {
			return false
		}

		if value.len() == 0 {
			return true
		}

		let (chunk_start, chunk_start_offset) = super::blob_chunk_start_index(offset as usize);
		let (chunk_end, chunk_end_offset) = super::blob_chunk_end_index(end);

		if chunk_start == chunk_end {
			let chunk = self.modify_chunk(chunk_start);
			chunk[chunk_start_offset..chunk_end_offset].copy_from_slice(value);
		} else {
			let chunk = self.modify_chunk(chunk_start);
			let mut offset = BLOB_CHUNK_SIZE - chunk_start_offset;
			chunk[chunk_start_offset..].copy_from_slice(&value[..offset]);
			for i in chunk_start + 1..chunk_end {
				let chunk = self.modify_chunk(i);
				chunk[..].copy_from_slice(&value[offset..offset + BLOB_CHUNK_SIZE]);
				offset += BLOB_CHUNK_SIZE;
			}
			let chunk = self.modify_chunk(chunk_end);
			chunk[..chunk_end_offset].copy_from_slice(&value[offset..]);
		}
		true
	}

	/// Get an optional reference to the blob chunk at a given chunk number.
	pub(super) fn get(&self, index: u32) -> &OverlayedEntry<BlobChunk> {
		&self.chunks[index as usize]
	}

	/// Entry iter.
	pub(super) fn into_iter(self) -> impl Iterator<Item = OverlayedEntry<BlobChunk>> {
		self.chunks.into_iter()
	}

	/// Clearing is not needed as we always check size on access.
	/// This method will therefore only clean entry when possible.
	pub(super) fn truncate_clean(&mut self, start_chunk: u32) {
		if self.dirty.len() == 0 {
			// can happen on empty blob.
			return
		}
		let nb_chunks = self.chunks.len() as u32;
		let Some(dirty) = self.dirty.last_mut() else {
			return
		};

		for i in (start_chunk..nb_chunks).rev() {
			let i = i as usize;
			if dirty.remove(&i) {
				self.chunks[i].transactions.pop();
				if self.chunks[i].transactions.is_empty() {
					self.chunks.pop();
				} else {
					break
				}
			} else {
				break
			}
		}
	}

	/// Access mutable value on  the change set.
	///
	/// If undefined, init with default.
	///
	/// This should be use to change the value as it will
	/// create a new overlay value if there is none for
	/// current transaction.
	///
	/// Index must be checked to never be more than current chunks len.
	#[must_use = "A change was registered, so this value MUST be modified."]
	pub(super) fn modify_chunk(&mut self, at: u32) -> &mut BlobChunk {
		let at = at as usize;
		if at == self.chunks.len() {
			self.chunks.push(OverlayedEntry { transactions: SmallVec::new() });
		}

		let chunk = &mut self.chunks[at];
		let first_write_in_tx = insert_dirty_chunk(&mut self.dirty, at);
		let clone_into_new_tx = if let Some(tx) = chunk.transactions.last() {
			if first_write_in_tx {
				Some(tx.clone())
			} else {
				None
			}
		} else {
			Some([0u8; BLOB_CHUNK_SIZE])
		};
		if let Some(cloned) = clone_into_new_tx {
			chunk.set(cloned, first_write_in_tx);
		}
		chunk.value_mut()
	}

	/// Start a new nested transaction.
	///
	/// This allows to either commit or roll back all changes that were made while this
	/// transaction was open. Any transaction must be closed by either `commit_transaction`
	/// or `rollback_transaction` before this overlay can be converted into storage changes.
	///
	/// Changes made without any open transaction are committed immediately.
	/// TODO merge all transaction related stuff somehow between all struct.
	pub fn start_transaction(&mut self) {
		self.dirty.push(Default::default());
	}

	/// Commit the last transaction started by `start_transaction`.
	///
	/// Any changes made during that transaction are committed. Returns an error if
	/// there is no open transaction that can be committed.
	pub fn commit_transaction(&mut self) -> Result<(), NoOpenTransaction> {
		self.close_transaction(false)
	}

	fn close_transaction(&mut self, rollback: bool) -> Result<(), NoOpenTransaction> {
		for key in self.dirty.pop().ok_or(NoOpenTransaction)? {
			let overlayed = &mut self.chunks[key];

			if rollback {
				overlayed.pop_transaction();
			} else {
				let has_predecessor = if let Some(dirty_keys) = self.dirty.last_mut() {
					// Not the last tx: Did the previous tx write to this key?
					!dirty_keys.insert(key)
				} else {
					// Last tx: Is there already a value in the committed set?
					// Check against one rather than empty because the current tx is still
					// in the list as it is popped later in this function.
					overlayed.transactions.len() > 1
				};

				// We only need to merge if there is an pre-existing value. It may be a value from
				// the previous transaction or a value committed without any open transaction.
				if has_predecessor {
					let dropped_tx = overlayed.pop_transaction();
					*overlayed.value_mut() = dropped_tx;
				}
			}
		}

		if rollback {
			for i in (0..self.chunks.len()).rev() {
				if self.chunks[i].transactions.is_empty() {
					self.chunks.pop();
				} else {
					break
				}
			}
		}

		Ok(())
	}
}

impl Transactional for OverlayedChangeSetBlob {
	fn rollback_transaction(&mut self) -> Result<bool, NoOpenTransaction> {
		self.close_transaction(true)?;
		Ok(self.chunks.is_empty())
	}
}

impl OverlayedChangeSet {
	/// Returns a double-Option: None if the key is unknown (i.e. and the query should be referred
	/// to the backend); Some(None) if the key has been deleted. Some(Some(...)) for a key whose
	/// value has been set.
	pub fn storage(
		&self,
		key: &[u8],
		start: u32,
		limit: Option<u32>,
		stats: &StateMachineStats,
	) -> Option<Option<&[u8]>> {
		if let Some(x) = self.get(key) {
			let value = range_slice(x.value().map(AsRef::as_ref), start, limit);
			let size_read = value.map(|x| x.len() as u64).unwrap_or(0);
			stats.tally_read_modified(size_read);
			Some(value)
		} else {
			None
		}
	}

	/// Get a mutable reference for a value.
	///
	/// Can be rolled back or committed when called inside a transaction.
	#[must_use = "A change was registered, so this value MUST be modified."]
	pub fn modify(
		&mut self,
		key: StorageKey,
		init: impl Fn() -> StorageValue,
	) -> &mut Option<StorageValue> {
		let overlayed = self.changes.entry(key.clone()).or_default();
		let first_write_in_tx = insert_dirty(&mut self.dirty_keys, key);
		let clone_into_new_tx = if let Some(tx) = overlayed.transactions.last() {
			if first_write_in_tx {
				Some(tx.clone())
			} else {
				None
			}
		} else {
			Some(Some(init()))
		};

		if let Some(cloned) = clone_into_new_tx {
			overlayed.set(cloned, first_write_in_tx);
		}
		overlayed.value_mut()
	}

	/// Set all values to deleted which are matched by the predicate.
	///
	/// Can be rolled back or committed when called inside a transaction.
	pub fn clear_where(&mut self, predicate: impl Fn(&[u8], &OverlayedValue) -> bool) -> u32 {
		let mut count = 0;
		for (key, val) in self.changes.iter_mut().filter(|(k, v)| predicate(k, v)) {
			if val.value_ref().is_some() {
				count += 1;
			}
			val.set(None, insert_dirty(&mut self.dirty_keys, key.clone()));
		}
		count
	}

	/// Get the iterator over all changes that follow the supplied `key`.
	pub fn changes_after(&self, key: &[u8]) -> impl Iterator<Item = (&[u8], &OverlayedValue)> {
		use sp_std::ops::Bound;
		let range = (Bound::Excluded(key), Bound::Unbounded);
		self.changes.range::<[u8], _>(range).map(|(k, v)| (k.as_slice(), v))
	}
}

#[cfg(test)]
mod test {
	use super::*;
	use pretty_assertions::assert_eq;

	type Changes<'a> = Vec<(&'a [u8], Option<&'a [u8]>)>;
	type Drained<'a> = Vec<(&'a [u8], Option<&'a [u8]>)>;

	fn assert_changes(is: &OverlayedChangeSet, expected: &Changes) {
		let is: Changes =
			is.changes().map(|(k, v)| (k.as_ref(), v.value().map(AsRef::as_ref))).collect();
		assert_eq!(&is, expected);
	}

	fn assert_drained_changes(is: OverlayedChangeSet, expected: Changes) {
		let is = is.drain_committed().collect::<Vec<_>>();
		let expected = expected
			.iter()
			.map(|(k, v)| (k.to_vec(), v.map(From::from)))
			.collect::<Vec<_>>();
		assert_eq!(is, expected);
	}

	fn assert_drained(is: OverlayedChangeSet, expected: Drained) {
		let is = is.drain_committed().collect::<Vec<_>>();
		let expected = expected
			.iter()
			.map(|(k, v)| (k.to_vec(), v.map(From::from)))
			.collect::<Vec<_>>();
		assert_eq!(is, expected);
	}

	#[test]
	fn no_transaction_works() {
		let mut changeset = OverlayedChangeSet::default();
		assert_eq!(changeset.transaction_depth(), 0);

		changeset.set(b"key0".to_vec(), Some(b"val0".to_vec()));
		changeset.set(b"key1".to_vec(), Some(b"val1".to_vec()));
		changeset.set(b"key0".to_vec(), Some(b"val0-1".to_vec()));

		assert_drained(changeset, vec![(b"key0", Some(b"val0-1")), (b"key1", Some(b"val1"))]);
	}

	#[test]
	fn transaction_works() {
		let mut changeset = OverlayedChangeSet::default();
		assert_eq!(changeset.transaction_depth(), 0);

		// no transaction: committed on set
		changeset.set(b"key0".to_vec(), Some(b"val0".to_vec()));
		changeset.set(b"key1".to_vec(), Some(b"val1".to_vec()));
		changeset.set(b"key0".to_vec(), Some(b"val0-1".to_vec()));

		changeset.start_transaction();
		assert_eq!(changeset.transaction_depth(), 1);

		// we will commit that later
		changeset.set(b"key42".to_vec(), Some(b"val42".to_vec()));
		changeset.set(b"key99".to_vec(), Some(b"val99".to_vec()));

		changeset.start_transaction();
		assert_eq!(changeset.transaction_depth(), 2);

		// we will roll that back
		changeset.set(b"key42".to_vec(), Some(b"val42-rolled".to_vec()));
		changeset.set(b"key7".to_vec(), Some(b"val7-rolled".to_vec()));
		changeset.set(b"key0".to_vec(), Some(b"val0-rolled".to_vec()));
		changeset.set(b"key5".to_vec(), Some(b"val5-rolled".to_vec()));

		// changes contain all changes not only the commmited ones.
		let all_changes: Changes = vec![
			(b"key0", Some(b"val0-rolled")),
			(b"key1", Some(b"val1")),
			(b"key42", Some(b"val42-rolled")),
			(b"key5", Some(b"val5-rolled")),
			(b"key7", Some(b"val7-rolled")),
			(b"key99", Some(b"val99")),
		];
		assert_changes(&changeset, &all_changes);

		// this should be no-op
		changeset.start_transaction();
		assert_eq!(changeset.transaction_depth(), 3);
		changeset.start_transaction();
		assert_eq!(changeset.transaction_depth(), 4);
		changeset.rollback_transaction().unwrap();
		assert_eq!(changeset.transaction_depth(), 3);
		changeset.commit_transaction().unwrap();
		assert_eq!(changeset.transaction_depth(), 2);
		assert_changes(&changeset, &all_changes);

		// roll back our first transactions that actually contains something
		changeset.rollback_transaction().unwrap();
		assert_eq!(changeset.transaction_depth(), 1);

		let rolled_back: Changes = vec![
			(b"key0", Some(b"val0-1")),
			(b"key1", Some(b"val1")),
			(b"key42", Some(b"val42")),
			(b"key99", Some(b"val99")),
		];
		assert_changes(&changeset, &rolled_back);

		changeset.commit_transaction().unwrap();
		assert_eq!(changeset.transaction_depth(), 0);
		assert_changes(&changeset, &rolled_back);

		assert_drained_changes(changeset, rolled_back);
	}

	#[test]
	fn transaction_commit_then_rollback_works() {
		let mut changeset = OverlayedChangeSet::default();
		assert_eq!(changeset.transaction_depth(), 0);

		changeset.set(b"key0".to_vec(), Some(b"val0".to_vec()));
		changeset.set(b"key1".to_vec(), Some(b"val1".to_vec()));
		changeset.set(b"key0".to_vec(), Some(b"val0-1".to_vec()));

		changeset.start_transaction();
		assert_eq!(changeset.transaction_depth(), 1);

		changeset.set(b"key42".to_vec(), Some(b"val42".to_vec()));
		changeset.set(b"key99".to_vec(), Some(b"val99".to_vec()));

		changeset.start_transaction();
		assert_eq!(changeset.transaction_depth(), 2);

		changeset.set(b"key42".to_vec(), Some(b"val42-rolled".to_vec()));
		changeset.set(b"key7".to_vec(), Some(b"val7-rolled".to_vec()));
		changeset.set(b"key0".to_vec(), Some(b"val0-rolled".to_vec()));
		changeset.set(b"key5".to_vec(), Some(b"val5-rolled".to_vec()));

		let all_changes: Changes = vec![
			(b"key0", Some(b"val0-rolled")),
			(b"key1", Some(b"val1")),
			(b"key42", Some(b"val42-rolled")),
			(b"key5", Some(b"val5-rolled")),
			(b"key7", Some(b"val7-rolled")),
			(b"key99", Some(b"val99")),
		];
		assert_changes(&changeset, &all_changes);

		// this should be no-op
		changeset.start_transaction();
		assert_eq!(changeset.transaction_depth(), 3);
		changeset.start_transaction();
		assert_eq!(changeset.transaction_depth(), 4);
		changeset.rollback_transaction().unwrap();
		assert_eq!(changeset.transaction_depth(), 3);
		changeset.commit_transaction().unwrap();
		assert_eq!(changeset.transaction_depth(), 2);
		assert_changes(&changeset, &all_changes);

		changeset.commit_transaction().unwrap();
		assert_eq!(changeset.transaction_depth(), 1);

		assert_changes(&changeset, &all_changes);

		changeset.rollback_transaction().unwrap();
		assert_eq!(changeset.transaction_depth(), 0);

		let rolled_back: Changes = vec![(b"key0", Some(b"val0-1")), (b"key1", Some(b"val1"))];
		assert_changes(&changeset, &rolled_back);

		assert_drained_changes(changeset, rolled_back);
	}

	#[test]
	fn modify_works() {
		let mut changeset = OverlayedChangeSet::default();
		assert_eq!(changeset.transaction_depth(), 0);
		let init = || b"valinit".to_vec();

		// committed set
		changeset.set(b"key0".to_vec(), Some(b"val0".to_vec()));
		changeset.set(b"key1".to_vec(), None);
		let val = changeset.modify(b"key3".to_vec(), init);
		assert_eq!(val, &Some(b"valinit".to_vec()));
		val.as_mut().unwrap().extend_from_slice(b"-modified");

		changeset.start_transaction();
		assert_eq!(changeset.transaction_depth(), 1);
		changeset.start_transaction();
		assert_eq!(changeset.transaction_depth(), 2);

		// non existing value -> init value should be returned
		let val = changeset.modify(b"key2".to_vec(), init);
		assert_eq!(val, &Some(b"valinit".to_vec()));
		val.as_mut().unwrap().extend_from_slice(b"-modified");

		// existing value should be returned by modify
		let val = changeset.modify(b"key0".to_vec(), init);
		assert_eq!(val, &Some(b"val0".to_vec()));
		val.as_mut().unwrap().extend_from_slice(b"-modified");

		// should work for deleted keys
		let val = changeset.modify(b"key1".to_vec(), init);
		assert_eq!(val, &None);
		*val = Some(b"deleted-modified".to_vec());

		let all_changes: Changes = vec![
			(b"key0", Some(b"val0-modified")),
			(b"key1", Some(b"deleted-modified")),
			(b"key2", Some(b"valinit-modified")),
			(b"key3", Some(b"valinit-modified")),
		];
		assert_changes(&changeset, &all_changes);
		changeset.commit_transaction().unwrap();
		assert_eq!(changeset.transaction_depth(), 1);
		assert_changes(&changeset, &all_changes);

		changeset.rollback_transaction().unwrap();
		assert_eq!(changeset.transaction_depth(), 0);
		let rolled_back: Changes =
			vec![(b"key0", Some(b"val0")), (b"key1", None), (b"key3", Some(b"valinit-modified"))];
		assert_changes(&changeset, &rolled_back);
		assert_drained_changes(changeset, rolled_back);
	}

	#[test]
	fn clear_works() {
		let mut changeset = OverlayedChangeSet::default();

		changeset.set(b"key0".to_vec(), Some(b"val0".to_vec()));
		changeset.set(b"key1".to_vec(), Some(b"val1".to_vec()));
		changeset.set(b"del1".to_vec(), Some(b"delval1".to_vec()));
		changeset.set(b"del2".to_vec(), Some(b"delval2".to_vec()));

		changeset.start_transaction();

		changeset.clear_where(|k, _| k.starts_with(b"del"));

		assert_changes(
			&changeset,
			&vec![
				(b"del1", None),
				(b"del2", None),
				(b"key0", Some(b"val0")),
				(b"key1", Some(b"val1")),
			],
		);

		changeset.rollback_transaction().unwrap();

		assert_changes(
			&changeset,
			&vec![
				(b"del1", Some(b"delval1")),
				(b"del2", Some(b"delval2")),
				(b"key0", Some(b"val0")),
				(b"key1", Some(b"val1")),
			],
		);
	}

	#[test]
	fn next_change_works() {
		let mut changeset = OverlayedChangeSet::default();

		changeset.set(b"key0".to_vec(), Some(b"val0".to_vec()));
		changeset.set(b"key1".to_vec(), Some(b"val1".to_vec()));
		changeset.set(b"key2".to_vec(), Some(b"val2".to_vec()));

		changeset.start_transaction();

		changeset.set(b"key3".to_vec(), Some(b"val3".to_vec()));
		changeset.set(b"key4".to_vec(), Some(b"val4".to_vec()));
		changeset.set(b"key11".to_vec(), Some(b"val11".to_vec()));

		assert_eq!(changeset.changes_after(b"key0").next().unwrap().0, b"key1");
		assert_eq!(
			changeset.changes_after(b"key0").next().unwrap().1.value(),
			Some(&b"val1".to_vec())
		);
		assert_eq!(changeset.changes_after(b"key1").next().unwrap().0, b"key11");
		assert_eq!(
			changeset.changes_after(b"key1").next().unwrap().1.value(),
			Some(&b"val11".to_vec())
		);
		assert_eq!(changeset.changes_after(b"key11").next().unwrap().0, b"key2");
		assert_eq!(
			changeset.changes_after(b"key11").next().unwrap().1.value(),
			Some(&b"val2".to_vec())
		);
		assert_eq!(changeset.changes_after(b"key2").next().unwrap().0, b"key3");
		assert_eq!(
			changeset.changes_after(b"key2").next().unwrap().1.value(),
			Some(&b"val3".to_vec())
		);
		assert_eq!(changeset.changes_after(b"key3").next().unwrap().0, b"key4");
		assert_eq!(
			changeset.changes_after(b"key3").next().unwrap().1.value(),
			Some(&b"val4".to_vec())
		);
		assert_eq!(changeset.changes_after(b"key4").next(), None);

		changeset.rollback_transaction().unwrap();

		assert_eq!(changeset.changes_after(b"key0").next().unwrap().0, b"key1");
		assert_eq!(
			changeset.changes_after(b"key0").next().unwrap().1.value(),
			Some(&b"val1".to_vec())
		);
		assert_eq!(changeset.changes_after(b"key1").next().unwrap().0, b"key2");
		assert_eq!(
			changeset.changes_after(b"key1").next().unwrap().1.value(),
			Some(&b"val2".to_vec())
		);
		assert_eq!(changeset.changes_after(b"key11").next().unwrap().0, b"key2");
		assert_eq!(
			changeset.changes_after(b"key11").next().unwrap().1.value(),
			Some(&b"val2".to_vec())
		);
		assert_eq!(changeset.changes_after(b"key2").next(), None);
		assert_eq!(changeset.changes_after(b"key3").next(), None);
		assert_eq!(changeset.changes_after(b"key4").next(), None);
	}

	#[test]
	fn no_open_tx_commit_errors() {
		let mut changeset = OverlayedChangeSet::default();
		assert_eq!(changeset.transaction_depth(), 0);
		assert_eq!(changeset.commit_transaction(), Err(NoOpenTransaction));
	}

	#[test]
	fn no_open_tx_rollback_errors() {
		let mut changeset = OverlayedChangeSet::default();
		assert_eq!(changeset.transaction_depth(), 0);
		assert_eq!(changeset.rollback_transaction(), Err(NoOpenTransaction));
	}

	#[test]
	fn unbalanced_transactions_errors() {
		let mut changeset = OverlayedChangeSet::default();
		changeset.start_transaction();
		changeset.commit_transaction().unwrap();
		assert_eq!(changeset.commit_transaction(), Err(NoOpenTransaction));
	}

	#[test]
	fn runtime_cannot_close_client_tx() {
		let mut context = OverlayedContext::default();
		context.start_transaction();
		context.enter_runtime().unwrap();
		context.start_transaction();
		context.commit_transaction().unwrap();
		assert_eq!(context.rollback_transaction(), Err(NoOpenTransaction));
	}

	#[test]
	fn enter_exit_runtime_fails_when_already_in_requested_mode() {
		let mut context = OverlayedContext::default();

		assert_eq!(context.exit_runtime(), Err(NotInRuntime));
		assert_eq!(context.enter_runtime(), Ok(()));
		assert_eq!(context.enter_runtime(), Err(AlreadyInRuntime));
		assert_eq!(context.exit_runtime(), Ok(0));
		assert_eq!(context.exit_runtime(), Err(NotInRuntime));
	}
}
