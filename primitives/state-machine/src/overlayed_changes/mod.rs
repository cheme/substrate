// This file is part of Substrate.

// Copyright (C) 2017-2022 Parity Technologies (UK) Ltd.
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
// limitations under the License.

//! The overlayed changes to state.

mod changeset;
mod filters;
mod local_scope;
mod loggers;
mod markers;
mod offchain;

use self::changeset::OverlayedChangeSet;
use crate::{
	backend::{AsyncBackendAdapterRef, AsyncBackendNonSend, Backend},
	stats::StateMachineStats,
};
pub use offchain::OffchainOverlayedChanges;
use sp_std::{
	any::{Any, TypeId},
	boxed::Box,
	fmt::Debug,
	vec::Vec,
};

use crate::DefaultError;
use codec::{Decode, Encode};
use hash_db::Hasher;
use sp_core::{
	offchain::OffchainOverlayedChange,
	storage::{well_known_keys::EXTRINSIC_INDEX, ChildInfo, StateVersion},
};
use sp_externalities::{
	AccessDeclarations, AccessLog, Change, DeclarationFailureHandling, Extension, Extensions,
	TaskId, WorkerDeclaration, WorkerResult, WorkerState, WorkerType,
};
#[cfg(not(feature = "std"))]
use sp_std::collections::btree_map::{BTreeMap as Map, Entry as MapEntry};
use sp_std::collections::btree_set::BTreeSet;
#[cfg(feature = "std")]
use std::collections::{hash_map::Entry as MapEntry, HashMap as Map};

pub use self::changeset::{AlreadyInRuntime, NoOpenTransaction, NotInRuntime, OverlayedValue};

/// Changes that are made outside of extrinsics are marked with this index;
pub const NO_EXTRINSIC_INDEX: u32 = 0xffffffff;

/// Storage key.
pub type StorageKey = Vec<u8>;

/// Storage value.
pub type StorageValue = Vec<u8>;

/// In memory array of storage values.
pub type StorageCollection = Vec<(StorageKey, Option<StorageValue>)>;

/// In memory arrays of storage values for multiple child tries.
pub type ChildStorageCollection = Vec<(StorageKey, StorageCollection)>;

/// In memory array of storage values.
pub type OffchainChangesCollection = Vec<((Vec<u8>, Vec<u8>), OffchainOverlayedChange)>;

/// Keep trace of extrinsics index for a modified value.
#[derive(Debug, Default, Eq, PartialEq, Clone)]
pub struct Extrinsics(Vec<u32>);

impl Extrinsics {
	/// Extracts extrinsics into a `BTreeSets`.
	fn copy_extrinsics_into(&self, dest: &mut BTreeSet<u32>) {
		dest.extend(self.0.iter())
	}

	/// Add an extrinsics.
	fn insert(&mut self, ext: u32) {
		if Some(&ext) != self.0.last() {
			self.0.push(ext);
		}
	}

	/// Extend `self` with `other`.
	fn extend(&mut self, other: Self) {
		self.0.extend(other.0.into_iter());
	}
}

/// The set of changes that are overlaid onto the backend.
///
/// It allows changes to be modified using nestable transactions.
#[derive(Debug, Default, Clone)]
pub struct OverlayedChanges {
	/// Top level storage changes.
	top: OverlayedChangeSet,
	/// Child storage changes. The map key is the child storage key without the common prefix.
	children: Map<StorageKey, (OverlayedChangeSet, ChildInfo)>,
	/// Offchain related changes.
	offchain: OffchainOverlayedChanges,
	/// Transaction index changes,
	transaction_index_ops: Vec<IndexOperation>,
	/// True if extrinsics stats must be collected.
	collect_extrinsics: bool,
	/// Collect statistic on this execution.
	stats: StateMachineStats,
	/// Marker keeping trace of worker async externalities in use.
	markers: markers::Markers,
	/// Filters over some key and prefix accesses.
	filters: filters::Filters,
	/// Logger for optimistic workers.
	optimistic_logger: loggers::AccessLogger,
	/// Do we log accesses.
	log_accesses: bool,
	/// If concurrent child worker write.
	need_resolve: bool,
	/// Local write scope.
	write_local_scope: local_scope::WriteLocalScope,
}

impl OverlayedChanges {
	/// Activate access logging
	pub fn log_accesses(&mut self) {
		self.log_accesses = true;
	}

	/// Does delta need resolve.
	pub fn need_resolve(&self) -> bool {
		self.need_resolve
	}

	/// An overlay that can be use for child worker.
	/// It contains the overlay changes and ensure
	/// we do not commit or rollback them.
	/// Return None if cannot be spawn from parent.
	pub fn child_worker_overlay(
		&mut self,
		worker_id: TaskId,
		mut declaration: WorkerDeclaration,
	) -> Option<Self> {
		let mut need_resolve = false;
		if let Some(accesses) = declaration.accesses.clone() {
			if accesses.write_only.no_access() && accesses.read_write.no_access() {
				need_resolve = true;
			}
			if !self.declare_child_worker(worker_id, accesses, declaration.failure_handling) {
				return None
			}
		}
		let mut result = OverlayedChanges::default();
		result.need_resolve = need_resolve;
		result.top = self.top.clone();
		result.children = self.children.clone();
		result.stats = Default::default();
		result.start_transaction();
		let transaction_offset = result.top.transaction_depth();
		result.markers = markers::Markers::from_start_depth(transaction_offset);

		// Force log if it is in parent.
		if self.log_accesses {
			declaration.log_accesses = true;
		}
		result.set_worker_declaration(declaration);

		Some(result)
	}
}

/// Transaction index operation.
#[derive(Debug, Clone)]
pub enum IndexOperation {
	/// Insert transaction into index.
	Insert {
		/// Extrinsic index in the current block.
		extrinsic: u32,
		/// Data content hash.
		hash: Vec<u8>,
		/// Indexed data size.
		size: u32,
	},
	/// Renew existing transaction storage.
	Renew {
		/// Extrinsic index in the current block.
		extrinsic: u32,
		/// Referenced index hash.
		hash: Vec<u8>,
	},
}

/// A storage changes structure that can be generated by the data collected in [`OverlayedChanges`].
///
/// This contains all the changes to the storage and transactions to apply theses changes to the
/// backend.
pub struct StorageChanges<Transaction, H: Hasher> {
	/// All changes to the main storage.
	///
	/// A value of `None` means that it was deleted.
	pub main_storage_changes: StorageCollection,
	/// All changes to the child storages.
	pub child_storage_changes: ChildStorageCollection,
	/// Offchain state changes to write to the offchain database.
	pub offchain_storage_changes: OffchainChangesCollection,
	/// A transaction for the backend that contains all changes from
	/// [`main_storage_changes`](StorageChanges::main_storage_changes) and from
	/// [`child_storage_changes`](StorageChanges::child_storage_changes).
	/// [`offchain_storage_changes`](StorageChanges::offchain_storage_changes).
	pub transaction: Transaction,
	/// The storage root after applying the transaction.
	pub transaction_storage_root: H::Out,
	/// Changes to the transaction index,
	#[cfg(feature = "std")]
	pub transaction_index_changes: Vec<IndexOperation>,
}

#[cfg(feature = "std")]
impl<Transaction, H: Hasher> StorageChanges<Transaction, H> {
	/// Deconstruct into the inner values
	pub fn into_inner(
		self,
	) -> (
		StorageCollection,
		ChildStorageCollection,
		OffchainChangesCollection,
		Transaction,
		H::Out,
		Vec<IndexOperation>,
	) {
		(
			self.main_storage_changes,
			self.child_storage_changes,
			self.offchain_storage_changes,
			self.transaction,
			self.transaction_storage_root,
			self.transaction_index_changes,
		)
	}
}

