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

//! State machine backends. These manage the code and storage of contracts.

use std::{error, fmt, cmp::Ord, collections::HashMap, marker::PhantomData};
use log::warn;
use hash_db::Hasher;
use crate::trie_backend::TrieBackend;
use crate::trie_backend_essence::TrieBackendStorage;
use crate::kv_backend::{KvBackend, InMemory as InMemoryKvBackend};
use trie::{
	TrieMut, MemoryDB, child_trie_root, default_child_trie_root, TrieConfiguration,
	trie_types::{TrieDBMut, Layout},
};
use primitives::child_trie::{
	KeySpace, NO_CHILD_KEYSPACE, prefixed_keyspace_kv, KEYSPACE_COUNTER,
	reverse_keyspace, produce_keyspace,
};

/// A state backend is used to read state data and can have changes committed
/// to it.
///
/// The clone operation (if implemented) should be cheap.
pub trait Backend<H: Hasher>: std::fmt::Debug {
	/// An error type when fetching data is not possible.
	type Error: super::Error;

	/// Storage changes to be applied if committing
	type Transaction: Consolidate + Default;

	/// Type of trie backend storage.
	type TrieBackendStorage: TrieBackendStorage<H>;

	/// Type of trie backend storage.
	type KvBackend: KvBackend;

