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
mod offchain;

use self::changeset::{OverlayedChangeSet, OverlayedChangeSetBlob, OverlayedInfos};
use crate::{backend::Backend, stats::StateMachineStats, DefaultError};
use codec::{Decode, Encode};
use hash_db::Hasher;
pub use offchain::OffchainOverlayedChanges;
use sp_core::{
	offchain::OffchainOverlayedChange,
	storage::{
		transient::Hash32Algorithm, well_known_keys::EXTRINSIC_INDEX, Blob as BlobInfo,
		Btree as BtreeInfo, ChildInfo, ChildType, Name, StateVersion, BLOB_CHUNK_SIZE,
	},
};
use sp_externalities::range_slice;
#[cfg(feature = "std")]
use sp_externalities::{Extension, Extensions};
#[cfg(not(feature = "std"))]
use sp_std::collections::btree_map::BTreeMap as Map;
use sp_std::{borrow::Cow, collections::btree_set::BTreeSet, vec::Vec};
#[cfg(feature = "std")]
use std::collections::{hash_map::Entry as MapEntry, HashMap as Map};
#[cfg(feature = "std")]
use std::{
	any::{Any, TypeId},
	boxed::Box,
};

pub use self::changeset::{
	AlreadyInRuntime, NoOpenTransaction, NotInRuntime, OverlayedContext, OverlayedValue,
};

/// Changes that are made outside of extrinsics are marked with this index;
pub const NO_EXTRINSIC_INDEX: u32 = 0xffffffff;

/// Storage key.
pub type StorageKey = Vec<u8>;

/// Storage value.
pub type StorageValue = Vec<u8>;

/// Blob value.
pub type Blob = Vec<u8>;

/// In memory array of storage values.
pub type StorageCollection = Vec<(StorageKey, Option<StorageValue>)>;

/// In memory arrays of storage values for multiple child tries.
pub type ChildStorageCollection = Vec<(StorageKey, StorageCollection)>;

/// Encoded root can be cached.
pub type CachedRoot = Option<Vec<u8>>;

/// Hash can be cache.
pub type CachedHash = Option<[u8; 32]>;

/// In memory changes for btrees.
pub type BtreesCollection = Vec<(BtreeInfo, StorageCollection, CachedRoot)>;

/// In memory changes for blobs.
pub type BlobsCollection = Vec<(BlobInfo, Blob, CachedHash)>;

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
	/// Context of execution.
	context: OverlayedContext,
	/// Top level storage changes.
	top: OverlayedChangeSet,
	/// Child storage changes. The map key is the child storage key without the common prefix.
	children: Map<StorageKey, (OverlayedChangeSet, ChildInfo)>,
	/// Btrees changes. The map key is the child storage key without the common prefix.
	/// TODO BtreeInfo in changeset (also cache root
	btrees: Map<Name, (OverlayedChangeSet, OverlayedInfos<BtreeMeta>)>,
	/// Blobs changes. The map key is the child storage key without the common prefix.
	/// TODO BlobInfo in changeset (also cache root
	blobs: Map<Name, (OverlayedChangeSetBlob, OverlayedInfos<BlobMeta>)>,
	/// Offchain related changes.
	offchain: OffchainOverlayedChanges,
	/// Transaction index changes,
	transaction_index_ops: Vec<IndexOperation>,
	/// True if extrinsics stats must be collected.
	collect_extrinsics: bool,
	/// Collect statistic on this execution.
	stats: StateMachineStats,
}

/// Info attached to a transient btree storage.
#[derive(Debug, Clone)]
pub struct BtreeMeta {
	pub infos: BtreeInfo,
	pub size: usize,
	pub root: Option<Vec<u8>>,
}

impl From<BtreeInfo> for BtreeMeta {
	fn from(infos: BtreeInfo) -> Self {
		Self { infos, size: 0, root: None }
	}
}

/// Info attached to a Blob storage.
#[derive(Debug, Clone)]
pub struct BlobMeta {
	pub infos: BlobInfo,
	pub hash: Option<[u8; 32]>,
}