/// Storage transactions are calculated as part of the `storage_root`.
/// These transactions can be reused for importing the block into the
/// storage. So, we cache them to not require a recomputation of those transactions.
pub struct StorageTransactionCache<Transaction, H: Hasher> {
	/// Contains the changes for the main and the child storages as one transaction.
	pub(crate) transaction: Option<Transaction>,
	/// The storage root after applying the transaction.
	pub(crate) transaction_storage_root: Option<H::Out>,
}

impl<Transaction, H: Hasher> StorageTransactionCache<Transaction, H> {
	/// Reset the cached transactions.
	pub fn reset(&mut self) {
		*self = Self::default();
	}
}

impl<Transaction, H: Hasher> Default for StorageTransactionCache<Transaction, H> {
	fn default() -> Self {
		Self { transaction: None, transaction_storage_root: None }
	}
}

impl<Transaction: Default, H: Hasher> Default for StorageChanges<Transaction, H> {
	fn default() -> Self {
		Self {
			main_storage_changes: Default::default(),
			child_storage_changes: Default::default(),
			offchain_storage_changes: Default::default(),
			transaction: Default::default(),
			transaction_storage_root: Default::default(),
			#[cfg(feature = "std")]
			transaction_index_changes: Default::default(),
		}
	}
}

/*
enum OverlayResult<'a> {
	Unchanged,
	Remove,
	NewValue(&'a [u8]),
	Appended(Option<&'a [u8]> , &'a [Vec<u8>]), // maybe &mut ref to fill it on value access (but then become a NewValue so )
	IncCounter(Option<&'a [u8]>, usize),
}
*/

impl OverlayedChanges {
	/// Whether no changes are contained in the top nor in any of the child changes.
	pub fn is_empty(&self) -> bool {
		self.top.is_empty() && self.children.is_empty()
	}

	/// Ask to collect/not to collect extrinsics indices where key(s) has been changed.
	pub fn set_collect_extrinsics(&mut self, collect_extrinsics: bool) {
		self.collect_extrinsics = collect_extrinsics;
	}

	/// Returns a double-Option: None if the key is unknown (i.e. and the query should be referred
	/// to the backend); Some(None) if the key has been deleted. Some(Some(...)) for a key whose
	/// value has been set.
	/// TODO Option<&mut Changes>? or &mut OverlayResult (would mean removing Unchanged variant).
	/// TODO passing backend is ill design: just return enum
	pub fn storage(
		&mut self,
		key: &[u8],
		backend: &impl AsyncBackendNonSend,
	) -> Option<Option<&[u8]>> {
		self.filters.guard_read(None, key);
		self.optimistic_logger.log_read(None, key);
		let stats = &mut self.stats;
		self.top.get(key).map(|x| {
			if x.need_resolve() {
				// guard and log already call
				if let Some(aggregate) =
					change_to_change_set(None, key, &mut x.transactions[..], backend)
				{
					x.transactions.last_mut().map(|e| e.value = Change::Write(aggregate));
				}
			}
			let value = x.value_from_resolved();
			let size_read = value.map(|x| x.len() as u64).unwrap_or(0);
			stats.tally_read_modified(size_read);
			value.map(AsRef::as_ref)
		})
	}

	/// Returns a double-Option: None if the key is unknown (i.e. and the query should be referred
	/// to the backend); Some(None) if the key has been deleted. Some(Some(...)) for a key whose
	/// value has been set.
	pub fn child_storage(
		&mut self,
		child_info: &ChildInfo,
		key: &[u8],
		backend: &impl AsyncBackendNonSend,
	) -> Option<Option<&[u8]>> {
		self.filters.guard_read(Some(child_info), key);
		self.optimistic_logger.log_read(Some(child_info), key);
		let map = self.children.get_mut(child_info.storage_key())?;

		let value = map.0.get(key)?;
		if value.need_resolve() {
			// gard and log already call
			if let Some(aggregate) =
				change_to_change_set(Some(child_info), key, &mut value.transactions[..], backend)
			{
				value.transactions.last_mut().map(|e| e.value = Change::Write(aggregate));
			}
		}
		let value = value.value_from_resolved();
		let size_read = value.map(|x| x.len() as u64).unwrap_or(0);
		self.stats.tally_read_modified(size_read);
		Some(value.map(AsRef::as_ref))
	}

	/// Set a new value for the specified key.
	///
	/// Can be rolled back or committed when called inside a transaction.
	pub(crate) fn set_storage(&mut self, key: StorageKey, val: Option<StorageValue>) {
		self.filters.guard_write(None, key.as_slice());
		self.optimistic_logger.log_write(None, key.as_slice());
		let size_write = val.as_ref().map(|x| x.len() as u64).unwrap_or(0);
		self.stats.tally_write_overlay(size_write);
		let extrinsic_index = self.extrinsic_index();
		self.top.set(key, val.into(), extrinsic_index);
	}

	/// Append a value to encoded storage.
	pub(crate) fn append_storage(&mut self, key: StorageKey, val: StorageValue) {
		self.filters.guard_write(None, key.as_slice());
		self.optimistic_logger.log_write(None, key.as_slice());
		let extrinsic_index = self.extrinsic_index();
		let size_write = val.len() as u64;
		self.stats.tally_write_overlay(size_write);
		self.top.append_storage(key, val, extrinsic_index);
	}

	pub(crate) fn append_child_storage(
		&mut self,
		child_info: &ChildInfo,
		key: StorageKey,
		val: StorageValue,
	) {
		self.filters.guard_write(Some(child_info), key.as_slice());
		self.optimistic_logger.log_write(Some(child_info), key.as_slice());
		let extrinsic_index = self.extrinsic_index();
		let size_write = val.len() as u64;
		self.stats.tally_write_overlay(size_write);
		let storage_key = child_info.storage_key().to_vec();
		let top = &self.top;
		let (changeset, info) = self
			.children
			.entry(storage_key)
			.or_insert_with(|| (top.spawn_child(), child_info.clone()));
		let updatable = info.try_update(child_info);
		debug_assert!(updatable);
		changeset.append_storage(key, val, extrinsic_index);
	}

	/// Append a value to encoded storage.
	pub(crate) fn increment_storage(&mut self, key: StorageKey, nb: u32) {
		self.filters.guard_write(None, key.as_slice());
		self.optimistic_logger.log_write(None, key.as_slice());
		let extrinsic_index = self.extrinsic_index();
		let size_write = 2;
		self.stats.tally_write_overlay(size_write);
		self.top.inc_counter(key, nb, extrinsic_index);
	}

	pub(crate) fn increment_child_storage(
		&mut self,
		child_info: &ChildInfo,
		key: StorageKey,
		nb: u32,
	) {
		self.filters.guard_write(Some(child_info), key.as_slice());
		self.optimistic_logger.log_write(Some(child_info), key.as_slice());
		let extrinsic_index = self.extrinsic_index();
		let size_write = 2 as u64;
		self.stats.tally_write_overlay(size_write);
		let storage_key = child_info.storage_key().to_vec();
		let top = &self.top;
		let (changeset, info) = self
			.children
			.entry(storage_key)
			.or_insert_with(|| (top.spawn_child(), child_info.clone()));
		let updatable = info.try_update(child_info);
		debug_assert!(updatable);
		changeset.inc_counter(key, nb, extrinsic_index);
	}

