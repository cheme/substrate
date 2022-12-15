// This file is part of Substrate.

// Copyright (C) 2017-2022 Parity Technologies (UK) Ltd.
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

//! Client db storage to handle transient storage info.
//!
//! This provides a default option storage backend and a way to
//! plug external code to handle this information.

use crate::{columns, Database, DbHash, PruningMode};
use codec::{Decode, Encode};
use log::{debug, warn};
use sp_database::Transaction;
use sp_runtime::traits::{Block as BlockT, NumberFor};
use sp_state_machine::{BlobsCollection, BtreesCollection};
use std::{collections::HashMap, sync::Arc};

/// A simple implementation of transient data indexing.
/// This is meant to be use by default when no better implementation
/// is attached, but do not try to be performant.
pub(super) struct TransientArchive<'a, Block: BlockT> {
	pub db: Arc<dyn Database<DbHash>>,
	pub hash: &'a Block::Hash,
	pub parent_hash: &'a Block::Hash,
	pub number: NumberFor<Block>,
	pub in_mem: &'a mut TransientArchiveInMemory<Block>,
}

/// In memory storage of transient data.
/// This is memory buffer before storing.
/// Keep non-canonical infos.
pub(super) struct TransientArchiveInMemory<Block: BlockT> {
	infos: HashMap<Block::Hash, TransientBlockInfo<Block>>,
	// TODO consider using its own configuration
	pruning: PruningMode,
}

// not send transient ptr should not leak.
unsafe impl<Block: BlockT + Send> Send for TransientArchiveInMemory<Block> {}

// not sync transient ptr should not leak.
unsafe impl<Block: BlockT + Sync> Sync for TransientArchiveInMemory<Block> {}

impl<Block: BlockT> TransientArchiveInMemory<Block> {
	/// New in memory archive storage.
	pub fn new(pruning: PruningMode) -> Self {
		TransientArchiveInMemory { infos: HashMap::new(), pruning }
	}
}

/*
/// Pointer in in memory chain struct just *mut.
/// Could switch to safe `Arc<RwLock` later or less unsafe `Rc<Cell`.
type TransientPtr<B> = Option<*mut TransientBlockInfo<B>>;
*/

/// Simple struct to keep trace of chain structure
/// locally.
/// The api could use fork tree, but this is also
/// meant to be a simple example for other indexer.
/// Some pointer are not necessary but will be if a persistence
/// is needed (and we store deltas).
struct TransientBlockInfo<Block: BlockT> {
	hash: Block::Hash,
	number: NumberFor<Block>, // Not strictly neccessary.
	/*
	parent_siblings: TransientPtr<Block>,
	direct_parent: TransientPtr<Block>,
	children_siblings: TransientPtr<Block>,
	direct_children: Vec<TransientPtr<Block>>,
	next_siblings: TransientPtr<Block>,
	prev_siblings: TransientPtr<Block>,
	*/
	btrees_changes: BtreesCollection,
	blobs_changes: BlobsCollection,
}

#[derive(Encode, Decode)]
enum JournalEntry {
	NewName(Vec<u8>),
	NewKey(Vec<u8>),
}

impl<'a, Block: BlockT> TransientArchive<'a, Block> {
	/// Write btrees and blobs changes.
	pub fn add_transient_changes(
		&mut self,
		btrees_changes: BtreesCollection,
		blobs_changes: BlobsCollection,
		tx: &mut Transaction<DbHash>,
	) {
		if self.in_mem.pruning.is_archive() {
			// just write, no checks TODO same if range is long (see latest change), but then need
			// to journal for removal of other on canonicalization
			let mut journal = vec![0u8]; // version 0
			self.write_btrees(btrees_changes, tx, &mut journal);
			self.write_blobs(blobs_changes, tx, &mut journal);
			tx.set_from_vec(columns::AUX, &Self::journal_key(self.hash), journal);
			return
		}
		let info = TransientBlockInfo {
			hash: self.hash.clone(),
			number: self.number.clone(),
			btrees_changes,
			blobs_changes,
		};

		if self.in_mem.infos.insert(self.hash.clone(), info).is_some() {
			warn!(
				"Adding transient info twice for a same block at {:?}: {:?}",
				self.hash, self.number
			);
		}
	}

	/// Block canonicalization call.
	pub fn canonicalize(&mut self, tx: &mut Transaction<DbHash>) {
		if self.in_mem.pruning.is_archive() {
			// done in `add_transient_changes`. TODO for long history read from db and prune.
			return
		}
		if let Some(info) = self.in_mem.infos.remove(&self.hash) {
			let mut journal = vec![0u8]; // version 0
			self.write_btrees(info.btrees_changes, tx, &mut journal);
			self.write_blobs(info.blobs_changes, tx, &mut journal);
			tx.set_from_vec(columns::AUX, &Self::journal_key(self.hash), journal);
		} else {
			warn!("Canonicalize miss transient info at {:?}: {:?}", self.hash, self.number);
		}
	}

