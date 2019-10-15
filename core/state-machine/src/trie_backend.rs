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

//! Trie-based state machine backend.

use log::{warn, debug};
use hash_db::Hasher;
use trie::{Trie, delta_trie_root, default_child_trie_root, child_delta_trie_root};
use trie::trie_types::{TrieDB, TrieError, Layout};
use trie::KeySpacedDB;
use crate::trie_backend_essence::{TrieBackendEssence, TrieBackendStorage, Ephemeral};
use crate::Backend;
use crate::kv_backend::KvBackend;
use primitives::storage::well_known_keys::CHILD_STORAGE_KEY_PREFIX;
use std::collections::HashMap;
use primitives::child_trie::{KeySpace, prefixed_keyspace_kv, NO_CHILD_KEYSPACE};

/// Patricia trie-based backend. Transaction type is an overlay of changes to commit.
/// A simple key value backend is also accessible for direct key value storage.
pub struct TrieBackend<S: TrieBackendStorage<H>, H: Hasher, O: KvBackend> {
	essence: TrieBackendEssence<S, H>,
	kv_storage: O,
}

impl<S: TrieBackendStorage<H>, O: KvBackend, H: Hasher> TrieBackend<S, H, O> {
	/// Create new trie-based backend.
	pub fn new(storage: S, root: H::Out, kv_storage: O) -> Self {
		TrieBackend {
			essence: TrieBackendEssence::new(storage, root),
			kv_storage,
		}
	}

	/// Get backend essence reference.
	pub fn essence(&self) -> &TrieBackendEssence<S, H> {
		&self.essence
	}

	/// Get backend storage reference.
	pub fn backend_storage(&self) -> &S {
		self.essence.backend_storage()
	}

	/// Get key value storage backend reference.
	pub fn kv_backend(&self) -> &O {
		&self.kv_storage
	}

	/// Get key value storage backend mutable reference.
	pub fn kv_backend_mut(&mut self) -> &mut O {
		&mut self.kv_storage
	}

	/// Get trie root.
	pub fn root(&self) -> &H::Out {
		self.essence.root()
	}

	/// Consumes self and returns underlying storage.
	pub fn into_storage(self) -> S {
		self.essence.into_storage()
	}

}

impl<
	S: TrieBackendStorage<H>,
	H: Hasher,
	O: KvBackend,
> std::fmt::Debug for TrieBackend<S, H, O> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "TrieBackend")
	}
}

impl<
	S: TrieBackendStorage<H>,
	H: Hasher,
	O: KvBackend,