	/// Set a new value for the specified key and child.
	///
	/// `None` can be used to delete a value specified by the given key.
	///
	/// Can be rolled back or committed when called inside a transaction.
	pub(crate) fn set_child_storage(
		&mut self,
		child_info: &ChildInfo,
		key: StorageKey,
		val: Option<StorageValue>,
	) {
		self.filters.guard_write(Some(child_info), key.as_slice());
		self.optimistic_logger.log_write(Some(child_info), key.as_slice());
		let extrinsic_index = self.extrinsic_index();
		let size_write = val.as_ref().map(|x| x.len() as u64).unwrap_or(0);
		self.stats.tally_write_overlay(size_write);
		let storage_key = child_info.storage_key().to_vec();
		let top = &self.top;
		let (changeset, info) = self
			.children
			.entry(storage_key)
			.or_insert_with(|| (top.spawn_child(), child_info.clone()));
		let updatable = info.try_update(child_info);
		debug_assert!(updatable);
		changeset.set(key, val.into(), extrinsic_index);
	}

	/// Clear child storage of given storage key.
	///
	/// Can be rolled back or committed when called inside a transaction.
	pub(crate) fn clear_child_storage(&mut self, child_info: &ChildInfo) -> u32 {
		// clear is reading value, this is not idealy, still guarding
		// read_write (would not be needed with special clear prefix
		// optimization).
		// This apply to all clear.
		self.filters.guard_write_interval(Some(child_info), &[], None);
		self.optimistic_logger.log_write_prefix(Some(child_info), &[]);

		let extrinsic_index = self.extrinsic_index();
		let storage_key = child_info.storage_key().to_vec();
		let top = &self.top;
		let (changeset, info) = self
			.children
			.entry(storage_key)
			.or_insert_with(|| (top.spawn_child(), child_info.clone()));
		let updatable = info.try_update(child_info);
		debug_assert!(updatable);
		changeset.clear_where(|_, _| true, extrinsic_index)
	}

	/// Removes all key-value pairs which keys share the given prefix.
	///
	/// Can be rolled back or committed when called inside a transaction.
	pub(crate) fn clear_prefix(&mut self, prefix: &[u8]) -> u32 {
		self.filters.guard_write_interval(
			None,
			prefix,
			end_prefix(prefix).as_ref().map(Vec::as_slice),
		);
		self.optimistic_logger.log_write_prefix(None, prefix);
		let extrinsic_index = self.extrinsic_index();
		self.top.clear_where(|key, _| key.starts_with(prefix), extrinsic_index)
	}

	/// Removes all key-value pairs which keys share the given prefix.
	///
	/// Can be rolled back or committed when called inside a transaction
	pub(crate) fn clear_child_prefix(&mut self, child_info: &ChildInfo, prefix: &[u8]) -> u32 {
		self.filters.guard_write_interval(
			Some(child_info),
			prefix,
			end_prefix(prefix).as_ref().map(Vec::as_slice),
		);
		self.optimistic_logger.log_write_prefix(Some(child_info), prefix);
		let extrinsic_index = self.extrinsic_index();
		let storage_key = child_info.storage_key().to_vec();
		let top = &self.top;
		let (changeset, info) = self
			.children
			.entry(storage_key)
			.or_insert_with(|| (top.spawn_child(), child_info.clone()));
		let updatable = info.try_update(child_info);
		debug_assert!(updatable);
		changeset.clear_where(|key, _| key.starts_with(prefix), extrinsic_index)
	}

	/// Returns the current nesting depth of the transaction stack.
	///
	/// A value of zero means that no transaction is open and changes are committed on write.
	pub fn transaction_depth(&self) -> usize {
		// The top changeset and all child changesets transact in lockstep and are
		// therefore always at the same transaction depth.
		self.top.transaction_depth()
	}

	/// Set access declaration in the parent worker.
	///
	/// For worker declaration it also guard child declaration on parent, resulting in failure when
	/// child declaration allows more than current parent allowed access.
	///
	/// Return false if the declaration is incompatible with current parent worker accesses.
	/// TODO check if we can &WorkerDeclaration
	pub fn declare_child_worker(
		&mut self,
		child_marker: TaskId,
		filters: AccessDeclarations,
		failure: DeclarationFailureHandling,
	) -> bool {
		if !self.filters.guard_declare_child_filter_read(&filters.read_only) {
			return false
		}
		if !self.filters.guard_declare_child_filter_write(&filters.write_only) {
			return false
		}
		if !self.filters.guard_declare_child_filter_read(&filters.read_write) {
			return false
		}
		if !self.filters.guard_declare_child_filter_write(&filters.read_write) {
			return false
		}
		self.filters.set_failure_handler(Some(child_marker), failure);
		self.filters.add_change(filters.clone(), child_marker);
		self.filters.forbid_all(filters.read_write, child_marker);
		self.filters.forbid_writes(filters.read_only, child_marker);
		self.filters.forbid_reads(filters.write_only, child_marker);
		self.markers.set_marker(child_marker);
		true
	}

	/// Set access declaration in the child worker.
	pub fn set_worker_declaration(&mut self, declaration: WorkerDeclaration) {
		self.write_local_scope.set_write_local_scope(declaration.write_local_scope);
		if declaration.log_accesses {
			self.optimistic_logger.log_reads_against();
			self.optimistic_logger.log_writes_against();
		}
		if let Some(filters) = declaration.accesses {
			self.filters.set_failure_handler(None, declaration.failure_handling);
			self.filters.allow_reads(filters.read_only);
			self.filters.allow_writes(filters.write_only);
			self.filters.allow_all(filters.read_write);
		}
	}

	/// Check if worker result is compatible with changes
	/// that happens in parent worker state.
	///
	/// Return `None` when the worker execution should be invalidated.
	pub fn resolve_worker_result(&mut self, result: WorkerResult) -> Option<Vec<u8>> {
		if !self.filters.on_worker_result(&result) ||
			!self.optimistic_logger.on_worker_result(&result) ||
			!self.markers.on_worker_result(&result)
		{
			return None
		}

		match (result.state, result.result) {
			(WorkerState::Valid, Some(res)) | (WorkerState::CallAt, Some(res)) => {
				if let Some(delta) = result.state_delta {
					self.inject_worker_delta(delta);
				}
				Some(res)
			},
			(WorkerState::Invalid, _) => None,
			(WorkerState::RuntimePanic, _) => {
				panic!("Runtime panic from a worker.")
			},
			(WorkerState::HardPanic, _) => {
				panic!("Panic running a worker.")
			},
			_ => unimplemented!("TODO wrong state and content"),
		}
	}

	/// A worker was dissmissed.
	///
	/// Update internal state relative to this worker.
	pub fn dismiss_worker(&mut self, id: TaskId) {
		self.filters.remove_worker(id);
		self.markers.remove_worker(id);
	}

	/// Start a new nested transaction.
	///
	/// This allows to either commit or roll back all changes that where made while this
	/// transaction was open. Any transaction must be closed by either `rollback_transaction` or
	/// `commit_transaction` before this overlay can be converted into storage changes.
	///
	/// Changes made without any open transaction are committed immediately.
	pub fn start_transaction(&mut self) {
		self.markers.start_transaction();
		self.top.start_transaction();
		for (_, (changeset, _)) in self.children.iter_mut() {
			changeset.start_transaction();
		}
		self.offchain.overlay_mut().start_transaction();
	}

