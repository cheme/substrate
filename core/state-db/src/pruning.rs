// Copyright 2017-2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Pruning window.
//!
//! For each block we maintain a list of nodes pending deletion.
//! There is also a global index of node key to block number.
//! If a node is re-inserted into the window it gets removed from
//! the death list.
//! The changes are journaled in the DB.

use std::collections::{HashMap, HashSet, VecDeque};
use codec::{Encode, Decode};
use crate::{CommitSet, CommitSetCanonical, Error, MetaDb, to_meta_key, Hash,
	OffstateKey};
use log::{trace, warn};

const LAST_PRUNED: &[u8] = b"last_pruned";
const PRUNING_JOURNAL: &[u8] = b"pruning_journal";
const OFFSTATE_PRUNING_JOURNAL: &[u8] = b"offstate_pruning_journal";

/// See module documentation.
pub struct RefWindow<BlockHash: Hash, Key: Hash> {
	/// A queue of keys that should be deleted for each block in the pruning window.
	death_rows: VecDeque<DeathRow<BlockHash, Key>>,
	/// An index that maps each key from `death_rows` to block number.
	death_index: HashMap<Key, u64>,
	/// Block number that corresponts to the front of `death_rows`
	pending_number: u64,
	/// Number of call of `note_canonical` after
	/// last call `apply_pending` or `revert_pending`
	pending_canonicalizations: usize,
	/// Number of calls of `prune_one` after
	/// last call `apply_pending` or `revert_pending`
	pending_prunings: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct DeathRow<BlockHash: Hash, Key: Hash> {
	hash: BlockHash,
	journal_key: Vec<u8>,
	offstate_journal_key: Vec<u8>,
	deleted: HashSet<Key>,
	// TODO EMCH for offstate there is no need to put
	// in memory so we can make it lazy (load from
	// pruning journal on actual prune).
	offstate_modified: HashSet<OffstateKey>,
}

#[derive(Encode, Decode)]
struct JournalRecord<BlockHash: Hash, Key: Hash> {
	hash: BlockHash,
	inserted: Vec<Key>,
	deleted: Vec<Key>,
}

#[derive(Encode, Decode)]
struct OffstateJournalRecord {
	modified: Vec<OffstateKey>,
}

fn to_journal_key(block: u64) -> Vec<u8> {
	to_meta_key(PRUNING_JOURNAL, &block)
}

fn to_offstate_journal_key(block: u64) -> Vec<u8> {
	to_meta_key(OFFSTATE_PRUNING_JOURNAL, &block)
}

impl<BlockHash: Hash, Key: Hash> RefWindow<BlockHash, Key> {
	pub fn new<D: MetaDb>(db: &D) -> Result<RefWindow<BlockHash, Key>, Error<D::Error>> {
		let last_pruned = db.get_meta(&to_meta_key(LAST_PRUNED, &()))
			.map_err(|e| Error::Db(e))?;
		let pending_number: u64 = match last_pruned {
			Some(buffer) => u64::decode(&mut buffer.as_slice())? + 1,
			None => 0,
		};
		let mut block = pending_number;
		let mut pruning = RefWindow {
			death_rows: Default::default(),
			death_index: Default::default(),
			pending_number: pending_number,
			pending_canonicalizations: 0,
			pending_prunings: 0,
		};
		// read the journal
		trace!(target: "state-db", "Reading pruning journal. Pending #{}", pending_number);
		loop {
			let journal_key = to_journal_key(block);
			let offstate_journal_key = to_offstate_journal_key(block);
			match db.get_meta(&journal_key).map_err(|e| Error::Db(e))? {
				Some(record) => {
					let record: JournalRecord<BlockHash, Key> = Decode::decode(&mut record.as_slice())?;
					let offstate_record_inserted = if let Some(record) = db
						.get_meta(&offstate_journal_key).map_err(|e| Error::Db(e))? {
						let record = OffstateJournalRecord::decode(&mut record.as_slice())?;
						record.modified
					} else { Vec::new() };
	
					trace!(
						target: "state-db",
						"Pruning journal entry {} ({} {} inserted, {} deleted)",
						block,
						record.inserted.len(),
						offstate_record_inserted.len(),
						record.deleted.len(),
					);
					pruning.import(
						&record.hash,
						journal_key,
						offstate_journal_key,
						record.inserted.into_iter(),
						record.deleted,
						offstate_record_inserted.into_iter(),
					);
				},
				None => break,
			}
			block += 1;
		}
		Ok(pruning)
	}