> Backend<H> for TrieBackend<S, H, O> where
	H::Out: Ord,
{
	type Error = String;
	type Transaction = (S::Overlay, HashMap<Vec<u8>, Option<Vec<u8>>>, Vec<KeySpace>);
	type TrieBackendStorage = S;
	type KvBackend = O;

	fn storage(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		self.essence.storage(key)
	}

	fn child_storage(&self, storage_key: &[u8], key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		let keyspace = if let Some(keyspace) = self.get_child_keyspace(storage_key)? {
			keyspace
		} else {
			// cannot early exit for case where we do not prefix db
			// (proof): can be any dummy value here.
			NO_CHILD_KEYSPACE.to_vec()
		};

		self.essence.child_storage(storage_key, &keyspace, key)
	}

	fn kv_storage(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		self.kv_storage.get(key)
	}

	fn for_keys_with_prefix<F: FnMut(&[u8])>(&self, prefix: &[u8], f: F) {
		self.essence.for_keys_with_prefix(prefix, f)
	}

	fn for_key_values_with_prefix<F: FnMut(&[u8], &[u8])>(&self, prefix: &[u8], f: F) {
		self.essence.for_key_values_with_prefix(prefix, f)
	}

	fn for_keys_in_child_storage<F: FnMut(&[u8])>(&self, storage_key: &[u8], f: F) {
		let keyspace = match self.get_child_keyspace(storage_key) {
			Ok(Some(keyspace)) => keyspace,
			Err(e) => {
				warn!(target: "trie", "Failed to query keyspace: {}", e);
				return;
			},
			Ok(None) => return,
		};

		self.essence.for_keys_in_child_storage(storage_key, &keyspace, f)
	}

	fn for_child_keys_with_prefix<F: FnMut(&[u8])>(&self, storage_key: &[u8], prefix: &[u8], f: F) {
		self.essence.for_child_keys_with_prefix(storage_key, prefix, f)
	}

	fn pairs(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
		let mut read_overlay = S::Overlay::default();
		let eph = Ephemeral::new(self.essence.backend_storage(), &mut read_overlay);

		let collect_all = || -> Result<_, Box<TrieError<H::Out>>> {
			let eph = KeySpacedDB::new(&eph, None);
			let trie = TrieDB::<H>::new(&eph, self.essence.root())?;
			let mut v = Vec::new();
			for x in trie.iter()? {
				let (key, value) = x?;
				v.push((key.to_vec(), value.to_vec()));
			}

			Ok(v)
		};

		match collect_all() {
			Ok(v) => v,
			Err(e) => {
				debug!(target: "trie", "Error extracting trie values: {}", e);
				Vec::new()
			}
		}
	}

	fn children_storage_keys(&self) -> Vec<Vec<u8>> {
		let mut result = Vec::new();
		self.for_keys_with_prefix(CHILD_STORAGE_KEY_PREFIX, |k| result.push(k.to_vec()));
		result
	}

	fn child_pairs(&self, storage_key: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {

		let root_slice = self.essence.storage(storage_key)
			.unwrap_or(None)
			.unwrap_or(default_child_trie_root::<Layout<H>>(storage_key));
		let mut root = H::Out::default();
		root.as_mut().copy_from_slice(&root_slice[..]);

		let mut read_overlay = S::Overlay::default();
		let eph = Ephemeral::new(self.essence.backend_storage(), &mut read_overlay);

		let keyspace = match self.get_child_keyspace(storage_key) {
			Ok(ks) => ks,
			Err(e) => {
				debug!(target: "trie", "Error extracting child trie values: {}", e);
				return Vec::new();
			}
		};

		let collect_all = || -> Result<_, Box<TrieError<H::Out>>> {
			let mut v = Vec::new();
			// if no keyspace, the child trie just got created.
			if let Some(keyspace) = keyspace {
				let eph = KeySpacedDB::new(&eph, Some(&keyspace));
				let trie = TrieDB::<H>::new(&eph, &root)?;
				for x in trie.iter()? {
					let (key, value) = x?;
					v.push((key.to_vec(), value.to_vec()));
				}
			}

			Ok(v)
		};

		match collect_all() {
			Ok(v) => v,
			Err(e) => {
				debug!(target: "trie", "Error extracting child trie values: {}", e);
				Vec::new()
			}
		}
	}

	fn kv_pairs(&self) -> HashMap<Vec<u8>, Option<Vec<u8>>> {
		self.kv_storage.pairs()
	}

	fn keys(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
		let mut read_overlay = S::Overlay::default();
		let eph = Ephemeral::new(self.essence.backend_storage(), &mut read_overlay);

		let collect_all = || -> Result<_, Box<TrieError<H::Out>>> {
			let eph = KeySpacedDB::new(&eph, None);
			let trie = TrieDB::<H>::new(&eph, self.essence.root())?;
			let mut v = Vec::new();
			for x in trie.iter()? {
				let (key, _) = x?;
				if key.starts_with(prefix) {
					v.push(key.to_vec());
				}
			}

			Ok(v)
		};

		collect_all().map_err(|e| debug!(target: "trie", "Error extracting trie keys: {}", e)).unwrap_or_default()
	}

	fn storage_root<I>(&self, delta: I) -> (H::Out, Self::Transaction)
		where I: IntoIterator<Item=(Vec<u8>, Option<Vec<u8>>)>
	{
		let mut write_overlay = S::Overlay::default();
		let mut root = *self.essence.root();

		{
			let mut eph = Ephemeral::new(
				self.essence.backend_storage(),
				&mut write_overlay,
			);

			match delta_trie_root::<Layout<H>, _, _, _, _>(&mut eph, root, delta) {
				Ok(ret) => root = ret,
				Err(e) => warn!(target: "trie", "Failed to write to trie: {}", e),
			}
		}

		(root, (write_overlay, Default::default(), Default::default()))
	}

	fn child_storage_root<I>(
		&self,
		storage_key: &[u8],
		keyspace: &KeySpace,
		delta: (I, bool, Option<Vec<u8>>)
	) -> (Vec<u8>, bool, Self::Transaction)
		where
			I: IntoIterator<Item=(Vec<u8>, Option<Vec<u8>>)>,
			H::Out: Ord
	{
		// move only between same type so storage_key use does not matter here.
		let storage_key = delta.2.as_ref().map(Vec::as_ref).unwrap_or(storage_key);
		let default_root = default_child_trie_root::<Layout<H>>(storage_key);

		let mut write_overlay = S::Overlay::default();
		let mut root = if delta.1 {
			default_root.clone()
		} else {
			match self.storage(storage_key) {
				Ok(value) => value.unwrap_or(default_root.clone()),
				Err(e) => {
					warn!(target: "trie", "Failed to read child storage root: {}", e);
					default_root.clone()
				},
			}
		};

		{
			let mut eph = Ephemeral::new(
				self.essence.backend_storage(),
				&mut write_overlay,
			);

			match child_delta_trie_root::<Layout<H>, _, _, _, _>(
				storage_key,
				&mut eph,
				keyspace,
				root.clone(),
				delta.0
			) {
				Ok(ret) => root = ret,
				Err(e) => warn!(target: "trie", "Failed to write to trie: {}", e),
			}
		}

		let is_default = root == default_root;

		(root, is_default, (write_overlay, Default::default(), Default::default()))
	}

	fn kv_transaction<I>(&self, delta: I) -> Self::Transaction
		where
			I: IntoIterator<Item=(Vec<u8>, Option<Vec<u8>>)>
	{
		let mut result = self.kv_storage.pairs();
		result.extend(delta.into_iter());
		(Default::default(), result, Default::default())
	}

	fn as_trie_backend(&mut self) -> Option<
		&TrieBackend<Self::TrieBackendStorage, H, Self::KvBackend>
	> {
		Some(self)
	}
}

#[cfg(test)]
pub mod tests {
	use std::collections::HashSet;
	use primitives::{Blake2Hasher, H256};
	use primitives::child_trie::{produce_keyspace, KEYSPACE_COUNTER, reverse_keyspace};
	use trie::{TrieMut, PrefixedMemoryDB, KeySpacedDBMut, trie_types::TrieDBMut};
	use super::*;

	type KvBackend = crate::kv_backend::InMemory;

	const CHILD_KEY_1: &[u8; 27] = b":child_storage:default:sub1";

	fn test_db() -> (PrefixedMemoryDB<Blake2Hasher>, H256, KvBackend) {
		let mut root = H256::default();
		let mut mdb = PrefixedMemoryDB::<Blake2Hasher>::default();

		let keyspace1 = produce_keyspace(1);
		let mut sub_root = H256::default();
		{
			let mut mdb = KeySpacedDBMut::new(&mut mdb, Some(&keyspace1));
			let mut trie = TrieDBMut::new(&mut mdb, &mut sub_root);
			trie.insert(b"value3", &[142]).expect("insert failed");
			trie.insert(b"value4", &[124]).expect("insert failed");
		}
	
		{
			let mut mdb = KeySpacedDBMut::new(&mut mdb, None);
			let mut trie = TrieDBMut::new(&mut mdb, &mut root);
			trie.insert(CHILD_KEY_1, &sub_root[..]).expect("insert failed");
			trie.insert(b"key", b"value").expect("insert failed");
			trie.insert(b"value1", &[42]).expect("insert failed");
			trie.insert(b"value2", &[24]).expect("insert failed");
			trie.insert(b":code", b"return 42").expect("insert failed");
			for i in 128u8..255u8 {
				trie.insert(&[i], &[i]).unwrap();
			}
		}
		// empty history.
		let mut kv = crate::kv_backend::InMemory::default();
		kv.insert(KEYSPACE_COUNTER.to_vec(), Some(keyspace1.clone()));
		kv.insert(prefixed_keyspace_kv(&CHILD_KEY_1[..]), Some(keyspace1));
		(mdb, root, kv)
	}

	pub(crate) fn test_trie(
	) -> TrieBackend<PrefixedMemoryDB<Blake2Hasher>, Blake2Hasher, KvBackend> {
		let (mdb, root, kv) = test_db();
		TrieBackend::new(mdb, root, kv)
	}

	#[test]
	fn read_from_storage_returns_some() {
		assert_eq!(test_trie().storage(b"key").unwrap(), Some(b"value".to_vec()));
	}

	#[test]
	fn read_from_child_storage_returns_some() {
		let test_trie = test_trie();
		assert_eq!(
			test_trie.child_storage(CHILD_KEY_1, b"value3").unwrap(),
			Some(vec![142u8]),
		);
	}

	#[test]
	fn read_from_storage_returns_none() {
		assert_eq!(test_trie().storage(b"non-existing-key").unwrap(), None);
	}

	#[test]
	fn pairs_are_not_empty_on_non_empty_storage() {
		assert!(!test_trie().pairs().is_empty());
	}

	#[test]
	fn pairs_are_empty_on_empty_storage() {
		assert!(TrieBackend::<PrefixedMemoryDB<Blake2Hasher>, Blake2Hasher, KvBackend>::new(
			Default::default(),
			Default::default(),
			Default::default(),
		).pairs().is_empty());
	}

	#[test]
	fn storage_root_is_non_default() {
		assert!(test_trie().storage_root(::std::iter::empty()).0 != H256::repeat_byte(0));
	}

	#[test]
	fn storage_root_transaction_is_empty() {
		let mut tx = test_trie().storage_root(::std::iter::empty()).1;
		assert!(tx.0.drain().is_empty());
		assert!(tx.1.is_empty());
	}

	#[test]
	fn storage_root_transaction_is_non_empty() {
		let (new_root, mut tx) = test_trie().storage_root(vec![(b"new-key".to_vec(), Some(b"new-value".to_vec()))]);
		assert!(!tx.0.drain().is_empty());
		assert!(tx.1.is_empty());
		assert!(new_root != test_trie().storage_root(::std::iter::empty()).0);
	}

	#[test]
	fn prefix_walking_works() {
		let trie = test_trie();

		let mut seen = HashSet::new();
		trie.for_keys_with_prefix(b"value", |key| {
			let for_first_time = seen.insert(key.to_vec());
			assert!(for_first_time, "Seen key '{:?}' more than once", key);
		});

		let mut expected = HashSet::new();
		expected.insert(b"value1".to_vec());
		expected.insert(b"value2".to_vec());
		assert_eq!(seen, expected);
	}
}