	/// Rollback the last transaction started by `start_transaction`.
	///
	/// Any changes made during that transaction are discarded. Returns an error if
	/// there is no open transaction that can be rolled back.
	#[must_use]
	pub fn rollback_transaction(&mut self) -> Result<Vec<TaskId>, NoOpenTransaction> {
		let to_kill = self.markers.rollback_transaction();
		self.top.rollback_transaction()?;
		retain_map(&mut self.children, |_, (changeset, _)| {
			changeset
				.rollback_transaction()
				.expect("Top and children changesets are started in lockstep; qed");
			!changeset.is_empty()
		});
		self.offchain
			.overlay_mut()
			.rollback_transaction()
			.expect("Top and offchain changesets are started in lockstep; qed");
		Ok(to_kill)
	}

	/// Commit the last transaction started by `start_transaction`.
	///
	/// Any changes made during that transaction are committed. Returns an error if there
	/// is no open transaction that can be committed.
	#[must_use]
	pub fn commit_transaction(&mut self) -> Result<Vec<TaskId>, NoOpenTransaction> {
		let to_kill = self.markers.commit_transaction();
		self.top.commit_transaction(apply_commit_change)?;
		for (_, (changeset, _)) in self.children.iter_mut() {
			changeset
				.commit_transaction(apply_commit_change)
				.expect("Top and children changesets are started in lockstep; qed");
		}
		self.offchain
			.overlay_mut()
			.commit_transaction(apply_commit_default)
			.expect("Top and offchain changesets are started in lockstep; qed");
		Ok(to_kill)
	}

	/// Call this before transfering control to the runtime.
	///
	/// This protects all existing transactions from being removed by the runtime.
	/// Calling this while already inside the runtime will return an error.
	pub fn enter_runtime(&mut self) -> Result<(), AlreadyInRuntime> {
		self.top.enter_runtime()?;
		for (_, (changeset, _)) in self.children.iter_mut() {
			changeset
				.enter_runtime()
				.expect("Top and children changesets are entering runtime in lockstep; qed")
		}
		self.offchain
			.overlay_mut()
			.enter_runtime()
			.expect("Top and offchain changesets are started in lockstep; qed");
		Ok(())
	}

	/// Call this when control returns from the runtime.
	///
	/// This commits all dangling transaction left open by the runtime.
	/// Calling this while outside the runtime will return an error.
	pub fn exit_runtime(&mut self) -> Result<(), NotInRuntime> {
		self.top.exit_runtime()?;
		for (_, (changeset, _)) in self.children.iter_mut() {
			changeset
				.exit_runtime()
				.expect("Top and children changesets are entering runtime in lockstep; qed");
		}
		self.offchain
			.overlay_mut()
			.exit_runtime()
			.expect("Top and offchain changesets are started in lockstep; qed");
		Ok(())
	}

	/// Consume all changes up to a given transaction depth (top + children)
	/// and return them.
	///
	/// Panics:
	/// Panics if `transaction_depth() > initial_depth`
	fn drain_committed_for(
		&mut self,
		initial_depth: usize,
	) -> (
		impl Iterator<Item = (StorageKey, Change)>,
		impl Iterator<Item = (StorageKey, (impl Iterator<Item = (StorageKey, Change)>, ChildInfo))>,
	) {
		use sp_std::mem::take;
		(
			take(&mut self.top).drain_commited_for(initial_depth),
			take(&mut self.children).into_iter().map(move |(key, (val, info))| {
				(key, (val.drain_commited_for(initial_depth), info))
			}),
		)
	}