impl From<BlobInfo> for BlobMeta {
	fn from(infos: BlobInfo) -> Self {
		Self { infos, hash: None }
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
	/// All changes to the btree storages.
	pub btrees_changes: BtreesCollection,
	/// All changes to the blob storages.
	pub blobs_changes: BlobsCollection,
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
			btrees_changes: Default::default(),
			blobs_changes: Default::default(),
			offchain_storage_changes: Default::default(),
			transaction: Default::default(),
			transaction_storage_root: Default::default(),
			#[cfg(feature = "std")]
			transaction_index_changes: Default::default(),
		}
	}
}

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
	pub fn storage(&self, key: &[u8]) -> Option<Option<&[u8]>> {
		self.top.get(key).map(|x| {
			let value = x.value();
			let size_read = value.map(|x| x.len() as u64).unwrap_or(0);
			self.stats.tally_read_modified(size_read);
			value.map(AsRef::as_ref)
		})
	}

	/// Returns mutable reference to current value.
	/// If there is no value in the overlay, the given callback is used to initiate the value.
	/// Warning this function registers a change, so the mutable reference MUST be modified.
	///
	/// Can be rolled back or committed when called inside a transaction.
	#[must_use = "A change was registered, so this value MUST be modified."]
	pub fn value_mut_or_insert_with(
		&mut self,
		key: &[u8],
		init: impl Fn() -> StorageValue,
	) -> &mut StorageValue {
		let value = self.top.modify(key.to_vec(), init, self.extrinsic_index());

		// if the value was deleted initialise it back with an empty vec
		value.get_or_insert_with(StorageValue::default)
	}

	/// Returns a double-Option: None if the key is unknown (i.e. and the query should be referred
	/// to the backend); Some(None) if the key has been deleted. Some(Some(...)) for a key whose
	/// value has been set.
	pub fn child_storage(
		&self,
		child_info: &ChildInfo,
		key: &[u8],
		start: u32,
		limit: Option<u32>,
	) -> Option<Option<Cow<[u8]>>> {
		let value = match child_info {
			ChildInfo::ParentKeyId(_) => {
				let map = self.children.get(child_info.storage_key())?;
				range_slice(map.0.get(key)?.value().map(AsRef::as_ref), start, limit)
					.map(Cow::Borrowed)
			},
			ChildInfo::BTree(info) => {
				let map = self.btrees.get(&info.name)?;
				map.1.value_ref().as_ref()?;
				range_slice(map.0.get(key)?.value().map(AsRef::as_ref), start, limit)
					.map(Cow::Borrowed)
			},
			ChildInfo::Blob(info) => {
				if key.len() > 0 {
					crate::warn!(target: "runtime", "Access blob with non null key.");
					return None
				}

				// semantic is key is index in blob and return value is at most a full blob chunk.
				let map = self.blobs.get(&info.name)?;
				map.1.value_ref().as_ref()?;

				let start = start as usize;
				let size = map.0.bytes_size() as usize;
				if start >= size {
					return Some(Some(Cow::Owned(Vec::new())))
				}
				let size_slice = size - start;
				let size_slice =
					sp_std::cmp::min(size_slice, limit.map(|l| l as usize).unwrap_or(size_slice));
				let end = start + size_slice;
				if end == start {
					return Some(Some(Cow::Owned(Vec::new())))
				}

				let chunk_start = start as u32 / BLOB_CHUNK_SIZE as u32;
				let chunk_start_offset = start % BLOB_CHUNK_SIZE;
				let chunk_end = end as u32 / BLOB_CHUNK_SIZE as u32;
				let chunk_end_offset = end % BLOB_CHUNK_SIZE;
				let (chunk_end, chunk_end_offset) = if chunk_end == 0 {
					(chunk_end - 1, BLOB_CHUNK_SIZE)
				} else {
					(chunk_end, chunk_end_offset)
				};
				let mut result = Vec::with_capacity(size_slice);
				if chunk_start == chunk_end {
					result.extend_from_slice(
						&map.0.get(chunk_start)?.value_ref().as_ref()
							[chunk_start_offset..chunk_end_offset],
					)
				} else {
					result.extend_from_slice(
						&map.0.get(chunk_start)?.value_ref().as_ref()[chunk_start_offset..],
					);
					for at in chunk_start + 1..chunk_end {
						result.extend_from_slice(map.0.get(at)?.value_ref().as_ref());
					}
					result.extend_from_slice(
						&map.0.get(chunk_end)?.value_ref().as_ref()[..chunk_end_offset],
					);
				}
				Some(Cow::Owned(result))
			},
		};
		let size_read = value.as_ref().map(|x| x.len() as u64).unwrap_or(0);
		self.stats.tally_read_modified(size_read);
		Some(value)
	}

	/// Return size of a given blob when defined.
	pub fn blob_len(&self, name: &[u8]) -> Option<u32> {
		let (blob, infos, ..) = self.blobs.get(name)?;
		infos.value_ref().as_ref()?;
		Some(blob.bytes_size())
	}

	/// Return size of a given blob when defined.
	pub fn map_count(&self, name: &[u8]) -> Option<u32> {
		let (blob, infos, ..) = self.btrees.get(name)?;
		infos.value_ref().as_ref().map(|infos| infos.size as u32)
	}

	/// Length of value at a given storage key.
	pub fn child_storage_len(&self, child_info: &ChildInfo, key: &[u8]) -> Option<Option<u32>> {
		let value = match child_info {
			ChildInfo::ParentKeyId(_) => {
				let map = self.children.get(child_info.storage_key())?;
				map.0.get(key)?.value().map(|v| v.len() as u32)
			},
			ChildInfo::BTree(info) => {
				let map = self.btrees.get(&info.name)?;
				map.1.value_ref().as_ref()?;
				map.0.get(key)?.value().map(|v| v.len() as u32)
			},
			ChildInfo::Blob(info) => {
				if key.len() > 0 {
					crate::warn!(target: "runtime", "Access blob with non null key.");
					return None
				}

				self.blob_len(&info.name)
			},
		};
		Some(value)
	}

	/// Set a new value for the specified key.
	///
	/// Can be rolled back or committed when called inside a transaction.
	pub fn set_storage(&mut self, key: StorageKey, val: Option<StorageValue>) {
		let size_write = val.as_ref().map(|x| x.len() as u64).unwrap_or(0);
		self.stats.tally_write_overlay(size_write);
		self.top.set(key, val, self.extrinsic_index());
	}

	/// Set a new value for the specified key and child.
	///
	/// `None` can be used to delete a value specified by the given key.
	///
	/// Can be rolled back or committed when called inside a transaction.
	///
	/// Return true if value was set.
	pub(crate) fn set_child_storage(
		&mut self,
		child_info: &ChildInfo,
		key: &[u8],
		val: Option<&[u8]>,
	) -> bool {
		let extrinsic_index = self.extrinsic_index();
		let size_write = val.as_ref().map(|x| x.len() as u64).unwrap_or(0);
		self.stats.tally_write_overlay(size_write);
		let top = &self.top;

		match child_info {
			ChildInfo::ParentKeyId(_) => {
				let storage_key = child_info.storage_key().to_vec();
				let (changeset, info) = self
					.children
					.entry(storage_key)
					.or_insert_with(|| (top.spawn_child(), child_info.clone()));
				let updatable = info.try_update(child_info);
				debug_assert!(updatable);
				changeset.set(key.to_vec(), val.map(Vec::from), extrinsic_index);
				true
			},
			ChildInfo::BTree(infos) =>
				if let Some((changeset, info)) = self.btrees.get_mut(&infos.name) {
					let has_root = match info.value_ref() {
						Some(infos) => infos.root.is_some(),
						None => return false,
					};
					let (existing, changed) = changeset
						.get(key)
						.map(|v| {
							(
								v.value_ref().is_some(),
								v.value_ref().as_ref().map(Vec::as_slice) == val,
							)
						})
						.unwrap_or((false, true));
					if (has_root && changed) ||
						(existing && val.is_none()) ||
						(!existing && val.is_some())
					{
						let infos =
							info.modify_value(extrinsic_index).as_mut().expect("checked above");
						if changed {
							infos.root = None;
						}
						if val.is_some() {
							infos.size += 1;
						} else {
							infos.size -= 1;
						}
					}
					changeset.set(key.to_vec(), val.map(Vec::from), extrinsic_index);
					true
				} else {
					false
				},
			ChildInfo::Blob(_) => {
				crate::warn!(target: "runtime", "Attempt to use set child on a blob, ignoring.");
				false
			},
		}
	}

	/// Push a new value in the specified child (if support appending).
	///
	/// Can be rolled back or committed when called inside a transaction.
	/// TODO correct api (offset does not exist (non persistent)).
	pub fn push_storage(
		&mut self,
		child_info: &ChildInfo,
		val: StorageValue,
		fetch_offset: impl FnOnce() -> u64,
	) {
		let size_write = val.len() as u64;
		self.stats.tally_write_overlay(size_write);
		unimplemented!("TODO");
		/*
		let storage_key = child_info.storage_key().to_vec();
		let top = &self.top;
		let (changeset, info) = self.blobs.entry(storage_key).or_insert_with(|| {
			let changes = top.spawn_child_list();
			let offset = fetch_offset();
			(OverlayedChangeSetBlob { changes, offset }, child_info.clone())
		});
		let updatable = info.try_update(child_info);
		debug_assert!(updatable);
		changeset.changes.push(val);
		*/
	}

	/// TODO a fn to change/set child info (for setting hashing for instance). Or only for caching??
	/// -> then cache with root -> then no use having it in BtreeInfo -> TODO rem from btreeinfo
	/// (try_update does not make much sense).

	/// Clear child storage of given storage key.
	///
	/// Can be rolled back or committed when called inside a transaction.
	pub(crate) fn clear_child_storage(&mut self, child_info: &ChildInfo) -> u32 {
		let extrinsic_index = self.extrinsic_index();
		let storage_key = child_info.storage_key().to_vec();
		let top = &self.top;
		match child_info {
			ChildInfo::ParentKeyId(_) => {
				let (changeset, info) = self
					.children
					.entry(storage_key)
					// could also return None here, but this does not impact
					// this kind of child trie where empty trie is never stored.
					.or_insert_with(|| (top.spawn_child(), child_info.clone()));
				let updatable = info.try_update(child_info);
				debug_assert!(updatable);
				changeset.clear_where(|_, _| true, extrinsic_index)
			},
			ChildInfo::Blob(info) =>
				if let Some(entry) = self.blobs.get_mut(&info.name) {
					let info = entry.1.modify_value(extrinsic_index);
					*info = None;
					entry.0.clear(extrinsic_index)
				} else {
					0
				},
			ChildInfo::BTree(info) =>
				if let Some(entry) = self.btrees.get_mut(&info.name) {
					let info = entry.1.modify_value(extrinsic_index);
					*info = None;
					entry.0.clear_where(|_, _| true, extrinsic_index)
				} else {
					0
				},
		}
	}

	// TODO here move or copy

	/// Removes all key-value pairs which keys share the given prefix.
	///
	/// Can be rolled back or committed when called inside a transaction.
	pub(crate) fn clear_prefix(&mut self, prefix: &[u8]) -> u32 {
		self.top.clear_where(|key, _| key.starts_with(prefix), self.extrinsic_index())
	}

	/// Removes all key-value pairs which keys share the given prefix.
	///
	/// Can be rolled back or committed when called inside a transaction
	pub(crate) fn clear_child_prefix(&mut self, child_info: &ChildInfo, prefix: &[u8]) -> u32 {
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

	/// Start a new nested transaction.
	///
	/// This allows to either commit or roll back all changes that where made while this
	/// transaction was open. Any transaction must be closed by either `rollback_transaction` or
	/// `commit_transaction` before this overlay can be converted into storage changes.
	///
	/// Changes made without any open transaction are committed immediately.
	pub fn start_transaction(&mut self) {
		self.top.start_transaction();
		for (_, (changeset, _)) in self.children.iter_mut() {
			changeset.start_transaction();
		}
		for (_, (changeset, infos)) in self.btrees.iter_mut() {
			infos.start_transaction();
			changeset.start_transaction();
		}
		for (_, (changeset, infos)) in self.blobs.iter_mut() {
			infos.start_transaction();
			changeset.changes.start_transaction();
		}
		self.offchain.overlay_mut().start_transaction();
	}

	/// Rollback the last transaction started by `start_transaction`.
	///
	/// Any changes made during that transaction are discarded. Returns an error if
	/// there is no open transaction that can be rolled back.
	pub fn rollback_transaction(&mut self) -> Result<(), NoOpenTransaction> {
		self.context.guard_no_runtime_transaction(&self.top)?;
		self.top.rollback_transaction()?;
		retain_map(&mut self.children, |_, (changeset, _)| {
			changeset
				.rollback_transaction()
				.expect("Top and children changesets are started in lockstep; qed");
			!changeset.is_empty()
		});
		retain_map(&mut self.btrees, |_, (changeset, infos)| {
			infos.rollback_transaction();
			changeset
				.rollback_transaction()
				.expect("Top and children changesets are started in lockstep; qed");
			!(changeset.is_empty() && infos.value_ref().is_none() && infos.transaction_depth() == 0)
		});
		retain_map(&mut self.blobs, |_, (changeset, infos)| {
			infos.rollback_transaction();
			changeset
				.changes
				.rollback_transaction()
				.expect("Top and children changesets are started in lockstep; qed");
			!(changeset.changes.is_empty() &&
				infos.value_ref().is_none() &&
				infos.transaction_depth() == 0)
		});
		self.offchain
			.overlay_mut()
			.rollback_transaction()
			.expect("Top and offchain changesets are started in lockstep; qed");
		Ok(())
	}

	/// Commit the last transaction started by `start_transaction`.
	///
	/// Any changes made during that transaction are committed. Returns an error if there
	/// is no open transaction that can be committed.
	pub fn commit_transaction(&mut self) -> Result<(), NoOpenTransaction> {
		self.context.guard_no_runtime_transaction(&self.top)?;
		self.top.commit_transaction()?;
		for (_, (changeset, _)) in self.children.iter_mut() {
			changeset
				.commit_transaction()
				.expect("Top and children changesets are started in lockstep; qed");
		}
		for (_, (changeset, infos)) in self.btrees.iter_mut() {
			infos.commit_transaction();
			changeset
				.commit_transaction()
				.expect("Top and children changesets are started in lockstep; qed");
		}
		for (_, (changeset, infos)) in self.blobs.iter_mut() {
			infos.commit_transaction();
			changeset
				.changes
				.commit_transaction()
				.expect("Top and children changesets are started in lockstep; qed");
		}
		self.offchain
			.overlay_mut()
			.commit_transaction()
			.expect("Top and offchain changesets are started in lockstep; qed");
		Ok(())
	}

	/// Call this before transfering control to the runtime.
	///
	/// This protects all existing transactions from being removed by the runtime.
	/// Calling this while already inside the runtime will return an error.
	pub fn enter_runtime(&mut self) -> Result<(), AlreadyInRuntime> {
		self.context.enter_runtime(self.top.transaction_depth())?;
		Ok(())
	}

	/// Call this when control returns from the runtime.
	///
	/// This commits all dangling transaction left open by the runtime.
	/// Calling this while outside the runtime will return an error.
	pub fn exit_runtime(&mut self) -> Result<(), NotInRuntime> {
		self.context.exit_runtime()?;
		self.top.roll_back_open_transactions(&self.context);
		for (_, (changeset, infos)) in self.children.iter_mut() {
			changeset.roll_back_open_transactions(&self.context)
		}
		for (_, (changeset, infos)) in self.btrees.iter_mut() {
			infos.roll_back_open_transactions(&self.context);
			changeset.roll_back_open_transactions(&self.context)
		}
		for (_, (changeset, infos)) in self.blobs.iter_mut() {
			infos.roll_back_open_transactions(&self.context);
			changeset.changes.roll_back_open_transactions(&self.context)
		}
		self.offchain.overlay_mut().roll_back_open_transactions(&self.context);
		Ok(())
	}

	/// Consume all changes (top + children) and return them.
	///
	/// After calling this function no more changes are contained in this changeset.
	///
	/// Panics:
	/// Panics if `transaction_depth() > 0`
	fn drain_committed(
		&mut self,
	) -> (
		impl Iterator<Item = (StorageKey, Option<StorageValue>)>,
		impl Iterator<
			Item = (
				StorageKey,
				(impl Iterator<Item = (StorageKey, Option<StorageValue>)>, ChildInfo),
			),
		>,
		impl Iterator<Item = (BtreeMeta, impl Iterator<Item = (StorageKey, Option<StorageValue>)>)>,
		impl Iterator<Item = (BlobMeta, Blob)>,
	) {
		use sp_std::mem::take;
		(
			take(&mut self.top).drain_commited(),
			take(&mut self.children)
				.into_iter()
				.map(|(key, (val, info))| (key, (val.drain_commited(), info))),
			take(&mut self.btrees).into_iter().filter_map(|(_key, (val, info))| {
				if let Some(info) = info.drain_commited() {
					Some((info, val.drain_commited()))
				} else {
					None
				}
			}),
			take(&mut self.blobs).into_iter().filter_map(|(_key, (val, info))| {
				if let Some(info) = info.drain_commited() {
					let blob: Blob = Vec::new(); // TODO val.changes.commited_blob();
					Some((info, blob))
				} else {
					None
				}
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
		&self,
	) -> impl Iterator<Item = (impl Iterator<Item = (&StorageKey, &OverlayedValue)>, &ChildInfo)> {
		self.children.values().map(|v| (v.0.changes(), &v.1))
	}

	/// Get an iterator over all child changes as seen by the current transaction.
	pub fn blob_storages(
		&self,
	) -> impl Iterator<Item = (impl Iterator<Item = &Vec<u8>>, &BlobMeta)> {
		self.blobs
			.iter()
			.filter_map(|(_, v)| v.1.value_ref().as_ref().map(|i| (v.0.changes.changes.iter(), i)))
	}

	/// Get an iterator over all top changes as been by the current transaction.
	pub fn changes(&self) -> impl Iterator<Item = (&StorageKey, &OverlayedValue)> {
		self.top.changes()
	}

	/// Get an optional iterator over all child changes stored under the supplied key.
	pub fn child_changes(
		&self,
		key: &[u8],
	) -> Option<(impl Iterator<Item = (&StorageKey, &OverlayedValue)>, &ChildInfo)> {
		self.children.get(key).map(|(overlay, info)| (overlay.changes(), info))
	}

	/// Get an optional iterator over all btree changes stored under the supplied key.
	pub fn btree_changes(
		&self,
		name: &[u8],
	) -> Option<(impl Iterator<Item = (&StorageKey, &OverlayedValue)>, &BtreeMeta)> {
		self.btrees
			.get(name)
			.and_then(|(overlay, info)| info.value_ref().as_ref().map(|i| (overlay.changes(), i)))
	}

	/// Get an optional iterator over all blob changes stored under the supplied key.
	pub fn blob_changes(&self, name: &[u8]) -> Option<(impl Iterator<Item = &[u8]>, &BlobMeta)> {
		self.blobs.get(name).and_then(|(overlay, info)| {
			info.value_ref()
				.as_ref()
				.map(|i| (overlay.changes.changes.iter().map(AsRef::as_ref), i))
		})
	}

	/// Get an list of all index operations.
	pub fn transaction_index_ops(&self) -> &[IndexOperation] {
		&self.transaction_index_ops
	}

	/// Convert this instance with all changes into a [`StorageChanges`] instance.
	#[cfg(feature = "std")]
	pub fn into_storage_changes<B: Backend<H>, H: Hasher>(
		mut self,
		backend: &B,
		mut cache: StorageTransactionCache<B::Transaction, H>,
		state_version: StateVersion,
	) -> Result<StorageChanges<B::Transaction, H>, DefaultError>
	where
		H::Out: Ord + Encode + 'static,
	{
		self.drain_storage_changes(backend, &mut cache, state_version)
	}

	/// Drain all changes into a [`StorageChanges`] instance. Leave empty overlay in place.
	pub fn drain_storage_changes<B: Backend<H>, H: Hasher>(
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

		let (main_storage_changes, child_storage_changes, btrees_changes, blobs_changes) =
			self.drain_committed();
		let offchain_storage_changes = self.offchain_drain_committed().collect();

		#[cfg(feature = "std")]
		let transaction_index_changes = std::mem::take(&mut self.transaction_index_ops);

		Ok(StorageChanges {
			main_storage_changes: main_storage_changes.collect(),
			child_storage_changes: child_storage_changes
				.map(|(sk, it)| (sk, it.0.collect()))
				.collect(),
			btrees_changes: btrees_changes
				.map(|(info, it)| (info.infos, it.collect(), info.root))
				.collect(),
			blobs_changes: blobs_changes
				.map(|(info, blob)| (info.infos, blob, info.hash))
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
		self.top.set(EXTRINSIC_INDEX.to_vec(), Some(extrinsic_index.encode()), None);
	}

	/// Returns current extrinsic index to use in changes trie construction.
	/// None is returned if it is not set or changes trie config is not set.
	/// Persistent value (from the backend) can be ignored because runtime must
	/// set this index before first and unset after last extrinsic is executed.
	/// Changes that are made outside of extrinsics, are marked with
	/// `NO_EXTRINSIC_INDEX` index.
	fn extrinsic_index(&self) -> Option<u32> {
		match self.collect_extrinsics {
			true => Some(
				self.storage(EXTRINSIC_INDEX)
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
		&self,
		backend: &B,
		cache: &mut StorageTransactionCache<B::Transaction, H>,
		state_version: StateVersion,
	) -> H::Out
	where
		H::Out: Ord + Encode,
	{
		let delta = self.changes().map(|(k, v)| (&k[..], v.value().map(|v| &v[..])));
		let child_delta = self.children().map(|(changes, info)| {
			(info, changes.map(|(k, v)| (&k[..], v.value().map(|v| &v[..]))))
		});

		let (root, transaction) = backend.full_storage_root(delta, child_delta, state_version);

		cache.transaction = Some(transaction);
		cache.transaction_storage_root = Some(root);

		root
	}

	/// Returns an iterator over the keys (in lexicographic order) following `key` (excluding `key`)
	/// alongside its value.
	pub fn iter_after(&self, key: &[u8]) -> impl Iterator<Item = (&[u8], &OverlayedValue)> {
		self.top.changes_after(key)
	}

	/// Returns an iterator over the keys (in lexicographic order) following `key` (excluding `key`)
	/// alongside its value for the given `storage_key` child.
	pub fn child_iter_after(
		&self,
		child_info: &ChildInfo,
		key: &[u8],
	) -> Option<impl Iterator<Item = (&[u8], &OverlayedValue)>> {
		match child_info {
			ChildInfo::ParentKeyId(_) => Some(
				self.children
					.get(child_info.storage_key())
					.map(|(overlay, _)| overlay.changes_after(key))
					.into_iter()
					.flatten(),
			),
			ChildInfo::BTree(infos) => unimplemented!(),
			ChildInfo::Blob(infos) => None,
		}
	}

	/// Read only access ot offchain overlay.
	pub fn offchain(&self) -> &OffchainOverlayedChanges {
		&self.offchain
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

	/// Check if a storage was initialized.
	pub fn storage_exists(&self, info: &ChildInfo) -> bool {
		match info {
			// default child storage are viewed as always existing
			// with empty root.
			ChildInfo::ParentKeyId(_) => true,
			ChildInfo::BTree(info) => self
				.btrees
				.get(info.name.as_slice())
				.map(|btree| btree.1.value_ref().is_some())
				.unwrap_or(false),
			ChildInfo::Blob(info) => self
				.blobs
				.get(info.name.as_slice())
				.map(|blob| blob.1.value_ref().is_some())
				.unwrap_or(false),
		}
	}

	/// Change info for a storage, reset storage data if `clear_existing` is set to `true`.
	/// Return true if update happen (either the storage already exist or was created).
	pub fn update_storage_info(&mut self, info: ChildInfo, clear_existing: bool) -> bool {
		match info {
			// No update info for default child storage,
			// its existence is only related to its content.
			ChildInfo::ParentKeyId(_) => false,
			ChildInfo::BTree(info) => {
				let extrinsic_index = self.extrinsic_index();
				let entry = self
					.btrees
					.entry(info.name.clone())
					.or_insert_with(|| (Default::default(), BtreeMeta::from(info.clone()).into()));
				// even if same content we register and
				// update.
				let previous_info = entry.1.modify_value(extrinsic_index);
				if clear_existing {
					entry.0.clear_where(|_, _| true, extrinsic_index);
				}
				if let Some(previous_info) = previous_info.as_mut() {
					if previous_info.infos.structure != info.structure {
						previous_info.root = None;
					}
					// TODO put in in a and_modify?? so only when existing entry?
					// TODO try update function is rather unclear: just explicitely
					// update algo and error on different name or different mode (if not None).
					let success = previous_info.infos.try_update(&info);
					if !success {
						crate::warn!(target: "runtime", "Try update failure should not happen.");
					}
					success
				} else {
					*previous_info = Some(info.into());
					true
				}
			},
			ChildInfo::Blob(info) => {
				let extrinsic_index = self.extrinsic_index();
				let entry = self
					.blobs
					.entry(info.name.clone())
					.or_insert_with(|| (Default::default(), BlobMeta::from(info.clone()).into()));
				// even if same content we register and
				// update.
				let previous_info = entry.1.modify_value(extrinsic_index);
				if clear_existing {
					entry.0.clear(extrinsic_index);
				}
				if let Some(previous_info) = previous_info.as_mut() {
					if previous_info.infos.algorithm != info.algorithm {
						previous_info.hash = None;
					}

					let success = previous_info.infos.try_update(&info);
					if !success {
						crate::warn!(target: "runtime", "Try update failure should not happen.");
					}
					success
				} else {
					*previous_info = Some(info.into());
					true
				}
			},
		}
	}

	/// Cache root.
	/// Do nothing on failure.
	pub fn cache_btree_root(&mut self, name: &[u8], root: Vec<u8>) {
		let extrinsic_index = self.extrinsic_index();
		let Some(entry) = self.btrees.get_mut(name) else {
			return
		};
		let Some(entry) = entry.1.modify_value(extrinsic_index).as_mut() else {
			return
		};
		entry.root = Some(root);
	}

	/// Cache hash.
	/// Do nothing on failure.
	pub fn cache_blob_hash(&mut self, name: &[u8], hash: [u8; 32]) {
		let extrinsic_index = self.extrinsic_index();
		let Some(entry) = self.blobs.get_mut(name) else {
			return
		};
		let Some(entry) = entry.1.modify_value(extrinsic_index).as_mut() else {
			return
		};
		entry.hash = Some(hash);
	}

	/// Copy current content of a storage to another one.
	/// If target exists it is replaced by this new content.
	pub fn storage_copy(&mut self, info: &ChildInfo, target_name: &[u8]) -> bool {
		match info {
			// Copy not supported.
			ChildInfo::ParentKeyId(_) => false,
			ChildInfo::BTree(info) => {
				if &info.name == target_name {
					return false
				}
				let extrinsic_index = self.extrinsic_index();
				let Some(origin) = self.btrees.get(&info.name) else {
					return false
				};
				let Some(info_origin) = origin.1.value_ref().clone() else {
					return false
				};

				let origin = &origin.0 as *const OverlayedChangeSet;

				let entry = self
					.btrees
					.entry(info.name.clone())
					.and_modify(|entry| {
						entry.0.clear_where(|_, _| true, extrinsic_index);
					})
					.or_insert_with(|| (Default::default(), BtreeMeta::from(info.clone()).into()));

				// even if same content we register and
				// update.
				let dest = entry.1.modify_value(extrinsic_index);
				*dest = Some(info_origin);

				// Access pointer to origin as we know btree
				// implementation do not change this one when
				// he create an entry.
				let origin = unsafe { &*origin };
				for (key, value) in origin.changes() {
					if let Some(value) = value.value_ref() {
						let dest = entry.0.modify(key.clone(), Default::default, extrinsic_index);
						*dest = Some(value.clone());
					}
				}
				true
			},
			ChildInfo::Blob(info) => {
				if &info.name == target_name {
					return false
				}
				let extrinsic_index = self.extrinsic_index();
				let Some(origin) = self.blobs.get(&info.name) else {
					return false
				};
				let Some(info_origin) = origin.1.value_ref().clone() else {
					return false
				};

				let origin = &origin.0 as *const OverlayedChangeSetBlob;

				let entry = self
					.blobs
					.entry(info.name.clone())
					.and_modify(|entry| {
						entry.0.clear(extrinsic_index);
					})
					.or_insert_with(|| (Default::default(), BlobMeta::from(info.clone()).into()));

				// even if same content we register and
				// update.
				let dest = entry.1.modify_value(extrinsic_index);
				*dest = Some(info_origin);

				// Access pointer to origin as we know btree
				// implementation do not change this one when
				// he create an entry.
				let origin = unsafe { &*origin };
				for value in origin.changes.changes() {
					entry.0.changes.push(value.clone());
				}
				true
			},
		}
	}

	/// Move a storage to a different name.
	/// If target exists it is replaced by this new content.
	pub fn storage_move(&mut self, info: &ChildInfo, target_name: &[u8]) -> bool {
		match info {
			// Move not supported.
			ChildInfo::ParentKeyId(_) => false,
			ChildInfo::BTree(btree) => {
				if &btree.name == target_name {
					return false
				}
				// we need to copy to keep transaction
				// history of changes.
				if self.storage_copy(info, target_name) {
					let _ = self.clear_child_storage(info);
					true
				} else {
					false
				}
			},
			ChildInfo::Blob(blob) => {
				if &blob.name == target_name {
					return false
				}
				// we need to copy to keep transaction
				// history of changes.
				if self.storage_copy(info, target_name) {
					let _ = self.clear_child_storage(info);
					true
				} else {
					false
				}
			},
		}
	}
}

#[cfg(feature = "std")]
impl From<sp_core::storage::Storage> for OverlayedChanges {
	fn from(storage: sp_core::storage::Storage) -> Self {
		Self {
			context: Default::default(),
			top: storage.top.into(),
			children: storage
				.children_default
				.into_iter()
				.map(|(k, v)| (k, (v.data.into(), v.child_info)))
				.collect(),
			btrees: storage
				.btree_storages
				.into_iter()
				// TODO overlay also with btree infos and root
				// TODO add btree info in overlayedchangeset?
				.map(|(k, v)| (k, (v.data.into(), BtreeMeta::from(v.info).into())))
				.collect(),
			blobs: storage
				.blob_storages
				.into_iter()
				.map(|(k, v)| {
					let overlay_set = unimplemented!("from data and root and info");
					(k, (overlay_set, BlobMeta::from(v.info).into()))
				})
				.collect(),
			offchain: Default::default(),
			collect_extrinsics: Default::default(),
			transaction_index_ops: Default::default(),
			stats: Default::default(),
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
#[cfg(feature = "std")]
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
#[cfg(feature = "std")]
pub struct OverlayedExtensions<'a> {
	extensions: Map<TypeId, OverlayedExtension<'a>>,
}

#[cfg(feature = "std")]
impl<'a> OverlayedExtensions<'a> {
	/// Create a new instance of overalyed extensions from the given extensions.
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

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{ext::Ext, InMemoryBackend};
	use sp_core::{traits::Externalities, Blake2Hasher};
	use std::collections::BTreeMap;

	fn assert_extrinsics(overlay: &OverlayedChangeSet, key: impl AsRef<[u8]>, expected: Vec<u32>) {
		assert_eq!(
			overlay.get(key.as_ref()).unwrap().extrinsics().into_iter().collect::<Vec<_>>(),
			expected
		)
	}

	#[test]
	fn overlayed_storage_works() {
		let mut overlayed = OverlayedChanges::default();

		let key = vec![42, 69, 169, 142];

		assert!(overlayed.storage(&key).is_none());

		overlayed.start_transaction();

		overlayed.set_storage(key.clone(), Some(vec![1, 2, 3]));
		assert_eq!(overlayed.storage(&key).unwrap(), Some(&[1, 2, 3][..]));

		overlayed.commit_transaction().unwrap();

		assert_eq!(overlayed.storage(&key).unwrap(), Some(&[1, 2, 3][..]));

		overlayed.start_transaction();

		overlayed.set_storage(key.clone(), Some(vec![]));
		assert_eq!(overlayed.storage(&key).unwrap(), Some(&[][..]));

		overlayed.set_storage(key.clone(), None);
		assert!(overlayed.storage(&key).unwrap().is_none());

		overlayed.rollback_transaction().unwrap();

		assert_eq!(overlayed.storage(&key).unwrap(), Some(&[1, 2, 3][..]));

		overlayed.set_storage(key.clone(), None);
		assert!(overlayed.storage(&key).unwrap().is_none());
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
		let root = array_bytes::hex2bytes_unchecked(
			"39245109cef3758c2eed2ccba8d9b370a917850af3824bc8348d505df2c298fa",
		);

		assert_eq!(&ext.storage_root(state_version)[..], &root);
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

		assert_extrinsics(&overlay.top, vec![1], vec![0, 2]);
		assert_extrinsics(&overlay.top, vec![3], vec![1]);
		assert_extrinsics(&overlay.top, vec![100], vec![NO_EXTRINSIC_INDEX]);

		overlay.start_transaction();

		overlay.set_extrinsic_index(3);
		overlay.set_storage(vec![3], Some(vec![7]));

		overlay.set_extrinsic_index(4);
		overlay.set_storage(vec![1], Some(vec![8]));

		assert_extrinsics(&overlay.top, vec![1], vec![0, 2, 4]);
		assert_extrinsics(&overlay.top, vec![3], vec![1, 3]);
		assert_extrinsics(&overlay.top, vec![100], vec![NO_EXTRINSIC_INDEX]);

		overlay.rollback_transaction().unwrap();

		assert_extrinsics(&overlay.top, vec![1], vec![0, 2]);
		assert_extrinsics(&overlay.top, vec![3], vec![1]);
		assert_extrinsics(&overlay.top, vec![100], vec![NO_EXTRINSIC_INDEX]);
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
		assert_eq!(next_to_5.1.value(), Some(&vec![10]));

		// next_committed < next_prospective
		let next_to_10 = overlay.iter_after(&[10]).next().unwrap();
		assert_eq!(next_to_10.0.to_vec(), vec![20]);
		assert_eq!(next_to_10.1.value(), Some(&vec![20]));

		// next_committed == next_prospective
		let next_to_20 = overlay.iter_after(&[20]).next().unwrap();
		assert_eq!(next_to_20.0.to_vec(), vec![30]);
		assert_eq!(next_to_20.1.value(), None);

		// next_committed, no next_prospective
		let next_to_30 = overlay.iter_after(&[30]).next().unwrap();
		assert_eq!(next_to_30.0.to_vec(), vec![40]);
		assert_eq!(next_to_30.1.value(), Some(&vec![40]));

		overlay.set_storage(vec![50], Some(vec![50]));
		// next_prospective, no next_committed
		let next_to_40 = overlay.iter_after(&[40]).next().unwrap();
		assert_eq!(next_to_40.0.to_vec(), vec![50]);
		assert_eq!(next_to_40.1.value(), Some(&vec![50]));
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
		assert_eq!(next_to_5.1.value(), Some(&vec![10]));

		// next_committed < next_prospective
		let next_to_10 = overlay.child_iter_after(child, &[10]).next().unwrap();
		assert_eq!(next_to_10.0.to_vec(), vec![20]);
		assert_eq!(next_to_10.1.value(), Some(&vec![20]));

		// next_committed == next_prospective
		let next_to_20 = overlay.child_iter_after(child, &[20]).next().unwrap();
		assert_eq!(next_to_20.0.to_vec(), vec![30]);
		assert_eq!(next_to_20.1.value(), None);

		// next_committed, no next_prospective
		let next_to_30 = overlay.child_iter_after(child, &[30]).next().unwrap();
		assert_eq!(next_to_30.0.to_vec(), vec![40]);
		assert_eq!(next_to_30.1.value(), Some(&vec![40]));

		overlay.set_child_storage(child_info, vec![50], Some(vec![50]));
		// next_prospective, no next_committed
		let next_to_40 = overlay.child_iter_after(child, &[40]).next().unwrap();
		assert_eq!(next_to_40.0.to_vec(), vec![50]);
		assert_eq!(next_to_40.1.value(), Some(&vec![50]));
	}
}