	/// Get keyed storage or None if there is nothing associated.
	fn storage(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error>;

	/// Access a value in the key value storage.
	fn kv_storage(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error>;

	/// Get keyed storage value hash or None if there is nothing associated.
	fn storage_hash(&self, key: &[u8]) -> Result<Option<H::Out>, Self::Error> {
		self.storage(key).map(|v| v.map(|v| H::hash(&v)))
	}

	/// Get keyed child storage or None if there is nothing associated.
	fn child_storage(&self, storage_key: &[u8], key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error>;

	/// Get child keyed storage value hash or None if there is nothing associated.
	fn child_storage_hash(&self, storage_key: &[u8], key: &[u8]) -> Result<Option<H::Out>, Self::Error> {
		self.child_storage(storage_key, key).map(|v| v.map(|v| H::hash(&v)))
	}

	/// Get technical keyspace use for child storage key.
	fn get_child_keyspace(&self, storage_key: &[u8]) -> Result<Option<KeySpace>, Self::Error> {
		self.kv_storage(&prefixed_keyspace_kv(&storage_key))
	}

	/// true if a key exists in storage.
	fn exists_storage(&self, key: &[u8]) -> Result<bool, Self::Error> {
		Ok(self.storage(key)?.is_some())
	}

	/// true if a key exists in child storage.
	fn exists_child_storage(&self, storage_key: &[u8], key: &[u8]) -> Result<bool, Self::Error> {
		Ok(self.child_storage(storage_key, key)?.is_some())
	}

	/// Retrieve all entries keys of child storage and call `f` for each of those keys.
	fn for_keys_in_child_storage<F: FnMut(&[u8])>(&self, storage_key: &[u8], f: F);

	/// Retrieve all entries keys which start with the given prefix and
	/// call `f` for each of those keys.
	fn for_keys_with_prefix<F: FnMut(&[u8])>(&self, prefix: &[u8], mut f: F) {
		self.for_key_values_with_prefix(prefix, |k, _v| f(k))
	}

	/// Retrieve all entries keys and values of which start with the given prefix and
	/// call `f` for each of those keys.
	fn for_key_values_with_prefix<F: FnMut(&[u8], &[u8])>(&self, prefix: &[u8], f: F);


	/// Retrieve all child entries keys which start with the given prefix and
	/// call `f` for each of those keys.
	fn for_child_keys_with_prefix<F: FnMut(&[u8])>(&self, storage_key: &[u8], prefix: &[u8], f: F);

	/// Calculate the storage root, with given delta over what is already stored in
	/// the backend, and produce a "transaction" that can be used to commit.
	/// Does not include child storage updates.
	fn storage_root<I>(&self, delta: I) -> (H::Out, Self::Transaction)
	where
		I: IntoIterator<Item=(Vec<u8>, Option<Vec<u8>>)>,
		H::Out: Ord;

	/// Calculate the child storage root, with given delta over what is already stored in
	/// the backend, and produce a "transaction" that can be used to commit. The second argument
	/// is true if child storage root equals default storage root.
	fn child_storage_root<I>(
		&self,
		storage_key: &[u8],
		keyspace: &KeySpace,
		delta: I,
	) -> (Vec<u8>, bool, Self::Transaction)
	where
		I: IntoIterator<Item=(Vec<u8>, Option<Vec<u8>>)>,
		H::Out: Ord;

	/// Produce transaction for a given kv information deltas.
	fn kv_transaction<I>(&self, delta: I) -> Self::Transaction
	where
		I: IntoIterator<Item=(Vec<u8>, Option<Vec<u8>>)>;

	/// Get all key/value pairs into a Vec.
	fn pairs(&self) -> Vec<(Vec<u8>, Vec<u8>)>;

	/// Get all children storage keys
	fn children_storage_keys(&self) -> Vec<Vec<u8>>;

	/// Get all key/value pairs into a Vec for a child storage.
	fn child_pairs(&self, child_storage_key: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)>;

	/// Get all key/value pairs of kv storage. 
	fn kv_pairs(&self) -> HashMap<Vec<u8>, Option<Vec<u8>>>;

	/// Get all keys with given prefix
	fn keys(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
		let mut all = Vec::new();
		self.for_keys_with_prefix(prefix, |k| all.push(k.to_vec()));
		all
	}

	/// Get all keys of child storage with given prefix
	fn child_keys(&self, child_storage_key: &[u8], prefix: &[u8]) -> Vec<Vec<u8>> {
		let mut all = Vec::new();
		self.for_child_keys_with_prefix(child_storage_key, prefix, |k| all.push(k.to_vec()));
		all
	}

	/// Try convert into trie backend.
	fn as_trie_backend(&mut self) -> Option<
		&TrieBackend<Self::TrieBackendStorage, H, Self::KvBackend>
	> {
		None
	}

	/// Calculate the storage root, with given delta over what is already stored
	/// in the backend, and produce a "transaction" that can be used to commit.
	/// Does include child storage updates.
	fn full_storage_root<I1, I2i, I2, I3>(
		&self,
		delta: I1,
		child_deltas: I2,
		kv_deltas: I3,
	) -> Result<(H::Out, Self::Transaction), Self::Error>
	where
		I1: IntoIterator<Item=(Vec<u8>, Option<Vec<u8>>)>,
		I2i: IntoIterator<Item=(Vec<u8>, Option<Vec<u8>>)>,
		I2: IntoIterator<Item=(Vec<u8>, I2i)>,
		I3: IntoIterator<Item=(Vec<u8>, Option<Vec<u8>>)>,
		<H as Hasher>::Out: Ord,
	{
		let mut txs: Self::Transaction = Default::default();

		let mut child_roots: Vec<_> = Default::default();

		let mut counter_keyspace = None;
		let mut new_keyspaces = Vec::new();
		// child first
		for (storage_key, child_delta) in child_deltas {
			let keyspace = match self.get_child_keyspace(storage_key.as_slice())? {
				Some(keyspace) => keyspace,
				None => {
					if counter_keyspace.is_none() {
						let counter_keyspace_enc = self.kv_storage(KEYSPACE_COUNTER)?
							.unwrap_or(produce_keyspace(0));
						let keyspace = reverse_keyspace(counter_keyspace_enc.as_slice())
							.expect("Keyspaces are never added manually so encoding is valid");
						counter_keyspace = Some(keyspace);
					}
					// increment counter
					counter_keyspace.as_mut().map(|c| {
						*c += 1;
					});
					let enc_counter_keyspace = produce_keyspace(
						*counter_keyspace.as_ref().expect("lazy init at start of this block")
					);
					new_keyspaces.push((
						prefixed_keyspace_kv(storage_key.as_slice()),
						Some(enc_counter_keyspace.clone()),
					));
					enc_counter_keyspace
				},
			};

			counter_keyspace.map(|c| {
				new_keyspaces.push((
					KEYSPACE_COUNTER.to_vec(),
					Some(produce_keyspace(c)),
				));
			});

			let (child_root, empty, child_txs) =
				self.child_storage_root(&storage_key[..], &keyspace, child_delta);
			txs.consolidate(child_txs);
			if empty {
				child_roots.push((storage_key, None));
			} else {
				child_roots.push((storage_key, Some(child_root)));
			}
		}
		let (root, parent_txs) = self.storage_root(
			delta.into_iter().chain(child_roots.into_iter())
		);
		txs.consolidate(parent_txs);
		txs.consolidate(self.kv_transaction(
			kv_deltas.into_iter().chain(new_keyspaces.into_iter())
		));
		Ok((root, txs))
	}

}

impl<'a, T: Backend<H>, H: Hasher> Backend<H> for &'a T {
	type Error = T::Error;
	type Transaction = T::Transaction;
	type TrieBackendStorage = T::TrieBackendStorage;
	type KvBackend = T::KvBackend;

	fn storage(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		(*self).storage(key)
	}

	fn child_storage(&self, storage_key: &[u8], key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		(*self).child_storage(storage_key, key)
	}

	fn kv_storage(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		(*self).kv_storage(key)
	}

	fn for_keys_in_child_storage<F: FnMut(&[u8])>(&self, storage_key: &[u8], f: F) {
		(*self).for_keys_in_child_storage(storage_key, f)
	}

	fn for_keys_with_prefix<F: FnMut(&[u8])>(&self, prefix: &[u8], f: F) {
		(*self).for_keys_with_prefix(prefix, f)
	}

	fn for_child_keys_with_prefix<F: FnMut(&[u8])>(&self, storage_key: &[u8], prefix: &[u8], f: F) {
		(*self).for_child_keys_with_prefix(storage_key, prefix, f)
	}

	fn storage_root<I>(&self, delta: I) -> (H::Out, Self::Transaction)
	where
		I: IntoIterator<Item=(Vec<u8>, Option<Vec<u8>>)>,
		H::Out: Ord,
	{
		(*self).storage_root(delta)
	}

	fn child_storage_root<I>(
		&self,
		storage_key: &[u8],
		keyspace: &KeySpace,
		delta: I,
	) -> (Vec<u8>, bool, Self::Transaction)
	where
		I: IntoIterator<Item=(Vec<u8>, Option<Vec<u8>>)>,
		H::Out: Ord,
	{
		(*self).child_storage_root(storage_key, keyspace, delta)
	}

	fn kv_transaction<I>(&self, delta: I) -> Self::Transaction
	where
		I: IntoIterator<Item=(Vec<u8>, Option<Vec<u8>>)>
	{
		(*self).kv_transaction(delta)
	}

	fn pairs(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
		(*self).pairs()
	}

	fn children_storage_keys(&self) -> Vec<Vec<u8>> {
		(*self).children_storage_keys()
	}

	fn child_pairs(&self, child_storage_key: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
		(*self).child_pairs(child_storage_key)
	}

	fn kv_pairs(&self) -> HashMap<Vec<u8>, Option<Vec<u8>>> {
		(*self).kv_pairs()
	}


	fn for_key_values_with_prefix<F: FnMut(&[u8], &[u8])>(&self, prefix: &[u8], f: F) {
		(*self).for_key_values_with_prefix(prefix, f);
	}
}

/// Trait that allows consolidate two transactions together.
pub trait Consolidate {
	/// Consolidate two transactions into one.
	fn consolidate(&mut self, other: Self);
}

impl Consolidate for () {
	fn consolidate(&mut self, _: Self) {
		()
	}
}

impl<U> Consolidate for Vec<U> {
	fn consolidate(&mut self, mut other: Self) {
		self.append(&mut other);
	}
}

impl<K: Eq + std::hash::Hash, V> Consolidate for HashMap<K, V> {
	fn consolidate(&mut self, other: Self) {
		self.extend(other);
	}
}

impl<U: Consolidate, V: Consolidate> Consolidate for (U, V) {
	fn consolidate(&mut self, other: Self) {
		self.0.consolidate(other.0);
		self.1.consolidate(other.1);
	}
}


impl Consolidate for InMemoryTransaction {
	fn consolidate(&mut self, other: Self) {
		self.storage.consolidate(other.storage);
		self.kv.consolidate(other.kv);
	}
}

impl<H: Hasher, KF: trie::KeyFunction<H>> Consolidate for trie::GenericMemoryDB<H, KF> {
	fn consolidate(&mut self, other: Self) {
		trie::GenericMemoryDB::consolidate(self, other)
	}
}

/// Error impossible.
// FIXME: use `!` type when stabilized. https://github.com/rust-lang/rust/issues/35121
#[derive(Debug)]
pub enum Void {}

impl fmt::Display for Void {
	fn fmt(&self, _: &mut fmt::Formatter) -> fmt::Result {
		match *self {}
	}
}

impl error::Error for Void {
	fn description(&self) -> &str { "unreachable error" }
}

/// In-memory backend. Fully recomputes tries on each commit but useful for
/// tests.
pub struct InMemory<H: Hasher> {
	inner: HashMap<Option<Vec<u8>>, HashMap<Vec<u8>, Vec<u8>>>,
	kv: Option<InMemoryKvBackend>,
	trie: Option<TrieBackend<MemoryDB<H>, H, InMemoryKvBackend>>,
	_hasher: PhantomData<H>,
}

impl<H: Hasher> std::fmt::Debug for InMemory<H> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "InMemory ({} values)", self.inner.len())
	}
}