	/// Consume all changes (top + children) and return them.
	///
	/// After calling this function no more changes are contained in this changeset.
	///
	/// Panics:
	/// Panics if `transaction_depth() > 0`
	fn drain_committed<'a, H: Hasher>(
		&mut self,
		backend: &'a impl crate::Backend<H>,
	) -> (
		impl Iterator<Item = (StorageKey, Option<StorageValue>)> + 'a,
		impl Iterator<
				Item = (
					StorageKey,
					(impl Iterator<Item = (StorageKey, Option<StorageValue>)> + 'a, ChildInfo),
				),
			> + 'a,
	) {
		use sp_std::mem::take;
		(
			take(&mut self.top).drain().map(move |(k, mut change)| {
				let change = change_to_change_set(
					None,
					k.as_slice(),
					&mut change.transactions[..],
					&AsyncBackendAdapterRef::new(backend),
				);
				(k, change)
			}),
			take(&mut self.children).into_iter().map(move |(key, (val, info))| {
				let info_iter = info.clone();
				(
					key,
					(
						val.drain().map(move |(k, mut change)| {
							let change = change_to_change_set(
								Some(&info_iter),
								k.as_slice(),
								&mut change.transactions[..],
								&AsyncBackendAdapterRef::new(backend),
							);
							(k, change)
						}),
						info,
					),
				)
			}),
		)
	}

	/// Consume all changes (top + children) and return them.
	///
	/// After calling this function no more changes are contained in this changeset.
	///
	/// Panics:
	/// Panics if `transaction_depth() > 0`
	pub fn offchain_drain_committed(
		&mut self,
	) -> impl Iterator<Item = ((StorageKey, StorageKey), OffchainOverlayedChange)> {
		self.offchain.drain()
	}

	/// Get an iterator over all child changes as seen by the current transaction.
	pub fn children(
		&mut self,
	) -> impl Iterator<Item = (impl Iterator<Item = (&StorageKey, &mut OverlayedValue)>, &ChildInfo)>
	{
		self.children.iter_mut().map(|(_, v)| (v.0.changes(), &v.1))
	}

	/// Get an iterator over all child changes as seen by the current transaction.
	pub fn children_resolved<'a>(
		&'a mut self,
		backend: &'a impl AsyncBackendNonSend,
	) -> impl Iterator<Item = (impl Iterator<Item = (&StorageKey, &mut OverlayedValue)>, &ChildInfo)>
	{
		self.children
			.iter_mut()
			.map(move |(_, v)| (v.0.resolved_changes(Some(&v.1), backend), &v.1))
	}

	/// Get an iterator over all top changes as been by the current transaction.
	pub fn changes(&mut self) -> impl Iterator<Item = (&StorageKey, &mut OverlayedValue)> {
		self.top.changes()
	}

	/// Get an iterator over all top changes as been by the current transaction.
	///
	/// Does resolve appendable values. TODO not sure why returning &mut then.
	pub fn resolved_changes<'a>(
		&'a mut self,
		backend: &'a impl AsyncBackendNonSend,
	) -> impl Iterator<Item = (&StorageKey, &mut OverlayedValue)> + 'a {
		self.top.resolved_changes(None, backend)
	}

	/// Get an optional iterator over all child changes stored under the supplied key.
	pub fn child_changes(
		&mut self,
		key: &[u8],
	) -> Option<(impl Iterator<Item = (&StorageKey, &mut OverlayedValue)>, &ChildInfo)> {
		self.children.get_mut(key).map(|(overlay, info)| (overlay.changes(), &*info))
	}

	/// Get an optional iterator over all child changes stored under the supplied key.
	///
	/// Does resolve appendable values.
	pub fn resolved_child_changes<'a>(
		&'a mut self,
		key: &[u8],
		backend: &'a impl AsyncBackendNonSend,
	) -> Option<(impl Iterator<Item = (&StorageKey, &mut OverlayedValue)>, &ChildInfo)> {
		self.children.get_mut(key).map(move |(overlay, info)| {
			let iter = overlay.resolved_changes(Some(info), backend);
			(iter, &*info)
		})
	}

	/// Get an list of all index operations.
	pub fn transaction_index_ops(&self) -> &[IndexOperation] {
		&self.transaction_index_ops
	}

	/// Convert this instance with all changes into a [`StorageChanges`] instance.
	#[cfg(feature = "std")]
	pub fn into_storage_changes<B: Backend<H>, H: Hasher + 'static>(
		mut self,
		backend: &B,
		mut cache: StorageTransactionCache<B::Transaction, H>,
		state_version: StateVersion,
	) -> Result<(StorageChanges<B::Transaction, H>, Option<AccessLog>), DefaultError>
	where
		H::Out: Ord + Encode + 'static,
	{
		let logs = self.optimistic_logger.extract_parent_log();
		let r = self.drain_storage_changes(backend, &mut cache, state_version)?;
		let _ = self.optimistic_logger.extract_parent_log();
		Ok((r, logs))
	}

	#[cfg(feature = "std")]
	pub fn extract_logs(&mut self) -> Option<AccessLog> {
		self.optimistic_logger.extract_parent_log()
	}

	/// Drain all changes into a [`StorageChanges`] instance. Leave empty overlay in place.
	pub fn drain_storage_changes<B: Backend<H>, H: Hasher + 'static>(
		&mut self,
		backend: &B,
		cache: &mut StorageTransactionCache<B::Transaction, H>,
		state_version: StateVersion,
	) -> Result<StorageChanges<B::Transaction, H>, DefaultError>
	where
		H::Out: Ord + Encode + 'static,
	{
		// If the transaction does not exist, we generate it.
		if cache.transaction.is_none() {
			self.storage_root(backend, cache, state_version);
		}

		let (transaction, transaction_storage_root) = cache
			.transaction
			.take()
			.and_then(|t| cache.transaction_storage_root.take().map(|tr| (t, tr)))
			.expect("Transaction was be generated as part of `storage_root`; qed");

		let (main_storage_changes, child_storage_changes) = self.drain_committed(backend);
		let offchain_storage_changes = self.offchain_drain_committed().collect();

		#[cfg(feature = "std")]
		let transaction_index_changes = std::mem::take(&mut self.transaction_index_ops);

		Ok(StorageChanges {
			main_storage_changes: main_storage_changes.collect(),
			child_storage_changes: child_storage_changes
				.map(|(sk, it)| (sk, it.0.collect()))
				.collect(),
			offchain_storage_changes,
			transaction,
			transaction_storage_root,
			#[cfg(feature = "std")]
			transaction_index_changes,
		})
	}

	/// Inserts storage entry responsible for current extrinsic index.
	#[cfg(test)]
	pub(crate) fn set_extrinsic_index(&mut self, extrinsic_index: u32) {
		self.top
			.set(EXTRINSIC_INDEX.to_vec(), Change::Write(extrinsic_index.encode()), None);
	}

	/// Returns current extrinsic index to use in changes trie construction.
	/// None is returned if it is not set or changes trie config is not set.
	/// Persistent value (from the backend) can be ignored because runtime must
	/// set this index before first and unset after last extrinsic is executed.
	/// Changes that are made outside of extrinsics, are marked with
	/// `NO_EXTRINSIC_INDEX` index.
	fn extrinsic_index(&mut self) -> Option<u32> {
		match self.collect_extrinsics {
			true => Some(
				self.storage(EXTRINSIC_INDEX, &()) // TODO here no backend for extrinsic index, but strictly will require just
					// independant storage (see local)
					.and_then(|idx| idx.and_then(|idx| Decode::decode(&mut &*idx).ok()))
					.unwrap_or(NO_EXTRINSIC_INDEX),
			),
			false => None,
		}
	}

	/// Generate the storage root using `backend` and all changes
	/// as seen by the current transaction.
	///
	/// Returns the storage root and caches storage transaction in the given `cache`.
	pub fn storage_root<H: Hasher, B: Backend<H>>(
		&mut self,
		backend: &B,
		cache: &mut StorageTransactionCache<B::Transaction, H>,
		state_version: StateVersion,
	) -> H::Out
	where
		H::Out: Ord + Encode,
	{
		self.filters.guard_read_all();
		self.optimistic_logger.log_read_all();

		let abackend = AsyncBackendAdapterRef::new(backend);
		let delta = self
			.top
			.resolved_changes(None, &abackend)
			.map(|(k, v)| (&k[..], v.value_from_resolved().map(|v| &v[..])));
		let child_delta = self
			.children
			.iter_mut()
			.map(|(_, v)| (v.0.resolved_changes(Some(&v.1), &abackend), &v.1))
			.map(|(changes, info)| {
				(info, changes.map(|(k, v)| (&k[..], v.value_from_resolved().map(|v| &v[..]))))
			});

		let (root, transaction) = backend.full_storage_root(delta, child_delta, state_version);

		cache.transaction = Some(transaction);
		cache.transaction_storage_root = Some(root);

		root
	}

	/// Log backend interval accessed.
	pub fn log_backend_interval(
		&mut self,
		child_info: Option<&ChildInfo>,
		key: &[u8],
		backend_accessed: Option<&Vec<u8>>,
	) {
		self.filters
			.guard_read_interval(child_info, key, backend_accessed.map(Vec::as_slice));
		self.optimistic_logger.log_read_interval(
			child_info,
			key,
			backend_accessed.map(Vec::as_slice),
		);
	}

	/// Returns an iterator over the keys (in lexicographic order) following `key` (excluding `key`)
	/// alongside its value.
	pub fn iter_after(&mut self, key: &[u8]) -> impl Iterator<Item = (&[u8], &mut OverlayedValue)> {
		self.top.changes_after(key)
	}

	/// Returns an iterator over the keys (in lexicographic order) following `key` (excluding `key`)
	/// alongside its value for the given `storage_key` child.
	pub fn child_iter_after(
		&mut self,
		storage_key: &[u8],
		key: &[u8],
	) -> impl Iterator<Item = (&[u8], &mut OverlayedValue)> {
		self.children
			.get_mut(storage_key)
			.map(|(overlay, _)| overlay.changes_after(key))
			.into_iter()
			.flatten()
	}

	/// Read only access ot offchain overlay.
	pub fn offchain(&self) -> &OffchainOverlayedChanges {
		&self.offchain
	}

	/// Extract logs from the overlay.
	pub fn extract_access_log(&mut self) -> Option<sp_externalities::AccessLog> {
		self.optimistic_logger.extract_parent_log()
	}

	/// Did the filters of this worker (not children) fail.
	pub fn did_fail(&self) -> bool {
		self.filters.is_result_for_parent_invalid()
	}

	/// Extract changes from overlay.
	/// TODO audit cost of this: probably very high when used against externalities.
	pub fn extract_delta(&mut self) -> Option<sp_externalities::StateDelta> {
		if self.top.is_empty() && self.children.is_empty() {
			return None
		}
		let (top_iter, children_iter) = self.drain_committed_for(self.markers.start_depth());
		let mut children = Vec::new();
		for (_, (iter, info)) in children_iter {
			let mut added = Vec::new();
			let mut deleted = Vec::new();
			let mut appended = Vec::new();
			let mut incremented = Vec::new();
			for (key, change) in iter {
				match change {
					Change::Write(value) => added.push((key, value)),
					Change::Delete => deleted.push(key),
					Change::Append(changes) => appended.push((key, changes)),
					Change::IncrementCounter(increment) => incremented.push((key, increment)),
				}
			}
			// TODO implement extraction of delta for logged delete prefix
			// actually having delete prefix as first state component of state machine
			// would be more straight forward and better for all but https://github.com/paritytech/substrate/pull/5280
			// did not have support (delete prefix as first state citizen).
			children.push((
				info,
				sp_externalities::TrieDelta {
					added,
					deleted,
					appended,
					incremented,
					deleted_prefix: Vec::new(), // TODO deleted prefix optimization
				},
			));
		}

		let mut added = Vec::new();
		let mut deleted = Vec::new();
		let mut appended = Vec::new();
		let mut incremented = Vec::new();
		for (key, change) in top_iter {
			match change {
				Change::Write(value) => added.push((key, value)),
				Change::Delete => deleted.push(key),
				Change::Append(changes) => appended.push((key, changes)),
				Change::IncrementCounter(increment) => incremented.push((key, increment)),
			}
		}
		Some(sp_externalities::StateDelta {
			top: sp_externalities::TrieDelta {
				added,
				deleted,
				appended,
				incremented,
				deleted_prefix: Vec::new(), // TODO
			},
			children,
		})
	}

	/// Write a key value pair to the offchain storage overlay.
	pub fn set_offchain_storage(&mut self, key: &[u8], value: Option<&[u8]>) {
		use sp_core::offchain::STORAGE_PREFIX;
		match value {
			Some(value) => self.offchain.set(STORAGE_PREFIX, key, value),
			None => self.offchain.remove(STORAGE_PREFIX, key),
		}
	}

	/// Add transaction index operation.
	pub fn add_transaction_index(&mut self, op: IndexOperation) {
		self.transaction_index_ops.push(op)
	}

	/// Inject successful child worker delta to our parent state.
	fn inject_worker_delta(&mut self, delta: sp_externalities::StateDelta) {
		for (key, value) in delta.top.added.into_iter() {
			self.set_storage(key, Some(value));
		}
		for key in delta.top.deleted.into_iter() {
			self.set_storage(key, None);
		}
		for (key, change) in delta.top.appended.into_iter() {
			for change in change.into_iter() {
				self.append_storage(key.clone(), change);
			}
		}
		for (key, change) in delta.top.incremented.into_iter() {
			self.increment_storage(key, change);
		}
		for key in delta.top.deleted_prefix.into_iter() {
			unimplemented!("Deleted prefix delta optimisation.");
		}
		for (child_info, delta) in delta.children.into_iter() {
			for (key, value) in delta.added.into_iter() {
				self.set_child_storage(&child_info, key, Some(value));
			}
			for key in delta.deleted.into_iter() {
				self.set_child_storage(&child_info, key, None);
			}
			for (key, change) in delta.appended.into_iter() {
				for change in change.into_iter() {
					self.append_child_storage(&child_info, key.clone(), change);
				}
			}
			for (key, change) in delta.incremented.into_iter() {
				self.increment_child_storage(&child_info, key, change);
			}
			for key in delta.deleted_prefix.into_iter() {
				unimplemented!("Deleted prefix delta optimisation.");
			}
		}
	}
}