	fn import<I: IntoIterator<Item=Key>, I2: IntoIterator<Item=OffstateKey>>(
		&mut self,
		hash: &BlockHash,
		journal_key: Vec<u8>,
		offstate_journal_key: Vec<u8>,
		inserted: I,
		deleted: Vec<Key>,
		offstate_modified: I2,
	) {
		// remove all re-inserted keys from death rows
		for k in inserted {
			if let Some(block) = self.death_index.remove(&k) {
				self.death_rows[(block - self.pending_number) as usize].deleted.remove(&k);
			}
		}

		// add new keys
		let imported_block = self.pending_number + self.death_rows.len() as u64;
		for k in deleted.iter() {
			self.death_index.insert(k.clone(), imported_block);
		}
			// TODO EMCH is it possible to change type to directly set ??
		let offstate_modified = offstate_modified.into_iter().collect();
		self.death_rows.push_back(
			DeathRow {
				hash: hash.clone(),
				deleted: deleted.into_iter().collect(),
				offstate_modified,
				journal_key,
				offstate_journal_key,
			}
		);
	}

	pub fn window_size(&self) -> u64 {
		(self.death_rows.len() - self.pending_prunings) as u64
	}

	pub fn next_hash(&self) -> Option<BlockHash> {
		self.death_rows.get(self.pending_prunings).map(|r| r.hash.clone())
	}

	pub fn mem_used(&self) -> usize {
		0
	}

	pub fn pending(&self) -> u64 {
		self.pending_number + self.pending_prunings as u64
	}

	pub fn have_block(&self, hash: &BlockHash) -> bool {
		self.death_rows.iter().skip(self.pending_prunings).any(|r| r.hash == *hash)
	}

	/// Prune next block. Expects at least one block in the window.
	/// Adds changes to `commit`.
	/// `offstate_prune` to None indicates archive mode.
	pub fn prune_one(
		&mut self,
		commit: &mut CommitSetCanonical<Key>,
	) {
		let (commit, offstate_prune) = commit;
		if let Some(pruned) = self.death_rows.get(self.pending_prunings) {
			trace!(target: "state-db", "Pruning {:?} ({} deleted)", pruned.hash, pruned.deleted.len());
			let index = self.pending_number + self.pending_prunings as u64;
			commit.data.deleted.extend(pruned.deleted.iter().cloned());
			if let Some(offstate) = offstate_prune.as_mut() {
				offstate.0 = std::cmp::max(offstate.0, index);
				offstate.1.extend(pruned.offstate_modified.iter().cloned());
			} else {
				*offstate_prune = Some((
					index,
					pruned.offstate_modified.iter().cloned().collect(),
				));
			}
			commit.meta.inserted.push((to_meta_key(LAST_PRUNED, &()), index.encode()));
			commit.meta.deleted.push(pruned.journal_key.clone());
			commit.meta.deleted.push(pruned.offstate_journal_key.clone());
			self.pending_prunings += 1;
		} else {
			warn!(target: "state-db", "Trying to prune when there's nothing to prune");
		}
	}

	/// Add a change set to the window. Creates a journal record and pushes it to `commit`
	pub fn note_canonical(&mut self, hash: &BlockHash, commit: &mut CommitSet<Key>) {
		trace!(target: "state-db", "Adding to pruning window: {:?} ({} inserted, {} deleted)", hash, commit.data.inserted.len(), commit.data.deleted.len());
		let inserted = commit.data.inserted.iter().map(|(k, _)| k.clone()).collect();
		let offstate_modified = commit.offstate.iter().map(|(k, _)| k.clone()).collect();
		let deleted = ::std::mem::replace(&mut commit.data.deleted, Vec::new());
		let journal_record = JournalRecord {
			hash: hash.clone(),
			inserted,
			deleted,
		};
		let offstate_journal_record = OffstateJournalRecord {
			modified: offstate_modified,
		};
		let block = self.pending_number + self.death_rows.len() as u64;
		let journal_key = to_journal_key(block);
		let offstate_journal_key = to_offstate_journal_key(block);
		commit.meta.inserted.push((journal_key.clone(), journal_record.encode()));
		commit.meta.inserted.push((offstate_journal_key.clone(), offstate_journal_record.encode()));
		self.import(
			&journal_record.hash,
			journal_key,
			offstate_journal_key,
			journal_record.inserted.into_iter(),
			journal_record.deleted,
			offstate_journal_record.modified.into_iter(),
		);
		self.pending_canonicalizations += 1;
	}