impl<H: Hasher> Default for InMemory<H> {
	fn default() -> Self {
		InMemory {
			inner: Default::default(),
			trie: None,
			kv: Some(Default::default()),
			_hasher: PhantomData,
		}
	}
}

impl<H: Hasher> Clone for InMemory<H> {
	fn clone(&self) -> Self {
		InMemory {
			inner: self.inner.clone(),
			trie: None,
			kv: self.kv.clone(),
			_hasher: PhantomData,
		}
	}
}

impl<H: Hasher> PartialEq for InMemory<H> {
	fn eq(&self, other: &Self) -> bool {
		self.inner.eq(&other.inner)
		&& self.kv().eq(other.kv()) 
	}
}

impl<H: Hasher> InMemory<H> {
	fn kv(&self) -> &InMemoryKvBackend {
		if let Some(kv) = self.kv.as_ref() {
			kv
		} else {
			self.trie.as_ref().unwrap().kv_backend()
		}
	}

	fn kv_mut(&mut self) -> &mut InMemoryKvBackend {
		if let Some(kv) = self.kv.as_mut() {
			kv
		} else {
			self.trie.as_mut().unwrap().kv_backend_mut()
		}
	}

	fn extract_kv(&mut self) -> InMemoryKvBackend {
		if let Some(kv) = self.kv.take() {
			kv
		} else {
			std::mem::replace(
				self.trie.as_mut().unwrap().kv_backend_mut(),
				Default::default(),
			)	
		}
	}