#[cfg(feature = "std")]
fn retain_map<K, V, F>(map: &mut Map<K, V>, f: F)
where
	K: std::cmp::Eq + std::hash::Hash,
	F: FnMut(&K, &mut V) -> bool,
{
	map.retain(f);
}

#[cfg(not(feature = "std"))]
fn retain_map<K, V, F>(map: &mut Map<K, V>, mut f: F)
where
	K: Ord,
	F: FnMut(&K, &mut V) -> bool,
{
	let old = sp_std::mem::replace(map, Map::default());
	for (k, mut v) in old.into_iter() {
		if f(&k, &mut v) {
			map.insert(k, v);
		}
	}
}

/// An overlayed extension is either a mutable reference
/// or an owned extension.
pub enum OverlayedExtension<'a> {
	MutRef(&'a mut Box<dyn Extension>),
	Owned(Box<dyn Extension>),
}

/// Overlayed extensions which are sourced from [`Extensions`].
///
/// The sourced extensions will be stored as mutable references,
/// while extensions that are registered while execution are stored
/// as owned references. After the execution of a runtime function, we
/// can safely drop this object while not having modified the original
/// list.
pub struct OverlayedExtensions<'a> {
	extensions: Map<TypeId, OverlayedExtension<'a>>,
}

impl<'a> OverlayedExtensions<'a> {
	/// Create a new instance of overlayed extensions from the given extensions.
	pub fn new(extensions: &'a mut Extensions) -> Self {
		Self {
			extensions: extensions
				.iter_mut()
				.map(|(k, v)| (*k, OverlayedExtension::MutRef(v)))
				.collect(),
		}
	}

	/// Return a mutable reference to the requested extension.
	pub fn get_mut(&mut self, ext_type_id: TypeId) -> Option<&mut dyn Any> {
		self.extensions.get_mut(&ext_type_id).map(|ext| match ext {
			OverlayedExtension::MutRef(ext) => ext.as_mut_any(),
			OverlayedExtension::Owned(ext) => ext.as_mut_any(),
		})
	}

	/// Register extension `extension` with the given `type_id`.
	pub fn register(
		&mut self,
		type_id: TypeId,
		extension: Box<dyn Extension>,
	) -> Result<(), sp_externalities::Error> {
		match self.extensions.entry(type_id) {
			MapEntry::Vacant(vacant) => {
				vacant.insert(OverlayedExtension::Owned(extension));
				Ok(())
			},
			MapEntry::Occupied(_) => Err(sp_externalities::Error::ExtensionAlreadyRegistered),
		}
	}

	/// Deregister extension with the given `type_id`.
	///
	/// Returns `true` when there was an extension registered for the given `type_id`.
	pub fn deregister(&mut self, type_id: TypeId) -> bool {
		self.extensions.remove(&type_id).is_some()
	}
}

/// Return existing value or newly aggregated value.
/// Last change set is consumed (should only be use on dropped
/// change or if rewriting afterward).
/// TODO make a fully drop variant for into_committed?
fn change_to_change_set(
	child_info: Option<&ChildInfo>,
	key: &[u8],
	change: &mut [crate::overlayed_changes::changeset::InnerValue<Change>],
	backend: &impl AsyncBackendNonSend,
) -> Option<StorageValue> {
	let (do_storage_change, do_increment) = match &mut change.last_mut()?.value {
		Change::Delete => return None,
		Change::Write(value) => return Some(sp_std::mem::take(value)),
		Change::Append(..) => (true, false),
		Change::IncrementCounter(..) => (false, true),
	};
	if do_storage_change {
		let mut start = change.len();
		let mut init_value = None;
		for change in change.iter().rev() {
			match &change.value {
				Change::Append(..) => {
					start -= 1;
				},
				Change::Write(value) => {
					// in previous tx, here only allowed to modifiy last tx
					init_value = Some(value.clone());
					break
				},
				Change::Delete => {
					init_value = Some(Vec::new());
					break
				},
				Change::IncrementCounter(..) => {
					init_value = Some(Vec::new());
					break
				},
			}
		}
		let mut value = if let Some(info) = child_info {
			init_value.unwrap_or_else(|| backend.child_storage(info, key).unwrap_or_default())
		} else {
			init_value.unwrap_or_else(|| backend.storage(key).unwrap_or_default())
		};
		let mut appendable = crate::ext::StorageAppend::new(&mut value);
		for i in start..change.len() - 1 {
			if let Change::Append(changes) = &change[i].value {
				for change in changes.iter() {
					appendable.append(change.clone());
				}
			}
		}
		if let Some(Change::Append(changes)) = change.last_mut().map(|c| &mut c.value) {
			for change in changes.drain(..) {
				appendable.append(change.clone());
			}
		}
		return Some(value)
	}
	if do_increment {
		let mut increment = 0;
		let mut init_value = None;
		for change in change.iter().rev() {
			match &change.value {
				Change::IncrementCounter(nb) => {
					increment += nb;
				},
				Change::Write(value) => {
					init_value = Some(value.clone());
					break
				},
				Change::Delete => {
					init_value = Some(Vec::new());
					break
				},
				Change::Append(..) => {
					init_value = Some(Vec::new());
					break
				},
			}
		}
		let mut value = if let Some(info) = child_info {
			init_value.unwrap_or_else(|| backend.child_storage(info, key).unwrap_or_default())
		} else {
			init_value.unwrap_or_else(|| backend.storage(key).unwrap_or_default())
		};
		for _ in 0..increment {
			// TODO extract and test counter logic.
			let mut nb_digit = value.len();
			while nb_digit > 0 && value[nb_digit - 1] == 255 {
				value[nb_digit - 1] = 0;
				nb_digit -= 1;
			}
			if nb_digit == 0 {
				value.insert(0, 1);
			} else {
				value[nb_digit - 1] += 1;
			}
		}
		return Some(value)
	}
	None
}