	/// Apply all pending changes
	pub fn apply_pending(&mut self) {
		self.pending_canonicalizations = 0;
		for _ in 0 .. self.pending_prunings {
			let pruned = self.death_rows.pop_front().expect("pending_prunings is always < death_rows.len()");
			trace!(target: "state-db", "Applying pruning {:?} ({} deleted)", pruned.hash, pruned.deleted.len());
			for k in pruned.deleted.iter() {
				self.death_index.remove(&k);
			}
			self.pending_number += 1;
		}
		self.pending_prunings = 0;
	}

	/// Revert all pending changes
	pub fn revert_pending(&mut self) {
		// Revert pending deletions.
		// Note that pending insertions might cause some existing deletions to be removed from `death_index`
		// We don't bother to track and revert that for now. This means that a few nodes might end up no being
		// deleted in case transaction fails and `revert_pending` is called.
		self.death_rows.truncate(self.death_rows.len() - self.pending_canonicalizations);
		let new_max_block = self.death_rows.len() as u64 + self.pending_number;
		self.death_index.retain(|_, block| *block < new_max_block);
		self.pending_canonicalizations = 0;
		self.pending_prunings = 0;
	}
}

#[cfg(test)]
mod tests {
	use super::RefWindow;
	use primitives::H256;
	use crate::CommitSetCanonical;
	use crate::test::{make_db, make_commit_both, TestDb, make_commit};

	fn check_journal(pruning: &RefWindow<H256, H256>, db: &TestDb) {
		let restored: RefWindow<H256, H256> = RefWindow::new(db).unwrap();
		assert_eq!(pruning.pending_number, restored.pending_number);
		assert_eq!(pruning.death_rows, restored.death_rows);
		assert_eq!(pruning.death_index, restored.death_index);
	}

	#[test]
	fn created_from_empty_db() {
		let db = make_db(&[]);
		let pruning: RefWindow<H256, H256> = RefWindow::new(&db).unwrap();
		assert_eq!(pruning.pending_number, 0);
		assert!(pruning.death_rows.is_empty());
		assert!(pruning.death_index.is_empty());
		assert!(pruning.pending_prunings == 0);
		assert!(pruning.pending_canonicalizations == 0);
	}

	#[test]
	fn prune_one() {
		let mut db = make_db(&[1, 2, 3]);
		db.initialize_offstate(&[1, 2, 3]);
		let mut pruning: RefWindow<H256, H256> = RefWindow::new(&db).unwrap();
		let mut commit = (make_commit_both(&[4, 5], &[1, 3]), None);
		commit.0.initialize_offstate(&[4, 5], &[1, 3]);
		let h = H256::random();
		assert!(!commit.0.data.deleted.is_empty());
		pruning.note_canonical(&h, &mut commit.0);
		db.commit_canonical(&commit);
		assert!(pruning.have_block(&h));
		pruning.apply_pending();
		assert!(pruning.have_block(&h));
		assert!(commit.0.data.deleted.is_empty());
		//assert!(commit.offstate.is_empty());
		assert_eq!(pruning.death_rows.len(), 1);
		assert_eq!(pruning.death_index.len(), 2);
		assert!(db.data_eq(&make_db(&[1, 2, 3, 4, 5])));
		assert!(db.offstate_eq_at(&[1, 2, 3], Some(0)));
		assert!(db.offstate_eq(&[2, 4, 5]));
		check_journal(&pruning, &db);

		let mut commit = CommitSetCanonical::default();
		pruning.prune_one(&mut commit);
		assert!(!pruning.have_block(&h));
		db.commit_canonical(&commit);
		pruning.apply_pending();
		assert!(!pruning.have_block(&h));
		assert!(db.data_eq(&make_db(&[2, 4, 5])));
		// two remains since it is still valid next
		assert!(db.offstate_eq_at(&[2], Some(0)));
		assert!(db.offstate_eq(&[2, 4, 5]));
		assert!(pruning.death_rows.is_empty());
		assert!(pruning.death_index.is_empty());
		assert_eq!(pruning.pending_number, 1);
	}