	/// Copy the state, with applied updates
	pub fn update(&self, changes: <Self as Backend<H>>::Transaction) -> Self {
		let mut inner: HashMap<_, _> = self.inner.clone();
		let mut kv: HashMap<_, _> = self.kv().clone();
		for (storage_key, key, val) in changes.storage {
			match val {
				Some(v) => { inner.entry(storage_key).or_default().insert(key, v); },
				None => { inner.entry(storage_key).or_default().remove(&key); },
			}
		}
		kv.extend(changes.kv);

		let kv = Some(kv);
		InMemory { inner, kv, trie: None, _hasher: PhantomData }
	}
}

type TupleInit = (
	HashMap<Option<Vec<u8>>, HashMap<Vec<u8>, Vec<u8>>>,
	HashMap<Vec<u8>, Option<KeySpace>>,
);

impl<H: Hasher> From<TupleInit> for InMemory<H> {
	fn from(inner: TupleInit) -> Self {
		InMemory {
			inner: inner.0,
			trie: None,
			kv: Some(inner.1),
			_hasher: PhantomData,
		}
	}
}

type TupleInit2 = (
	HashMap<Vec<u8>, Vec<u8>>,
	HashMap<Vec<u8>, HashMap<Vec<u8>, Vec<u8>>>,
	HashMap<Vec<u8>, Option<KeySpace>>,
);

impl<H: Hasher> From<TupleInit2> for InMemory<H> {
	fn from(tuple: TupleInit2) -> Self {
		let mut inner: HashMap<_, _> = tuple.1.into_iter().map(|(k, v)| (Some(k), v)).collect();
		inner.insert(None, tuple.0);
		InMemory {
			inner: inner,
			trie: None,
			kv: Some(tuple.2),
			_hasher: PhantomData,
		}
	}
}

impl<H: Hasher> From<HashMap<Vec<u8>, Vec<u8>>> for InMemory<H> {
	fn from(inner: HashMap<Vec<u8>, Vec<u8>>) -> Self {
		let mut expanded = HashMap::new();
		expanded.insert(None, inner);
		InMemory {
			inner: expanded,
			trie: None,
			kv: Some(Default::default()),
			_hasher: PhantomData,
		}
	}
}

type TupleTx = (
	Vec<(Option<Vec<u8>>, Vec<u8>, Option<Vec<u8>>)>,
	HashMap<Vec<u8>, Option<KeySpace>>,
);


impl<H: Hasher> From<TupleTx> for InMemory<H> {
	fn from(inner: TupleTx) -> Self {
		let mut expanded: HashMap<Option<Vec<u8>>, HashMap<Vec<u8>, Vec<u8>>> = HashMap::new();
		for (child_key, key, value) in inner.0 {
			if let Some(value) = value {
				expanded.entry(child_key).or_default().insert(key, value);
			}
		}
		(expanded, inner.1).into()
	}
}