fn apply_commit_change(parent: &mut Change, child: Change) {
	match child {
		change @ Change::Write(..) | change @ Change::Delete => {
			*parent = change;
		},
		Change::Append(mut changes) => {
			match parent {
				Change::Append(parent) => {
					parent.append(&mut changes);
					return
				},
				Change::Delete => {
					let mut dest = Vec::new();
					let mut appendable = crate::ext::StorageAppend::new(&mut dest);
					for change in changes {
						appendable.append(change);
					}
					*parent = Change::Write(dest);
					return
				},
				Change::Write(parent) => {
					let mut appendable = crate::ext::StorageAppend::new(parent);
					for change in changes {
						appendable.append(change);
					}
					return
				},
				_ => (),
			}
			*parent = Change::Append(changes);
		},
		Change::IncrementCounter(increment) => {
			match parent {
				Change::IncrementCounter(parent) => {
					*parent += increment;
					return
				},
				Change::Delete => {
					for _ in 0..increment {
						let mut value = Vec::new();
						// TODO extract and test counter logic.
						let mut nb_digit = value.len();
						while nb_digit > 0 && value[nb_digit - 1] == 255 {
							value[nb_digit - 1] = 0;
							nb_digit -= 1;
						}
						if nb_digit == 0 {
							value.insert(0, 1);
						} else {
							value[nb_digit - 1] += 1;
						}
						return
					}
				},
				Change::Write(value) => {
					for _ in 0..increment {
						// TODO extract and test counter logic.
						let mut nb_digit = value.len();
						while nb_digit > 0 && value[nb_digit - 1] == 255 {
							value[nb_digit - 1] = 0;
							nb_digit -= 1;
						}
						if nb_digit == 0 {
							value.insert(0, 1);
						} else {
							value[nb_digit - 1] += 1;
						}
						return
					}
				},
				_ => (),
			}
			*parent = Change::IncrementCounter(increment);
		},
	}
}

fn apply_commit_default<V>(parent: &mut V, child: V) {
	*parent = child;
}