	// TODO consider calling drop and prune from canonicalize instead, but
	// would require checking if cannonical for ArchiveCanonical.
	// And then have custom pruning duration here?
	/// Out of state pruning range.
	pub(super) fn drop_or_prune(&mut self, tx: &mut Transaction<DbHash>) {
		match self.in_mem.pruning {
			PruningMode::ArchiveAll => return,
			_ => (),
		}
		let journal_key = Self::journal_key(self.hash);
		let Some(journal) = self.db.get(columns::AUX, &journal_key) else {
			debug!("Missing journal, skipping pruning {:?}", self.hash);
			return;
		};
		tx.remove(columns::AUX, &journal_key);
		let mut read_journal = journal.as_slice();
		let mut current_btree: Option<Vec<u8>> = None;
		let Some(version) = read_journal.get(0) else {
			warn!("Invalid journal encoding, skipping pruning {:?}", self.hash);
			return
		};
		read_journal = &read_journal[1..];
		if *version != 0 {
			warn!("Unexepected journal version {:?}, skipping pruning {:?}", version, self.hash);
			return
		}
		while let Some(type_item) = read_journal.get(0) {
			read_journal = &read_journal[1..];
			match type_item {
				0u8 => {
					let Ok(name) = Vec::<u8>::decode(&mut read_journal) else {
						warn!("Invalid journal encoding, skipping pruning {:?}", self.hash);
						return
					};
					current_btree = Some(name);
				},
				1u8 => {
					// Note all decode here could be replace by just decoding size
					// to avoid instantiating a vec.
					let Ok(name) = Vec::<u8>::decode(&mut read_journal) else {
						warn!("Invalid journal encoding, skipping pruning {:?}", self.hash);
						return
					};
					current_btree = None;
					let key = Self::blob_key(self.hash, name.as_slice());
					tx.remove(columns::AUX, &key);
				},
				2u8 => {
					let Ok(key) = Vec::<u8>::decode(&mut read_journal) else {
						warn!("Invalid journal encoding, skipping pruning {:?}", self.hash);
						return
					};
					let Some(name) = current_btree.as_ref() else {
						warn!("Invalid journal encoding, skipping pruning {:?}", self.hash);
						return
					};
					let key = Self::btree_key(self.hash, name, &key);
					tx.remove(columns::AUX, &key);
				},
				_ => {
					warn!("Invalid journal encoding, skipping pruning {:?}", self.hash);
					return
				},
			}
		}
	}

	fn write_btrees(
		&mut self,
		btrees_changes: BtreesCollection,
		tx: &mut Transaction<DbHash>,
		journal: &mut Vec<u8>,
	) {
		for (info, changes) in btrees_changes {
			match info.infos.mode {
				Some(sp_core::storage::transient::Mode::Archive) => (),
				Some(sp_core::storage::transient::Mode::Drop) | None => continue,
			}
			if info.removed {
				// Note that there is currently no sense to have removed
				// info, but will be needed in case of a non transient mode.
				continue
			}
			let name = info.infos.name.encode();
			journal.push(0u8);
			journal.extend_from_slice(name.as_slice());
			let mut values = std::collections::BTreeSet::new();
			for (key, change) in changes {
				if let Some(change) = change {
					tx.set_from_vec(
						columns::AUX,
						&Self::btree_key(self.hash, name.as_slice(), key.as_slice()),
						change,
					);
					values.insert(key);
				} else {
					// transient, this is not needed at this point.
				}
			}
			// Note that empty btrees are only lookable through journal
			for key in values.iter() {
				journal.push(2u8);
				key.encode_to(journal);
			}
		}
	}

	fn write_blobs(
		&mut self,
		blobs_changes: BlobsCollection,
		tx: &mut Transaction<DbHash>,
		journal: &mut Vec<u8>,
	) {
		for (info, blob) in blobs_changes {
			match info.infos.mode {
				Some(sp_core::storage::transient::Mode::Archive) => (),
				Some(sp_core::storage::transient::Mode::Drop) | None => continue,
			}
			if info.removed {
				// Note that there is currently no sense to have removed
				// info, but will be needed in case of a non transient mode.
				continue
			}
			let name = &info.infos.name;
			journal.push(1u8);
			name.encode_to(journal);
			journal.extend_from_slice(name.as_slice());
			tx.set_from_vec(columns::AUX, &Self::blob_key(self.hash, name.as_slice()), blob);
		}
	}

	fn journal_key(hash: &Block::Hash) -> Vec<u8> {
		let mut key = Vec::with_capacity(32 + 16);
		key.extend_from_slice(b"TransientJournal");
		key.extend_from_slice(hash.as_ref());
		key
	}

	fn btree_key(hash: &Block::Hash, encoded_name: &[u8], btree_key: &[u8]) -> Vec<u8> {
		let mut key = Vec::with_capacity(32 + 19 + encoded_name.len() + btree_key.len());
		key.extend_from_slice(b"TransientBtreesItem");
		key.extend_from_slice(hash.as_ref());
		key.extend_from_slice(encoded_name);
		key.extend_from_slice(btree_key);
		key
	}

	fn blob_key(hash: &Block::Hash, blob_key: &[u8]) -> Vec<u8> {
		let mut key = Vec::with_capacity(32 + 18 + blob_key.len());
		key.extend_from_slice(b"TransientBlobsItem");
		key.extend_from_slice(hash.as_ref());
		key.extend_from_slice(blob_key);
		key
	}
}

/// Callback hooks for a transient indexer.
pub trait TransientIndexer<Block> {}

/*
 * dyn call proto:
 * supported api version
 * unsafe extern fn() -> u32
 * get dyn trait
 * unsafe extern fn(PruningMode) -> dyn TransientIndexer
 *TransientArchiveInMemory<Block>
 */