impl<H: Hasher> From<InMemoryTransaction> for InMemory<H> {
	fn from(inner: InMemoryTransaction) -> Self {
		let mut expanded: HashMap<Option<Vec<u8>>, HashMap<Vec<u8>, Vec<u8>>> = HashMap::new();
		for (child_key, key, value) in inner.storage {
			if let Some(value) = value {
				expanded.entry(child_key).or_default().insert(key, value);
			}
		}
		(expanded, inner.kv).into()
	}
}

impl<H: Hasher> InMemory<H> {
	/// child storage key iterator
	pub fn child_storage_keys(&self) -> impl Iterator<Item=&[u8]> {
		self.inner.iter().filter_map(|item| item.0.as_ref().map(|v|&v[..]))
	}
}

#[derive(Default)]
/// Transaction produced by the state machine execution for
/// in memory storage.
pub struct InMemoryTransaction {
	/// State trie key values changes (both top and child trie).
	pub storage: Vec<(Option<Vec<u8>>, Vec<u8>, Option<Vec<u8>>)>,
	/// Changes to non trie key value datas.
	pub kv: HashMap<Vec<u8>, Option<Vec<u8>>>,
}

impl<H: Hasher> Backend<H> for InMemory<H> {
	type Error = Void;
	type Transaction = InMemoryTransaction;
	type TrieBackendStorage = MemoryDB<H>;
	type KvBackend = InMemoryKvBackend;

	fn storage(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		Ok(self.inner.get(&None).and_then(|map| map.get(key).map(Clone::clone)))
	}

	fn child_storage(&self, storage_key: &[u8], key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		Ok(self.inner.get(&Some(storage_key.to_vec())).and_then(|map| map.get(key).map(Clone::clone)))
	}

	fn kv_storage(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		Ok(
			self.kv().get(key)
				.map(Clone::clone)
				.unwrap_or(None)
		)
	}

	fn exists_storage(&self, key: &[u8]) -> Result<bool, Self::Error> {
		Ok(self.inner.get(&None).map(|map| map.get(key).is_some()).unwrap_or(false))
	}

	fn for_keys_with_prefix<F: FnMut(&[u8])>(&self, prefix: &[u8], f: F) {
		self.inner.get(&None).map(|map| map.keys().filter(|key| key.starts_with(prefix)).map(|k| &**k).for_each(f));
	}

	fn for_key_values_with_prefix<F: FnMut(&[u8], &[u8])>(&self, prefix: &[u8], mut f: F) {
		self.inner.get(&None).map(|map| map.iter().filter(|(key, _val)| key.starts_with(prefix))
			.for_each(|(k, v)| f(k, v)));
	}

	fn for_keys_in_child_storage<F: FnMut(&[u8])>(&self, storage_key: &[u8], mut f: F) {
		self.inner.get(&Some(storage_key.to_vec())).map(|map| map.keys().for_each(|k| f(&k)));
	}

	fn for_child_keys_with_prefix<F: FnMut(&[u8])>(&self, storage_key: &[u8], prefix: &[u8], f: F) {
		self.inner.get(&Some(storage_key.to_vec()))
			.map(|map| map.keys().filter(|key| key.starts_with(prefix)).map(|k| &**k).for_each(f));
	}

	fn storage_root<I>(&self, delta: I) -> (H::Out, Self::Transaction)
	where
		I: IntoIterator<Item=(Vec<u8>, Option<Vec<u8>>)>,
		<H as Hasher>::Out: Ord,
	{
		let existing_pairs = self.inner.get(&None)
			.into_iter()
			.flat_map(|map| map.iter().map(|(k, v)| (k.clone(), Some(v.clone()))));

		let transaction: Vec<_> = delta.into_iter().collect();
		let root = Layout::<H>::trie_root(existing_pairs.chain(transaction.iter().cloned())
			.collect::<HashMap<_, _>>()
			.into_iter()
			.filter_map(|(k, maybe_val)| maybe_val.map(|val| (k, val)))
		);

		let full_transaction = transaction.into_iter().map(|(k, v)| (None, k, v)).collect();

		(root, InMemoryTransaction { storage: full_transaction, kv: Default::default() })
	}