	#[test]
	fn prune_two() {
		let mut db = make_db(&[1, 2, 3]);
		db.initialize_offstate(&[1, 2, 3]);
		let mut pruning: RefWindow<H256, H256> = RefWindow::new(&db).unwrap();
		let mut commit = make_commit_both(&[3, 4], &[1]);
		pruning.note_canonical(&H256::random(), &mut commit);
		db.commit(&commit);
		let mut commit = make_commit_both(&[5], &[2]);
		pruning.note_canonical(&H256::random(), &mut commit);
		db.commit(&commit);
		pruning.apply_pending();
		assert!(db.data_eq(&make_db(&[1, 2, 3, 4, 5])));
		assert!(db.offstate_eq_at(&[1, 2, 3], Some(0)));
		assert!(db.offstate_eq_at(&[2, 3, 4], Some(1)));
		assert!(db.offstate_eq(&[3, 4, 5]));

		check_journal(&pruning, &db);

		let mut commit = CommitSetCanonical::default();
		pruning.prune_one(&mut commit);
		db.commit_canonical(&commit);
		pruning.apply_pending();
		assert!(db.data_eq(&make_db(&[2, 3, 4, 5])));
		// 3 exists at 0 and 1 so 0 removed
		assert!(db.offstate_eq_at(&[2], Some(0)));
		assert!(db.offstate_eq_at(&[2, 3, 4], Some(1)));
		assert!(db.offstate_eq(&[3, 4, 5]));
		let mut commit = CommitSetCanonical::default();
		pruning.prune_one(&mut commit);
		db.commit_canonical(&commit);
		pruning.apply_pending();
		assert!(db.data_eq(&make_db(&[3, 4, 5])));
		assert!(db.offstate_eq_at(&[], Some(0)));
		assert!(db.offstate_eq_at(&[3, 4], Some(1)));
		assert!(db.offstate_eq(&[3, 4, 5]));
		assert_eq!(pruning.pending_number, 2);
	}

	#[test]
	fn prune_two_pending() {
		let mut db = make_db(&[1, 2, 3]);
		db.initialize_offstate(&[1, 2, 3]);
		let mut pruning: RefWindow<H256, H256> = RefWindow::new(&db).unwrap();
		let mut commit = (make_commit_both(&[4], &[1]), None);
		pruning.note_canonical(&H256::random(), &mut commit.0);
		db.commit_canonical(&commit);
		let mut commit = (make_commit_both(&[3, 5], &[2]), None);
		pruning.note_canonical(&H256::random(), &mut commit.0);
		db.commit_canonical(&commit);
		assert!(db.data_eq(&make_db(&[1, 2, 3, 4, 5])));
		assert!(db.offstate_eq_at(&[1, 2, 3], Some(0)));
		assert!(db.offstate_eq_at(&[2, 3, 4], Some(1)));
		assert!(db.offstate_eq(&[3, 4, 5]));
		let mut commit = CommitSetCanonical::default();
		pruning.prune_one(&mut commit);
		db.commit_canonical(&commit);
		assert!(db.data_eq(&make_db(&[2, 3, 4, 5])));
		assert!(db.offstate_eq_at(&[2, 3], Some(0)));
		assert!(db.offstate_eq_at(&[2, 3, 4], Some(1)));
		assert!(db.offstate_eq(&[3, 4, 5]));
		let mut commit = CommitSetCanonical::default();
		pruning.prune_one(&mut commit);
		db.commit_canonical(&commit);
		pruning.apply_pending();
		assert!(db.data_eq(&make_db(&[3, 4, 5])));
		assert!(db.offstate_eq_at(&[], Some(0)));
		assert!(db.offstate_eq_at(&[4], Some(1)));
		assert!(db.offstate_eq(&[3, 4, 5]));
		assert_eq!(pruning.pending_number, 2);
	}