/// Get exclusive end interval corresponding to a prefix.
pub fn end_prefix(prefix: &[u8]) -> Option<Vec<u8>> {
	let mut end_range = prefix.to_vec();
	while let Some(0xff) = end_range.last() {
		end_range.pop();
	}
	if let Some(byte) = end_range.last_mut() {
		*byte += 1;
		Some(end_range)
	} else {
		None
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{ext::Ext, InMemoryBackend};
	use hex_literal::hex;
	use sp_core::{traits::Externalities, Blake2Hasher};
	use std::collections::BTreeMap;

	fn assert_extrinsics(
		overlay: &mut OverlayedChangeSet,
		key: impl AsRef<[u8]>,
		expected: Vec<u32>,
	) {
		assert_eq!(
			overlay.get(key.as_ref()).unwrap().extrinsics().into_iter().collect::<Vec<_>>(),
			expected
		)
	}

	#[test]
	fn overlayed_storage_works() {
		let mut overlayed = OverlayedChanges::default();

		let key = vec![42, 69, 169, 142];

		assert!(overlayed.storage(&key, &()).is_none());

		overlayed.start_transaction();

		overlayed.set_storage(key.clone(), Some(vec![1, 2, 3]));
		assert_eq!(overlayed.storage(&key, &()).unwrap(), Some(&[1, 2, 3][..]));

		overlayed.commit_transaction().unwrap();

		assert_eq!(overlayed.storage(&key, &()).unwrap(), Some(&[1, 2, 3][..]));

		overlayed.start_transaction();

		overlayed.set_storage(key.clone(), Some(vec![]));
		assert_eq!(overlayed.storage(&key, &()).unwrap(), Some(&[][..]));

		overlayed.set_storage(key.clone(), None);
		assert!(overlayed.storage(&key, &()).unwrap().is_none());

		overlayed.rollback_transaction().unwrap();

		assert_eq!(overlayed.storage(&key, &()).unwrap(), Some(&[1, 2, 3][..]));

		overlayed.set_storage(key.clone(), None);
		assert!(overlayed.storage(&key, &()).unwrap().is_none());
	}

	#[test]
	fn offchain_overlayed_storage_transactions_works() {
		use sp_core::offchain::STORAGE_PREFIX;
		fn check_offchain_content(
			state: &OverlayedChanges,
			nb_commit: usize,
			expected: Vec<(Vec<u8>, Option<Vec<u8>>)>,
		) {
			let mut state = state.clone();
			for _ in 0..nb_commit {
				state.commit_transaction().unwrap();
			}
			let offchain_data: Vec<_> = state.offchain_drain_committed().collect();
			let expected: Vec<_> = expected
				.into_iter()
				.map(|(key, value)| {
					let change = match value {
						Some(value) => OffchainOverlayedChange::SetValue(value),
						None => OffchainOverlayedChange::Remove,
					};
					((STORAGE_PREFIX.to_vec(), key), change)
				})
				.collect();
			assert_eq!(offchain_data, expected);
		}

		let mut overlayed = OverlayedChanges::default();

		let key = vec![42, 69, 169, 142];

		check_offchain_content(&overlayed, 0, vec![]);

		overlayed.start_transaction();

		overlayed.set_offchain_storage(key.as_slice(), Some(&[1, 2, 3][..]));
		check_offchain_content(&overlayed, 1, vec![(key.clone(), Some(vec![1, 2, 3]))]);

		overlayed.commit_transaction().unwrap();

		check_offchain_content(&overlayed, 0, vec![(key.clone(), Some(vec![1, 2, 3]))]);

		overlayed.start_transaction();

		overlayed.set_offchain_storage(key.as_slice(), Some(&[][..]));
		check_offchain_content(&overlayed, 1, vec![(key.clone(), Some(vec![]))]);

		overlayed.set_offchain_storage(key.as_slice(), None);
		check_offchain_content(&overlayed, 1, vec![(key.clone(), None)]);

		overlayed.rollback_transaction().unwrap();

		check_offchain_content(&overlayed, 0, vec![(key.clone(), Some(vec![1, 2, 3]))]);

		overlayed.set_offchain_storage(key.as_slice(), None);
		check_offchain_content(&overlayed, 0, vec![(key.clone(), None)]);
	}

	#[test]
	fn overlayed_storage_root_works() {
		let state_version = StateVersion::default();
		let initial: BTreeMap<_, _> = vec![
			(b"doe".to_vec(), b"reindeer".to_vec()),
			(b"dog".to_vec(), b"puppyXXX".to_vec()),
			(b"dogglesworth".to_vec(), b"catXXX".to_vec()),
			(b"doug".to_vec(), b"notadog".to_vec()),
		]
		.into_iter()
		.collect();
		let backend = InMemoryBackend::<Blake2Hasher>::from((initial, state_version));
		let mut overlay = OverlayedChanges::default();

		overlay.start_transaction();
		overlay.set_storage(b"dog".to_vec(), Some(b"puppy".to_vec()));
		overlay.set_storage(b"dogglesworth".to_vec(), Some(b"catYYY".to_vec()));
		overlay.set_storage(b"doug".to_vec(), Some(vec![]));
		overlay.commit_transaction().unwrap();

		overlay.start_transaction();
		overlay.set_storage(b"dogglesworth".to_vec(), Some(b"cat".to_vec()));
		overlay.set_storage(b"doug".to_vec(), None);

		let mut cache = StorageTransactionCache::default();
		let mut ext = Ext::new(&mut overlay, &mut cache, &backend, None);
		const ROOT: [u8; 32] =
			hex!("39245109cef3758c2eed2ccba8d9b370a917850af3824bc8348d505df2c298fa");

		assert_eq!(&ext.storage_root(state_version)[..], &ROOT);
	}

	#[test]
	fn extrinsic_changes_are_collected() {
		let mut overlay = OverlayedChanges::default();
		overlay.set_collect_extrinsics(true);

		overlay.start_transaction();

		overlay.set_storage(vec![100], Some(vec![101]));

		overlay.set_extrinsic_index(0);
		overlay.set_storage(vec![1], Some(vec![2]));

		overlay.set_extrinsic_index(1);
		overlay.set_storage(vec![3], Some(vec![4]));

		overlay.set_extrinsic_index(2);
		overlay.set_storage(vec![1], Some(vec![6]));

		assert_extrinsics(&mut overlay.top, vec![1], vec![0, 2]);
		assert_extrinsics(&mut overlay.top, vec![3], vec![1]);
		assert_extrinsics(&mut overlay.top, vec![100], vec![NO_EXTRINSIC_INDEX]);

		overlay.start_transaction();

		overlay.set_extrinsic_index(3);
		overlay.set_storage(vec![3], Some(vec![7]));

		overlay.set_extrinsic_index(4);
		overlay.set_storage(vec![1], Some(vec![8]));

		assert_extrinsics(&mut overlay.top, vec![1], vec![0, 2, 4]);
		assert_extrinsics(&mut overlay.top, vec![3], vec![1, 3]);
		assert_extrinsics(&mut overlay.top, vec![100], vec![NO_EXTRINSIC_INDEX]);

		overlay.rollback_transaction().unwrap();

		assert_extrinsics(&mut overlay.top, vec![1], vec![0, 2]);
		assert_extrinsics(&mut overlay.top, vec![3], vec![1]);
		assert_extrinsics(&mut overlay.top, vec![100], vec![NO_EXTRINSIC_INDEX]);
	}

	#[test]
	fn next_storage_key_change_works() {
		let mut overlay = OverlayedChanges::default();
		overlay.start_transaction();
		overlay.set_storage(vec![20], Some(vec![20]));
		overlay.set_storage(vec![30], Some(vec![30]));
		overlay.set_storage(vec![40], Some(vec![40]));
		overlay.commit_transaction().unwrap();
		overlay.set_storage(vec![10], Some(vec![10]));
		overlay.set_storage(vec![30], None);

		// next_prospective < next_committed
		let next_to_5 = overlay.iter_after(&[5]).next().unwrap();
		assert_eq!(next_to_5.0.to_vec(), vec![10]);
		assert_eq!(next_to_5.1.value_from_resolved(), Some(&vec![10]));

		// next_committed < next_prospective
		let next_to_10 = overlay.iter_after(&[10]).next().unwrap();
		assert_eq!(next_to_10.0.to_vec(), vec![20]);
		assert_eq!(next_to_10.1.value_from_resolved(), Some(&vec![20]));

		// next_committed == next_prospective
		let next_to_20 = overlay.iter_after(&[20]).next().unwrap();
		assert_eq!(next_to_20.0.to_vec(), vec![30]);
		assert_eq!(next_to_20.1.value_from_resolved(), None);

		// next_committed, no next_prospective
		let next_to_30 = overlay.iter_after(&[30]).next().unwrap();
		assert_eq!(next_to_30.0.to_vec(), vec![40]);
		assert_eq!(next_to_30.1.value_from_resolved(), Some(&vec![40]));

		overlay.set_storage(vec![50], Some(vec![50]));
		// next_prospective, no next_committed
		let next_to_40 = overlay.iter_after(&[40]).next().unwrap();
		assert_eq!(next_to_40.0.to_vec(), vec![50]);
		assert_eq!(next_to_40.1.value_from_resolved(), Some(&vec![50]));
	}

	#[test]
	fn next_child_storage_key_change_works() {
		let child_info = ChildInfo::new_default(b"Child1");
		let child_info = &child_info;
		let child = child_info.storage_key();
		let mut overlay = OverlayedChanges::default();
		overlay.start_transaction();
		overlay.set_child_storage(child_info, vec![20], Some(vec![20]));
		overlay.set_child_storage(child_info, vec![30], Some(vec![30]));
		overlay.set_child_storage(child_info, vec![40], Some(vec![40]));
		overlay.commit_transaction().unwrap();
		overlay.set_child_storage(child_info, vec![10], Some(vec![10]));
		overlay.set_child_storage(child_info, vec![30], None);

		// next_prospective < next_committed
		let next_to_5 = overlay.child_iter_after(child, &[5]).next().unwrap();
		assert_eq!(next_to_5.0.to_vec(), vec![10]);
		assert_eq!(next_to_5.1.value_from_resolved(), Some(&vec![10]));

		// next_committed < next_prospective
		let next_to_10 = overlay.child_iter_after(child, &[10]).next().unwrap();
		assert_eq!(next_to_10.0.to_vec(), vec![20]);
		assert_eq!(next_to_10.1.value_from_resolved(), Some(&vec![20]));

		// next_committed == next_prospective
		let next_to_20 = overlay.child_iter_after(child, &[20]).next().unwrap();
		assert_eq!(next_to_20.0.to_vec(), vec![30]);
		assert_eq!(next_to_20.1.value_from_resolved(), None);

		// next_committed, no next_prospective
		let next_to_30 = overlay.child_iter_after(child, &[30]).next().unwrap();
		assert_eq!(next_to_30.0.to_vec(), vec![40]);
		assert_eq!(next_to_30.1.value_from_resolved(), Some(&vec![40]));

		overlay.set_child_storage(child_info, vec![50], Some(vec![50]));
		// next_prospective, no next_committed
		let next_to_40 = overlay.child_iter_after(child, &[40]).next().unwrap();
		assert_eq!(next_to_40.0.to_vec(), vec![50]);
		assert_eq!(next_to_40.1.value_from_resolved(), Some(&vec![50]));
	}
}
