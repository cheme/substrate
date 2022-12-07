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

use self::changeset::{OverlayedChangeSet, OverlayedChangeSetBlob, OverlayedInfos, OriginEntry};
use crate::{backend::Backend, stats::StateMachineStats, DefaultError};
use codec::{Decode, Encode};
use hash_db::Hasher;
pub use offchain::OffchainOverlayedChanges;
use sp_core::{
	offchain::OffchainOverlayedChange,
	storage::{
		well_known_keys::EXTRINSIC_INDEX, Blob as BlobInfo, Btree as BtreeInfo, ChildInfo, Name,
		StateVersion, BLOB_CHUNK_SIZE,
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

/// In memory changes for btrees.
pub type BtreesCollection = Vec<(BtreeMeta, StorageCollection)>;

/// In memory changes for blobs.
pub type BlobsCollection = Vec<(BlobMeta, Blob)>;

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
	btrees: Map<Name, (OverlayedChangeSet, OverlayedInfos<BtreeMeta, OverlayedChangeSet>)>,
	/// Blobs changes. The map key is the child storage key without the common prefix.
	/// TODO BlobInfo in changeset (also cache root
	blobs: Map<Name, (OverlayedChangeSetBlob, OverlayedInfos<BlobMeta, OverlayedChangeSetBlob>)>,
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
	// TODO this additional info will mark extrinsic
	// technical change: TODO probably remove this
	// extrinsics first anyway (otherwhise use two
	// modify_value variants).
	pub size: usize,
	pub root: Option<Vec<u8>>,
	pub removed: bool,
}

impl From<BtreeInfo> for BtreeMeta {
	fn from(infos: BtreeInfo) -> Self {
		Self { infos, size: 0, root: None, removed: false }
	}
}

/// Info attached to a Blob storage.
#[derive(Debug, Clone)]
pub struct BlobMeta {
	pub infos: BlobInfo,
	// TODO same remark as btree
	// TODO u32??
	pub size: usize,
	pub hash: Option<[u8; 32]>,
	pub removed: bool,
}

impl From<BlobInfo> for BlobMeta {
	fn from(infos: BlobInfo) -> Self {
		Self { infos, hash: None, size: 0, removed: false }
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
		BtreesCollection,
		BlobsCollection,
		OffchainChangesCollection,
		Transaction,
		H::Out,
		Vec<IndexOperation>,
	) {
		(
			self.main_storage_changes,
			self.child_storage_changes,
			self.btrees_changes,
			self.blobs_changes,
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
	/// TODO we use Cow here just for the blob case, maybe just separate functions.
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
				if map.1.value_ref().removed {
					return Some(None)
				}
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
				let infos = map.1.value_ref();
				if infos.removed {
					return Some(None)
				}

				let start = start as usize;
				let size = infos.size as usize;
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

				let (chunk_start, chunk_start_offset) = blob_chunk_start_index(start);
				let (chunk_end, chunk_end_offset) = blob_chunk_end_index(end);
				let mut result = Vec::with_capacity(size_slice);
				if chunk_start == chunk_end {
					result.extend_from_slice(
						&map.0.get(chunk_start).value_ref().as_ref()
							[chunk_start_offset..chunk_end_offset],
					)
				} else {
					result.extend_from_slice(
						&map.0.get(chunk_start).value_ref().as_ref()[chunk_start_offset..],
					);
					for at in chunk_start + 1..chunk_end {
						result.extend_from_slice(map.0.get(at).value_ref().as_ref());
					}
					result.extend_from_slice(
						&map.0.get(chunk_end).value_ref().as_ref()[..chunk_end_offset],
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
		let (_, infos, ..) = self.blobs.get(name)?;
		let infos = infos.value_ref();
		if infos.removed {
			None
		} else {
			Some(infos.size as u32)
		}
	}

	/// Return iterator on chunks if defined.
	pub fn blob_chunks(&self, name: &[u8]) -> Option<(impl Iterator<Item = &[u8]>, &BlobMeta)> {
		Self::blob_chunks_inner(self.blobs.get(name)?)
	}
	fn blob_chunks_inner<'a>(
		blob: &'a (OverlayedChangeSetBlob, OverlayedInfos<BlobMeta, OverlayedChangeSetBlob>),
	) -> Option<(impl Iterator<Item = &'a [u8]>, &'a BlobMeta)> {
		let (blob, infos, ..) = blob;
		let infos = infos.value_ref();
		if infos.removed {
			return None
		}

		let (chunk_end, chunk_end_offset) = blob_chunk_end_index(infos.size);

		let nb_chunk = if chunk_end == 0 && chunk_end_offset == 0 { 0 } else { chunk_end + 1 };
		let iter = (0..nb_chunk).into_iter().map(move |at| {
			let end_chunk = if at == chunk_end { chunk_end_offset } else { BLOB_CHUNK_SIZE };
			let chunk = &blob.get(at).value_ref().as_ref()[..end_chunk];
			chunk
		});

		Some((iter, infos))
	}

	fn blob_chunks_into_committed(
		blob: OverlayedChangeSetBlob,
		infos: OverlayedInfos<BlobMeta, OverlayedChangeSetBlob>,
	) -> Option<(impl Iterator<Item = [u8; BLOB_CHUNK_SIZE]>, BlobMeta)> {
		let infos = infos.into_value();

		if !matches!(infos.infos.mode, Some(sp_core::storage::transient::Mode::Archive)) {
			return None
		}

		let (chunk_end, chunk_end_offset) = blob_chunk_end_index(infos.size);

		let nb_chunk = if chunk_end == 0 && chunk_end_offset == 0 { 0 } else { chunk_end + 1 };
		let iter = blob.into_iter().enumerate().filter_map(move |(at, entry)| {
			let at = at as u32;
			if at < nb_chunk {
				Some(entry.into_value())
			} else {
				None
			}
		});

		Some((iter, infos))
	}

	/// Return size of a given map when defined.
	pub fn map_count(&self, name: &[u8]) -> Option<u32> {
		let (_, infos, ..) = self.btrees.get(name)?;
		let infos = infos.value_ref();
		(!infos.removed).then(|| infos.size as u32)
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
				let info = map.1.value_ref();
				if info.removed {
					return Some(None)
				}
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
					if info.value_ref().removed {
						return false
					}
					let has_root = info.value_ref().root.is_some();
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
						let infos = info.modify_value(extrinsic_index);
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

	pub(crate) fn blob_set(&mut self, name: &[u8], value: &[u8], offset: u32) -> bool {
		let Some(previous_len) = self.blob_len(name) else {
			return false;
		};

		if offset > previous_len {
			return false
		};

		let extrinsic_index = self.extrinsic_index();

		let Some((changeset, info)) = self.blobs.get_mut(name) else {
			return false;
		};

		let end = offset as usize + value.len();
		if end > u32::MAX as usize {
			return false
		}

		if value.len() == 0 {
			return true
		}

		let (chunk_start, chunk_start_offset) = blob_chunk_start_index(offset as usize);
		let (chunk_end, chunk_end_offset) = blob_chunk_end_index(end);

		let update_size = end as u32 > previous_len;
		if update_size || info.value_ref().hash.is_some() {
			let info = info.modify_value(extrinsic_index);
			info.hash = None;
			if update_size {
				info.size = end;
			}
		}

		if chunk_start == chunk_end {
			let chunk = changeset.modify_chunk(chunk_start, extrinsic_index);
			chunk[chunk_start_offset..chunk_end_offset].copy_from_slice(value);
		} else {
			let chunk = changeset.modify_chunk(chunk_start, extrinsic_index);
			let mut offset = BLOB_CHUNK_SIZE - chunk_start_offset;
			chunk[chunk_start_offset..].copy_from_slice(&value[..offset]);
			for i in chunk_start + 1..chunk_end {
				let chunk = changeset.modify_chunk(i, extrinsic_index);
				chunk[..].copy_from_slice(&value[offset..offset + BLOB_CHUNK_SIZE]);
				offset += BLOB_CHUNK_SIZE;
			}
			let chunk = changeset.modify_chunk(chunk_end, extrinsic_index);
			chunk[..chunk_end_offset].copy_from_slice(&value[offset..]);
		}
		true
	}

	pub(crate) fn blob_truncate(&mut self, name: &[u8], new_len: u32) -> bool {
		let Some(previous_len) = self.blob_len(name) else {
			return false;
		};

		let extrinsic_index = self.extrinsic_index();

		let Some((changeset, info)) = self.blobs.get_mut(name) else {
			return false;
		};
		if new_len >= previous_len {
			return false
		};

		let (chunk_start, chunk_start_offset) = blob_chunk_start_index(new_len as usize);
		let chunk_start = if chunk_start_offset == 0 { chunk_start } else { chunk_start + 1 };

		changeset.truncate_clean(chunk_start);

		let info = info.modify_value(extrinsic_index);
		info.hash = None;
		info.size = new_len as usize;

		true
	}

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
			ChildInfo::Blob(info) => self.clear_blob_storage(&info.name, false).0,
			ChildInfo::BTree(info) => self.clear_btree_storage(&info.name, false).0,
		}
	}

	fn clear_blob_storage(
		&mut self,
		name: &[u8],
		move_old_state: bool,
	) -> (u32, Option<(OverlayedChangeSetBlob, bool, Option<Vec<u8>>)>) {
		let top = &self.top;
		let extrinsic_index = self.extrinsic_index();
		if let Some(entry) = self.blobs.get_mut(name) {
			if !entry.1.value_ref().removed {
				let info = entry.1.modify_value(extrinsic_index);
				let result = info.size;
				info.removed = true;
				info.size = 0;
				info.hash = None;
				let mut last = sp_std::mem::replace(&mut entry.0, top.spawn_blob());
				// TODO the guard_no_runtime_transaction may be covered by the need_keep
				let moved = if self.context.guard_no_runtime_transaction(&self.top).is_ok() {
					if let Some(current_key) = entry.1.take_is_previous_moved_from() {
						if move_old_state {
							Some((last, false, Some(current_key)))
						} else {
							let _ = last.rollback_transaction();
							entry.1.set_moved_from(current_key, Some(last));
							None
						}
					} else if entry.1.need_keep_origin() {
						if move_old_state {
							entry.1.set_moved_to();
							Some((last, true, None))
						} else {
							let _ = last.rollback_transaction();
							entry.1.set_last_removed(last);
							None
						}
					} else {
						move_old_state.then(|| (last, false, None))
					}
				} else {
					move_old_state.then(|| (last, false, None))
				};
				(result as u32, moved)
			} else {
				(0, None)
			}
		} else {
			(0, None)
		}
	}

	fn clear_btree_storage(
		&mut self,
		name: &[u8],
		move_old_state: bool,
	) -> (u32, Option<(OverlayedChangeSet, bool, Option<Vec<u8>>)>) {
		let top = &self.top;
		let extrinsic_index = self.extrinsic_index();
		if let Some(entry) = self.btrees.get_mut(name) {
			if !entry.1.value_ref().removed {
				let info = entry.1.modify_value(extrinsic_index);
				let result = info.size;
				info.removed = true;
				info.size = 0;
				info.root = None;
				let mut last = sp_std::mem::replace(&mut entry.0, top.spawn_child());
				// TODO the guard_no_runtime_transaction may be covered by the need_keep
				let moved = if self.context.guard_no_runtime_transaction(&self.top).is_ok() {
					if let Some(current_key) = entry.1.take_is_previous_moved_from() {
						if move_old_state {
							entry.1.set_moved_to();
							Some((last, false, Some(current_key)))
						} else {
							let _ = last.rollback_transaction();
							entry.1.set_moved_from(current_key, Some(last));
							None
						}
					} else if entry.1.need_keep_origin() {
						if move_old_state {
							entry.1.set_moved_to();
							Some((last, true, None))
						} else {
							let _ = last.rollback_transaction();
							entry.1.set_last_removed(last);
							None
						}
					} else {
						move_old_state.then(|| (last, false, None))
					}
				} else {
					move_old_state.then(|| (last, false, None))
				};
				(result as u32, moved)
			} else {
				(0, None)
			}
		} else {
			(0, None)
		}
	}

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
			changeset.start_transaction();
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

		let mut moved_from = Vec::new();
		retain_map(&mut self.btrees, |_, (changeset, infos)| {
			if let Some(from) = infos.take_is_previous_moved_from() {
				changeset
					.rollback_transaction()
					.expect("Top and children changesets are started in lockstep; qed");
				// Note this spawn is not worknig if origin is current, but moved from
				// implies Removed or Empty.
				let change = sp_std::mem::replace(changeset, self.top.spawn_child());
				moved_from.push((from, change));
			}
			if let Some(others) = infos.take_others_previous_moved_from() {
				moved_from.extend(others.into_iter());
			}
			if let Some(from) = infos.take_is_previous_moved_from() {
				changeset
					.rollback_transaction()
					.expect("Top and children changesets are started in lockstep; qed");
				// Note this spawn is not worknig if origin is current, but moved from
				// implies Removed or Empty.
				let change = sp_std::mem::replace(changeset, self.top.spawn_child());
				moved_from.push((from, change));
			}

			let mut clean = false;
			match infos.take_last_origin() {
				Some(OriginEntry::Empty) => {
					clean = true;
				},
				None | Some(OriginEntry::Current) => {
					changeset
						.rollback_transaction()
						.expect("Top and children changesets are started in lockstep; qed");
				},
				Some(OriginEntry::Removed(last_removed)) => {
					*changeset = last_removed;
				},
				Some(OriginEntry::MovedTo) => {
					// will get new info from moved from in second step
				},
			}
			let _ = infos
				.rollback_transaction()
				.expect("Top and children changesets are started in lockstep; qed");
			!clean
		});
		for (at, state) in moved_from.into_iter() {
			if let Some(entry) = self.btrees.get_mut(&at) {
				entry.0 = state;
			} else {
				crate::warn!(target: "runtime", "Moving to non existing entry should not happen.");
			}
		}

		let mut moved_from = Vec::new();
		retain_map(&mut self.blobs, |_, (changeset, infos)| {
			if let Some(from) = infos.take_is_previous_moved_from() {
				changeset
					.rollback_transaction()
					.expect("Top and children changesets are started in lockstep; qed");
				// Note this spawn is not worknig if origin is current, but moved from
				// implies Removed or Empty.
				let change = sp_std::mem::replace(changeset, self.top.spawn_blob());
				moved_from.push((from, change));
			}
			if let Some(others) = infos.take_others_previous_moved_from() {
				moved_from.extend(others.into_iter());
			}
			if let Some(from) = infos.take_is_previous_moved_from() {
				changeset
					.rollback_transaction()
					.expect("Top and children changesets are started in lockstep; qed");
				// Note this spawn is not worknig if origin is current, but moved from
				// implies Removed or Empty.
				let change = sp_std::mem::replace(changeset, self.top.spawn_blob());
				moved_from.push((from, change));
			}

			let mut clean = false;
			match infos.take_last_origin() {
				Some(OriginEntry::Empty) => {
					clean = true;
				},
				None | Some(OriginEntry::Current) => {
					changeset
						.rollback_transaction()
						.expect("Top and children changesets are started in lockstep; qed");
				},
				Some(OriginEntry::Removed(last_removed)) => {
					*changeset = last_removed;
				},
				Some(OriginEntry::MovedTo) => {
					// will get new info from moved from in second step
				},
			}
			let _ = infos
				.rollback_transaction()
				.expect("Top and children changesets are started in lockstep; qed");
			!clean
		});
		for (at, state) in moved_from.into_iter() {
			if let Some(entry) = self.blobs.get_mut(&at) {
				entry.0 = state;
			} else {
				crate::warn!(target: "runtime", "Moving to non existing entry should not happen.");
			}
		}

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
			infos
				.commit_transaction()
				.expect("Top and children changesets are started in lockstep; qed");
			changeset
				.commit_transaction()
				.expect("Top and children changesets are started in lockstep; qed");
		}

		for (_, (changeset, infos)) in self.blobs.iter_mut() {
			infos
				.commit_transaction()
				.expect("Top and children changesets are started in lockstep; qed");
			changeset
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
		let nb_open = self.top.nb_open_transactions(&self.context);
		self.context.exit_runtime()?;
		for _ in 0..nb_open {
			self.rollback_transaction().expect("counted above");
		}
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
			take(&mut self.top).drain_committed(),
			take(&mut self.children)
				.into_iter()
				.map(|(key, (val, info))| (key, (val.drain_committed(), info))),
			take(&mut self.btrees)
				.into_iter()
				.map(|(_key, (val, info))| (info.drain_committed(), val.drain_committed())),
			take(&mut self.blobs).into_iter().filter_map(|(_key, (val, info))| {
				if let Some((iter, info)) = Self::blob_chunks_into_committed(val, info) {
					// TODO this is too costy, just send the info not concatenated ???
					let blob: Blob = blob_from_chunks(iter, info.size);
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
	pub fn blob_storages(&self) -> impl Iterator<Item = (impl Iterator<Item = &[u8]>, &BlobMeta)> {
		self.blobs.iter().filter_map(|(_name, blob)| Self::blob_chunks_inner(blob))
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
		self.btrees.get(name).and_then(|(overlay, info)| {
			let info = info.value_ref();
			(!info.removed).then(|| (overlay.changes(), info))
		})
	}

	/// Get an list of all index operations.
	pub fn transaction_index_ops(&self) -> &[IndexOperation] {
		&self.transaction_index_ops
	}

	/// Convert this instance with all changes into a [`StorageChanges`] instance.
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
			btrees_changes: btrees_changes.map(|(info, it)| (info, it.collect())).collect(),
			blobs_changes: blobs_changes.map(|(info, blob)| (info, blob)).collect(),
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
			ChildInfo::BTree(_infos) => None,
			ChildInfo::Blob(_infos) => None,
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
				.map(|btree| !btree.1.value_ref().removed)
				.unwrap_or(false),
			ChildInfo::Blob(info) => self
				.blobs
				.get(info.name.as_slice())
				.map(|blob| !blob.1.value_ref().removed)
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
				if clear_existing {
					self.clear_btree_storage(&info.name, false);
				}
				let top = &self.top;
				let extrinsic_index = self.extrinsic_index();
				let entry = self.btrees.entry(info.name.clone()).or_insert_with(|| {
					(top.spawn_child(), top.spawn_info(BtreeMeta::from(info.clone())))
				});
				// even if same content we register and
				// update.
				let stored = entry.1.modify_value(extrinsic_index);
				if stored.infos.structure != info.structure {
					stored.root = None;
				}
				// TODO put in in a and_modify?? so only when existing entry?
				// TODO try update function is rather unclear: just explicitely
				// update algo and error on different name or different mode (if not None).
				let success = if stored.removed {
					stored.removed = false;
					stored.infos = info;
					true
				} else {
					stored.infos.try_update(&info)
				};
				if !success {
					crate::warn!(target: "runtime", "Try update failure should not happen.");
				}
				success
			},
			ChildInfo::Blob(info) => {
				if clear_existing {
					self.clear_blob_storage(&info.name, false);
				}
				let top = &self.top;
				let extrinsic_index = self.extrinsic_index();
				let entry = self.blobs.entry(info.name.clone()).or_insert_with(|| {
					(top.spawn_blob(), top.spawn_info(BlobMeta::from(info.clone())))
				});

				// even if same content we register and
				// update.
				let stored = entry.1.modify_value(extrinsic_index);
				if clear_existing {
					stored.size = 0;
					stored.hash = None;
				}
				if stored.infos.algorithm != info.algorithm {
					stored.hash = None;
				}

				let success = if stored.removed {
					stored.removed = false;
					stored.infos = info;
					true
				} else {
					stored.infos.try_update(&info)
				};
				if !success {
					crate::warn!(target: "runtime", "Try update failure should not happen.");
				}
				success
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
		let entry = entry.1.modify_value(extrinsic_index);
		debug_assert!(!entry.removed);
		entry.root = Some(root);
	}

	/// Cache hash.
	/// Do nothing on failure.
	pub fn cache_blob_hash(&mut self, name: &[u8], hash: [u8; 32]) {
		let extrinsic_index = self.extrinsic_index();
		let Some(entry) = self.blobs.get_mut(name) else {
			return
		};
		let entry = entry.1.modify_value(extrinsic_index);
		debug_assert!(!entry.removed);
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
				let info_dest = if let Some(origin) = self.btrees.get(&info.name) {
					let mut info_dest = origin.1.value_ref().clone();
					if info_dest.removed {
						return false
					};
					info_dest.infos.name = target_name.to_vec();
					info_dest
				} else {
					return false
				};

				self.clear_btree_storage(target_name, false);

				let Some(origin) = self.btrees.get(&info.name) else {
					return false
				};
				let origin = &origin.0 as *const OverlayedChangeSet;

				let top = &self.top;
				let entry = self.btrees.entry(target_name.to_vec()).or_insert_with(|| {
					(top.spawn_child(), top.spawn_info(BtreeMeta::from(info.clone())))
				});

				// even if same content we register and
				// update.
				let dest = entry.1.modify_value(extrinsic_index);
				*dest = info_dest;

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
				let info_dest = if let Some(origin) = self.blobs.get(&info.name) {
					let mut info_dest = origin.1.value_ref().clone();
					if info_dest.removed {
						return false
					};
					info_dest.infos.name = target_name.to_vec();
					info_dest
				} else {
					return false
				};

				self.clear_blob_storage(target_name, false);

				let Some(origin) = self.blobs.get(&info.name) else {
					return false
				};

				let origin = origin
					as *const (
						OverlayedChangeSetBlob,
						OverlayedInfos<BlobMeta, OverlayedChangeSetBlob>,
					);

				let top = &self.top;
				let entry = self.blobs.entry(target_name.to_vec()).or_insert_with(|| {
					(top.spawn_blob(), top.spawn_info(BlobMeta::from(info.clone())))
				});

				// even if same content we register and
				// update.
				let dest = entry.1.modify_value(extrinsic_index);
				*dest = info_dest;

				// Access pointer to origin as we know btree
				// implementation do not change this one when
				// he create an entry.
				let origin = unsafe { &*origin };
				if let Some(chunks) = Self::blob_chunks_inner(origin) {
					for (i, value) in chunks.0.enumerate() {
						let chunk = entry.0.modify_chunk(i as u32, extrinsic_index);
						chunk[..value.len()].copy_from_slice(value);
					}
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
			ChildInfo::BTree(info) => {
				if &info.name == target_name {
					return false
				}
				let extrinsic_index = self.extrinsic_index();
				let info_dest = if let Some(origin) = self.btrees.get(&info.name) {
					let mut info_dest = origin.1.value_ref().clone();
					if info_dest.removed {
						return false
					};
					info_dest.infos.name = target_name.to_vec();
					info_dest
				} else {
					return false
				};

				let Some((state, ref_origin, proxy_origin)) = self.clear_btree_storage(&info.name, true).1 else {
					return false
				};

				self.clear_btree_storage(target_name, false);

				let top = &self.top;
				let entry = self.btrees.entry(target_name.to_vec()).or_insert_with(|| {
					(top.spawn_child(), top.spawn_info(BtreeMeta::from(info.clone())))
				});

				// even if same content we register and
				// update.
				let dest = entry.1.modify_value(extrinsic_index);
				*dest = info_dest;

				entry.0 = state;
				if ref_origin {
					entry.1.set_moved_from(info.name.clone(), None);
				} else if let Some(from) = proxy_origin {
					entry.1.set_moved_from(from.clone(), None);
				}
				true
			},
			ChildInfo::Blob(info) => {
				if &info.name == target_name {
					return false
				}
				let extrinsic_index = self.extrinsic_index();
				let info_dest = if let Some(origin) = self.blobs.get(&info.name) {
					let mut info_dest = origin.1.value_ref().clone();
					if info_dest.removed {
						return false
					};
					info_dest.infos.name = target_name.to_vec();
					info_dest
				} else {
					return false
				};

				let Some((state, ref_origin, proxy_origin)) = self.clear_blob_storage(&info.name, true).1 else {
					return false
				};

				self.clear_blob_storage(target_name, false);

				let top = &self.top;
				let entry = self.blobs.entry(target_name.to_vec()).or_insert_with(|| {
					(top.spawn_blob(), top.spawn_info(BlobMeta::from(info.clone())))
				});

				// even if same content we register and
				// update.
				let dest = entry.1.modify_value(extrinsic_index);
				*dest = info_dest;

				entry.0 = state;
				if ref_origin {
					entry.1.set_moved_from(info.name.clone(), None);
				} else if let Some(from) = proxy_origin {
					entry.1.set_moved_from(from.clone(), None);
				}
				true
			},
		}
	}
}

#[cfg(feature = "std")]
impl From<sp_core::storage::Storage> for OverlayedChanges {
	fn from(storage: sp_core::storage::Storage) -> Self {
		let top: OverlayedChangeSet = storage.top.into();
		let btrees: Map<Name, (OverlayedChangeSet, OverlayedInfos<BtreeMeta, OverlayedChangeSet>)> =
			storage
				.btree_storages
				.into_iter()
				// TODO overlay also with btree infos and root
				// TODO add btree info in overlayedchangeset?
				.map(|(k, v)| (k, (v.data.into(), top.spawn_info(BtreeMeta::from(v.info)))))
				.collect();
		let blobs: Map<
			Name,
			(OverlayedChangeSetBlob, OverlayedInfos<BlobMeta, OverlayedChangeSetBlob>),
		> = storage
			.blob_storages
			.into_iter()
			.map(|(k, v)| (k, (v.data.into(), top.spawn_info(BlobMeta::from(v.info)))))
			.collect();
		Self {
			context: Default::default(),
			top,
			children: storage
				.children_default
				.into_iter()
				.map(|(k, v)| (k, (v.data.into(), v.child_info)))
				.collect(),
			btrees,
			blobs,
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

fn blob_chunk_start_index(start: usize) -> (u32, usize) {
	((start / BLOB_CHUNK_SIZE) as u32, start % BLOB_CHUNK_SIZE)
}
fn blob_chunk_end_index(end: usize) -> (u32, usize) {
	if end == 0 {
		return (0, 0)
	}
	let chunk_end = end / BLOB_CHUNK_SIZE;
	let chunk_end_offset = end % BLOB_CHUNK_SIZE;
	if chunk_end_offset == 0 {
		(chunk_end as u32 - 1, BLOB_CHUNK_SIZE)
	} else {
		(chunk_end as u32, chunk_end_offset)
	}
}

pub(crate) fn blob_from_chunks<B: AsRef<[u8]>>(
	mut iter: impl Iterator<Item = B>,
	size: usize,
) -> Vec<u8> {
	let mut result = Vec::with_capacity(size);
	while let Some(chunk) = iter.next() {
		if (size - result.len()) < BLOB_CHUNK_SIZE {
			result.extend_from_slice(&chunk.as_ref()[..size]);
			break
		} else {
			result.extend_from_slice(&chunk.as_ref()[..]);
		}
	}
	debug_assert!(iter.next().is_none());
	result
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
		let mut overlay = OverlayedChanges::default();
		overlay.start_transaction();
		overlay.set_child_storage(child_info, &[20], Some(&[20]));
		overlay.set_child_storage(child_info, &[30], Some(&[30]));
		overlay.set_child_storage(child_info, &[40], Some(&[40]));
		overlay.commit_transaction().unwrap();
		overlay.set_child_storage(child_info, &[10], Some(&[10]));
		overlay.set_child_storage(child_info, &[30], None);

		// next_prospective < next_committed
		let next_to_5 = overlay.child_iter_after(child_info, &[5]).unwrap().next().unwrap();
		assert_eq!(next_to_5.0.to_vec(), vec![10]);
		assert_eq!(next_to_5.1.value(), Some(&vec![10]));

		// next_committed < next_prospective
		let next_to_10 = overlay.child_iter_after(child_info, &[10]).unwrap().next().unwrap();
		assert_eq!(next_to_10.0.to_vec(), vec![20]);
		assert_eq!(next_to_10.1.value(), Some(&vec![20]));

		// next_committed == next_prospective
		let next_to_20 = overlay.child_iter_after(child_info, &[20]).unwrap().next().unwrap();
		assert_eq!(next_to_20.0.to_vec(), vec![30]);
		assert_eq!(next_to_20.1.value(), None);

		// next_committed, no next_prospective
		let next_to_30 = overlay.child_iter_after(child_info, &[30]).unwrap().next().unwrap();
		assert_eq!(next_to_30.0.to_vec(), vec![40]);
		assert_eq!(next_to_30.1.value(), Some(&vec![40]));

		overlay.set_child_storage(child_info, &[50], Some(&[50]));
		// next_prospective, no next_committed
		let next_to_40 = overlay.child_iter_after(child_info, &[40]).unwrap().next().unwrap();
		assert_eq!(next_to_40.0.to_vec(), vec![50]);
		assert_eq!(next_to_40.1.value(), Some(&vec![50]));
	}
}
