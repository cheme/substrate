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

//! Trie-based state machine backend essence used to read values
//! from storage.

use std::ops::Deref;
use std::sync::Arc;
use log::{debug, warn};
use hash_db::{self, Hasher};
use trie::{
	TrieDB, Trie, MemoryDB, PrefixedMemoryDB, DBValue, TrieError,
	default_child_trie_root, read_trie_value, read_child_trie_value, for_keys_in_child_trie,
};
use crate::backend::Consolidate;
use crate::client::Externalities as ClientExternalities;
use crate::client::CHOut;

/// Patricia trie-based storage trait.
pub trait Storage<H: Hasher>: Send + Sync {
	/// Get a trie node.
	fn get(&self, key: &H::Out, prefix: &[u8]) -> Result<Option<DBValue>, String>;
}

/// Patricia trie-based pairs storage essence.
pub struct TrieBackendEssence<S: TrieBackendStorage<C>, C: ClientExternalities> {
	storage: S,
	root: CHOut<C>,
}

impl<S: TrieBackendStorage<C>, C: ClientExternalities> TrieBackendEssence<S, C> {
	/// Create new trie-based backend.
	pub fn new(storage: S, root: CHOut<C>) -> Self {
		TrieBackendEssence {
			storage,
			root,
		}
	}

	/// Get backend storage reference.
	pub fn backend_storage(&self) -> &S {
		&self.storage
	}

	/// Get trie root.
	pub fn root(&self) -> &CHOut<C> {
		&self.root
	}

	/// Reroot.
	pub fn reroot(&mut self, new_root: CHOut<C>) {
		self.root = new_root;
	}

	/// Consumes self and returns underlying storage.
	pub fn into_storage(self) -> S {
		self.storage
	}

	/// Get the value of storage at given key.
	pub fn storage(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
		let mut read_overlay = S::Overlay::default();
		let eph = Ephemeral {
			storage: &self.storage,
			overlay: &mut read_overlay,
		};

		let map_e = |e| format!("Trie lookup error: {}", e);

		read_trie_value(&eph, &self.root, key).map_err(map_e)
	}

	/// Get the value of child storage at given key.
	pub fn child_storage(&self, storage_key: &[u8], key: &[u8]) -> Result<Option<Vec<u8>>, String> {
		let root = self.storage(storage_key)?.unwrap_or(default_child_trie_root::<C::H>(storage_key));

		let mut read_overlay = S::Overlay::default();
		let eph = Ephemeral {
			storage: &self.storage,
			overlay: &mut read_overlay,
		};

		let map_e = |e| format!("Trie lookup error: {}", e);

		read_child_trie_value(storage_key, &eph, &root, key).map_err(map_e)
	}

	/// Retrieve all entries keys of child storage and call `f` for each of those keys.
	pub fn for_keys_in_child_storage<F: FnMut(&[u8])>(&self, storage_key: &[u8], f: F) {
		let root = match self.storage(storage_key) {
			Ok(v) => v.unwrap_or(default_child_trie_root::<C::H>(storage_key)),
			Err(e) => {
				debug!(target: "trie", "Error while iterating child storage: {}", e);
				return;
			}
		};

		let mut read_overlay = S::Overlay::default();
		let eph = Ephemeral {
			storage: &self.storage,
			overlay: &mut read_overlay,
		};

		if let Err(e) = for_keys_in_child_trie::<C::H, _, Ephemeral<S, C>>(storage_key, &eph, &root, f) {
			debug!(target: "trie", "Error while iterating child storage: {}", e);
		}
	}

	/// Execute given closure for all keys starting with prefix.
	pub fn for_keys_with_prefix<F: FnMut(&[u8])>(&self, prefix: &[u8], mut f: F) {
		let mut read_overlay = S::Overlay::default();
		let eph = Ephemeral {
			storage: &self.storage,
			overlay: &mut read_overlay,
		};

		let mut iter = move || -> Result<(), Box<TrieError<CHOut<C>>>> {
			let trie = TrieDB::<C::H>::new(&eph, &self.root)?;
			let mut iter = trie.iter()?;

			iter.seek(prefix)?;

			for x in iter {
				let (key, _) = x?;

				if !key.starts_with(prefix) {
					break;
				}

				f(&key);
			}

			Ok(())
		};

		if let Err(e) = iter() {
			debug!(target: "trie", "Error while iterating by prefix: {}", e);
		}
	}
}

pub(crate) struct Ephemeral<'a, S: 'a + TrieBackendStorage<C>, C: 'a + ClientExternalities> {
	storage: &'a S,
	overlay: &'a mut S::Overlay,
}

impl<'a,
	S: 'a + TrieBackendStorage<C>,
	C: 'a + ClientExternalities,
> hash_db::AsPlainDB<CHOut<C>, DBValue>
	for Ephemeral<'a, S, C>
{
	fn as_plain_db<'b>(&'b self) -> &'b (dyn hash_db::PlainDB<CHOut<C>, DBValue> + 'b) { self }
	fn as_plain_db_mut<'b>(&'b mut self) -> &'b mut (dyn hash_db::PlainDB<CHOut<C>, DBValue> + 'b) { self }
}

impl<'a,
	S: 'a + TrieBackendStorage<C>,
	C: 'a + ClientExternalities,