	#[test]
	fn reinserted_survives() {
		let mut db = make_db(&[1, 2, 3]);
		db.initialize_offstate(&[1, 2, 3]);
		let mut pruning: RefWindow<H256, H256> = RefWindow::new(&db).unwrap();
		let mut commit = (make_commit_both(&[], &[2]), None);
		commit.0.initialize_offstate(&[], &[2]);
		pruning.note_canonical(&H256::random(), &mut commit.0);
		db.commit_canonical(&commit);
		let mut commit = (make_commit_both(&[2], &[]), None);
		pruning.note_canonical(&H256::random(), &mut commit.0);
		db.commit_canonical(&commit);
		let mut commit = (make_commit_both(&[], &[2]), None);
		pruning.note_canonical(&H256::random(), &mut commit.0);
		db.commit_canonical(&commit);
		assert!(db.data_eq(&make_db(&[1, 2, 3])));
		assert!(db.offstate_eq_at(&[1, 2, 3], Some(0)));
		assert!(db.offstate_eq_at(&[1, 3], Some(1)));
		assert!(db.offstate_eq_at(&[1, 2, 3], Some(2)));
		assert!(db.offstate_eq(&[1, 3]));
		pruning.apply_pending();

		check_journal(&pruning, &db);

		let mut commit = CommitSetCanonical::default();
		pruning.prune_one(&mut commit);
		db.commit_canonical(&commit);
		assert!(db.data_eq(&make_db(&[1, 2, 3])));
		assert!(db.offstate_eq_at(&[1, 3], Some(0)));
		assert!(db.offstate_eq_at(&[1, 3], Some(1)));
		assert!(db.offstate_eq_at(&[1, 2, 3], Some(2)));
		assert!(db.offstate_eq(&[1, 3]));
		let mut commit = CommitSetCanonical::default();
		pruning.prune_one(&mut commit);
		db.commit_canonical(&commit);
		assert!(db.data_eq(&make_db(&[1, 2, 3])));
		assert!(db.offstate_eq_at(&[1, 3], Some(0)));
		assert!(db.offstate_eq_at(&[1, 3], Some(1)));
		assert!(db.offstate_eq_at(&[1, 2, 3], Some(2)));
		assert!(db.offstate_eq(&[1, 3]));
		pruning.prune_one(&mut commit);
		db.commit_canonical(&commit);
		assert!(db.data_eq(&make_db(&[1, 3])));
		assert!(db.offstate_eq_at(&[1, 3], Some(0)));
		assert!(db.offstate_eq_at(&[1, 3], Some(1)));
		assert!(db.offstate_eq_at(&[1, 3], Some(2)));
		assert!(db.offstate_eq(&[1, 3]));
		pruning.apply_pending();
		assert_eq!(pruning.pending_number, 3);
	}

	#[test]
	fn reinserted_survivew_pending() {
		let mut db = make_db(&[1, 2, 3]);
		let mut pruning: RefWindow<H256, H256> = RefWindow::new(&db).unwrap();
		let mut commit = make_commit(&[], &[2]);
		pruning.note_canonical(&H256::random(), &mut commit);
		db.commit(&commit);
		let mut commit = make_commit(&[2], &[]);
		pruning.note_canonical(&H256::random(), &mut commit);
		db.commit(&commit);
		let mut commit = make_commit(&[], &[2]);
		pruning.note_canonical(&H256::random(), &mut commit);
		db.commit(&commit);
		assert!(db.data_eq(&make_db(&[1, 2, 3])));

		let mut commit = CommitSetCanonical::default();
		pruning.prune_one(&mut commit);
		db.commit_canonical(&commit);
		assert!(db.data_eq(&make_db(&[1, 2, 3])));
		let mut commit = CommitSetCanonical::default();
		pruning.prune_one(&mut commit);
		db.commit_canonical(&commit);
		assert!(db.data_eq(&make_db(&[1, 2, 3])));
		pruning.prune_one(&mut commit);
		db.commit_canonical(&commit);
		assert!(db.data_eq(&make_db(&[1, 3])));
		pruning.apply_pending();
		assert_eq!(pruning.pending_number, 3);
	}
}