	fn child_storage_root<I>(
		&self,
		storage_key: &[u8],
		_keyspace: &KeySpace,
		delta: I,
	) -> (Vec<u8>, bool, Self::Transaction)
	where
		I: IntoIterator<Item=(Vec<u8>, Option<Vec<u8>>)>,
		H::Out: Ord
	{
		let storage_key = Some(storage_key.to_vec());

		let existing_pairs = self.inner.get(&storage_key)
			.into_iter()
			.flat_map(|map| map.iter().map(|(k, v)| (k.clone(), Some(v.clone()))));

		let transaction: Vec<_> = delta.into_iter().collect();
		let root = child_trie_root::<Layout<H>, _, _, _>(
			storage_key.as_ref().expect("Initialized to some"),
			existing_pairs.chain(transaction.iter().cloned())
				.collect::<HashMap<_, _>>()
				.into_iter()
				.filter_map(|(k, maybe_val)| maybe_val.map(|val| (k, val)))
		);

		let full_transaction = transaction.into_iter()
			.map(|(k, v)| (storage_key.clone(), k, v)).collect();

		let is_default = root == default_child_trie_root::<Layout<H>>(
			storage_key.as_ref().expect("Initialized to some")
		);

		(
			root,
			is_default,
			InMemoryTransaction { storage: full_transaction, kv: Default::default() },
		)
	}

	fn kv_transaction<I>(&self, delta: I) -> Self::Transaction
	where
		I: IntoIterator<Item=(Vec<u8>, Option<Vec<u8>>)>
	{
		let mut kv = self.kv().clone();
		kv.extend(delta.into_iter());
		InMemoryTransaction { storage: Default::default(), kv}
	}

	fn pairs(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
		self.inner.get(&None)
			.into_iter()
			.flat_map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())))
			.collect()
	}

	fn children_storage_keys(&self) -> Vec<Vec<u8>> {
		self.inner.iter().filter_map(|(child, _)| child.clone()).collect()
	}

	fn child_pairs(&self, storage_key: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
		self.inner.get(&Some(storage_key.to_vec()))
			.into_iter()
			.flat_map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())))
			.collect()
	}

	fn kv_pairs(&self) -> HashMap<Vec<u8>, Option<Vec<u8>>> {
		self.kv().clone()
	}

	fn keys(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
		self.inner.get(&None)
			.into_iter()
			.flat_map(|map| map.keys().filter(|k| k.starts_with(prefix)).cloned())
			.collect()
	}

	fn child_keys(&self, storage_key: &[u8], prefix: &[u8]) -> Vec<Vec<u8>> {
		self.inner.get(&Some(storage_key.to_vec()))
			.into_iter()
			.flat_map(|map| map.keys().filter(|k| k.starts_with(prefix)).cloned())
			.collect()
	}

	fn as_trie_backend(&mut self)-> Option<
		&TrieBackend<Self::TrieBackendStorage, H, Self::KvBackend>
	> {
		let mut mdb = MemoryDB::default();
		let mut root = None;
		let mut new_child_roots = Vec::new();
		let mut root_map = None;
		for (storage_key, map) in &self.inner {
			if let Some(storage_key) = storage_key.as_ref() {
				let ch = insert_into_memory_db::<H, _>(&mut mdb, map.clone().into_iter())?;
				new_child_roots.push((storage_key.clone(), ch.as_ref().into()));
			} else {
				root_map = Some(map);
			}
		}
		// root handling
		if let Some(map) = root_map.take() {
			root = Some(insert_into_memory_db::<H, _>(
				&mut mdb,
				map.clone().into_iter().chain(new_child_roots.into_iter())
			)?);
		}
		let root = match root {
			Some(root) => root,
			None => insert_into_memory_db::<H, _>(&mut mdb, ::std::iter::empty())?,
		};
		self.trie = Some(TrieBackend::new(mdb, root, self.extract_kv()));
		self.trie.as_ref()
	}
}

/// Insert input pairs into memory db.
pub(crate) fn insert_into_memory_db<H, I>(mdb: &mut MemoryDB<H>, input: I) -> Option<H::Out>
	where
		H: Hasher,
		I: IntoIterator<Item=(Vec<u8>, Vec<u8>)>,
{
	let mut root = <H as Hasher>::Out::default();
	{
		let mut trie = TrieDBMut::<H>::new(mdb, &mut root);
		for (key, value) in input {
			if let Err(e) = trie.insert(&key, &value) {
				warn!(target: "trie", "Failed to write to trie: {}", e);
				return None;
			}
		}
	}

	Some(root)
}