> hash_db::AsHashDB<C::H, DBValue>
	for Ephemeral<'a, S, C>
{
	fn as_hash_db<'b>(&'b self) -> &'b (dyn hash_db::HashDB<C::H, DBValue> + 'b) { self }
	fn as_hash_db_mut<'b>(&'b mut self) -> &'b mut (dyn hash_db::HashDB<C::H, DBValue> + 'b) { self }
}

impl<'a, S: TrieBackendStorage<C>, C: ClientExternalities> Ephemeral<'a, S, C> {
	pub fn new(storage: &'a S, overlay: &'a mut S::Overlay) -> Self {
		Ephemeral {
			storage,
			overlay,
		}
	}
}

impl<'a,
	S: 'a + TrieBackendStorage<C>,
	C: ClientExternalities,
> hash_db::PlainDB<CHOut<C>, DBValue>
	for Ephemeral<'a, S, C>
{
	fn get(&self, key: &CHOut<C>) -> Option<DBValue> {
		if let Some(val) = hash_db::HashDB::get(self.overlay, key, &[]) {
			Some(val)
		} else {
			match self.storage.get(&key, &[]) {
				Ok(x) => x,
				Err(e) => {
					warn!(target: "trie", "Failed to read from DB: {}", e);
					None
				},
			}
		}
	}

	fn contains(&self, key: &CHOut<C>) -> bool {
		hash_db::HashDB::get(self, key, &[]).is_some()
	}

	fn emplace(&mut self, key: CHOut<C>, value: DBValue) {
		hash_db::HashDB::emplace(self.overlay, key, &[], value)
	}

	fn remove(&mut self, key: &CHOut<C>) {
		hash_db::HashDB::remove(self.overlay, key, &[])
	}
}

impl<'a,
	S: 'a + TrieBackendStorage<C>,
	C: ClientExternalities,
> hash_db::PlainDBRef<CHOut<C>, DBValue>
	for Ephemeral<'a, S, C>
{
	fn get(&self, key: &CHOut<C>) -> Option<DBValue> { hash_db::PlainDB::get(self, key) }
	fn contains(&self, key: &CHOut<C>) -> bool { hash_db::PlainDB::contains(self, key) }
}

impl<'a,
	S: 'a + TrieBackendStorage<C>,
	C: ClientExternalities,
> hash_db::HashDB<C::H, DBValue>
	for Ephemeral<'a, S, C>
{
	fn get(&self, key: &CHOut<C>, prefix: &[u8]) -> Option<DBValue> {
		if let Some(val) = hash_db::HashDB::get(self.overlay, key, prefix) {
			Some(val)
		} else {
			match self.storage.get(&key, prefix) {
				Ok(x) => x,
				Err(e) => {
					warn!(target: "trie", "Failed to read from DB: {}", e);
					None
				},
			}
		}
	}

	fn contains(&self, key: &CHOut<C>, prefix: &[u8]) -> bool {
		hash_db::HashDB::get(self, key, prefix).is_some()
	}

	fn insert(&mut self, prefix: &[u8], value: &[u8]) -> CHOut<C> {
		hash_db::HashDB::insert(self.overlay, prefix, value)
	}

	fn emplace(&mut self, key: CHOut<C>, prefix: &[u8], value: DBValue) {
		hash_db::HashDB::emplace(self.overlay, key, prefix, value)
	}

	fn remove(&mut self, key: &CHOut<C>, prefix: &[u8]) {
		hash_db::HashDB::remove(self.overlay, key, prefix)
	}
}

impl<'a,
	S: 'a + TrieBackendStorage<C>,
	C: ClientExternalities,
> hash_db::HashDBRef<C::H, DBValue>
	for Ephemeral<'a, S, C>
{
	fn get(&self, key: &CHOut<C>, prefix: &[u8]) -> Option<DBValue> { hash_db::HashDB::get(self, key, prefix) }
	fn contains(&self, key: &CHOut<C>, prefix: &[u8]) -> bool { hash_db::HashDB::contains(self, key, prefix) }
}

/// Key-value pairs storage that is used by trie backend essence.
pub trait TrieBackendStorage<C: ClientExternalities>: Send + Sync {
	/// Type of in-memory overlay.
	type Overlay: hash_db::HashDB<C::H, DBValue> + Default + Consolidate;
	/// Get the value stored at key.
	fn get(&self, key: &CHOut<C>, prefix: &[u8]) -> Result<Option<DBValue>, String>;
}

// This implementation is used by normal storage trie clients.
impl<C: ClientExternalities> TrieBackendStorage<C> for Arc<dyn Storage<C::H>> {
	type Overlay = PrefixedMemoryDB<C::H>;

	fn get(&self, key: &CHOut<C>, prefix: &[u8]) -> Result<Option<DBValue>, String> {
		Storage::<C::H>::get(self.deref(), key, prefix)
	}
}

// This implementation is used by test storage trie clients.
impl<C: ClientExternalities> TrieBackendStorage<C> for PrefixedMemoryDB<C::H> {
	type Overlay = PrefixedMemoryDB<C::H>;

	fn get(&self, key: &CHOut<C>, prefix: &[u8]) -> Result<Option<DBValue>, String> {
		Ok(hash_db::HashDB::get(self, key, prefix))
	}
}

impl<C: ClientExternalities> TrieBackendStorage<C> for MemoryDB<C::H> {
	type Overlay = MemoryDB<C::H>;

	fn get(&self, key: &CHOut<C>, prefix: &[u8]) -> Result<Option<DBValue>, String> {
		Ok(hash_db::HashDB::get(self, key, prefix))
	}
}
