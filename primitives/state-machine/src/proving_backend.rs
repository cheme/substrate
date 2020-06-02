// This file is part of Substrate.

// Copyright (C) 2017-2020 Parity Technologies (UK) Ltd.
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

//! Proving state machine backend.

use std::{sync::Arc, collections::{HashMap, hash_map::Entry}};
use parking_lot::RwLock;
use codec::{Decode, Codec};
use log::debug;
use hash_db::{Hasher, Prefix};
use sp_trie::{
	MemoryDB, empty_child_trie_root, read_trie_value_with, read_child_trie_value_with,
	record_all_keys, TrieNodesStorageProof, TrieConfiguration, TrieHash,
};
pub use sp_trie::{Recorder, TrieError, trie_types::{Layout}};
use crate::trie_backend::TrieBackend;
use crate::trie_backend_essence::{Ephemeral, TrieBackendEssence, TrieBackendStorage};
use crate::backend::{Backend, ProofRegStateFor, ProofRegBackend};
use crate::DBValue;
use sp_core::storage::ChildInfo;

/// Patricia trie-based backend specialized in get value proofs.
pub struct ProvingBackendRecorder<'a, S: 'a + TrieBackendStorage<T::Hash>, T: 'a + TrieConfiguration> {
	pub(crate) backend: &'a TrieBackendEssence<S, T>,
	pub(crate) proof_recorder: &'a mut Recorder<TrieHash<T>>,
}

impl<'a, S, T> ProvingBackendRecorder<'a, S, T>
	where
		S: TrieBackendStorage<T::Hash>,
		T: TrieConfiguration,
		TrieHash<T>: Codec,
{
	/// Produce proof for a key query.
	pub fn storage(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
		let mut read_overlay = S::Overlay::default();
		let eph = Ephemeral::new(
			self.backend.backend_storage(),
			&mut read_overlay,
		);

		let map_e = |e| format!("Trie lookup error: {}", e);

		read_trie_value_with::<T, _, Ephemeral<S, T::Hash>>(
			&eph,
			self.backend.root(),
			key,
			&mut *self.proof_recorder,
		).map_err(map_e)
	}

	/// Produce proof for a child key query.
	pub fn child_storage(
		&mut self,
		child_info: &ChildInfo,
		key: &[u8]
	) -> Result<Option<Vec<u8>>, String> {
		let storage_key = child_info.storage_key();
		let root = self.storage(storage_key)?
			.and_then(|r| Decode::decode(&mut &r[..]).ok())
			.unwrap_or(empty_child_trie_root::<T>());

		let mut read_overlay = S::Overlay::default();
		let eph = Ephemeral::new(
			self.backend.backend_storage(),
			&mut read_overlay,
		);

		let map_e = |e| format!("Trie lookup error: {}", e);

		read_child_trie_value_with::<T, _, _>(
			child_info.keyspace(),
			&eph,
			&root.as_ref(),
			key,
			&mut *self.proof_recorder
		).map_err(map_e)
	}

	/// Produce proof for the whole backend.
	pub fn record_all_keys(&mut self) {
		let mut read_overlay = S::Overlay::default();
		let eph = Ephemeral::new(
			self.backend.backend_storage(),
			&mut read_overlay,
		);

		let mut iter = move || -> Result<(), Box<TrieError<T>>> {
			let root = self.backend.root();
			record_all_keys::<T, _>(&eph, root, &mut *self.proof_recorder)
		};

		if let Err(e) = iter() {
			debug!(target: "trie", "Error while recording all keys: {}", e);
		}
	}
}

/// Global proof recorder, act as a layer over a hash db for recording queried
/// data.
pub type ProofRecorder<H> = Arc<RwLock<HashMap<<H as Hasher>::Out, Option<DBValue>>>>;

/// Try merging two proof recorder, fails when both recorder records different entries.
fn merge_proof_recorder<H: Hasher>(first: ProofRecorder<H>, second: ProofRecorder<H>) -> Option<ProofRecorder<H>> {
	{
		let mut first = first.write();
		let mut second = second.write();
		for (key, value) in std::mem::replace(&mut *second, Default::default()) {
			match first.entry(key) {
				Entry::Occupied(entry) => {
					if entry.get() != &value {
						return None;
					}
				},
				Entry::Vacant(entry) => {
					entry.insert(value);
				},
			}
		}
	}
	Some(first)
}

/// Patricia trie-based backend which also tracks all touched storage trie values.
/// These can be sent to remote node and used as a proof of execution.
pub struct ProvingBackend<S: TrieBackendStorage<T::Hash>, T: TrieConfiguration> (
	pub TrieBackend<ProofRecorderBackend<S, T::Hash>, T>,
);

/// Trie backend storage with its proof recorder.
pub struct ProofRecorderBackend<S: TrieBackendStorage<H>, H: Hasher> {
	backend: S,
	proof_recorder: ProofRecorder<H>,
}

impl<'a, S, T> ProvingBackend<&'a S, T>
	where
		S: TrieBackendStorage<T::Hash>,
		T: TrieConfiguration,
		TrieHash<T>: Codec,
{
	/// Create new proving backend.
	pub fn new(backend: &'a TrieBackend<S, T>) -> Self {
		let proof_recorder = Default::default();
		Self::new_with_recorder(backend, proof_recorder)
	}

	fn new_with_recorder(
		backend: &'a TrieBackend<S, T>,
		proof_recorder: ProofRecorder<T::Hash>,
	) -> Self {
		let essence = backend.essence();
		let root = essence.root().clone();
		let recorder = ProofRecorderBackend {
			backend: essence.backend_storage(),
			proof_recorder,
		};
		ProvingBackend(TrieBackend::new(recorder, root))
	}
}

impl<S, T> ProvingBackend<S, T>
	where
		S: TrieBackendStorage<T::Hash>,
		T: TrieConfiguration,
		TrieHash<T>: Codec,
{
	/// Create new proving backend with the given recorder.
	pub fn from_backend_with_recorder(
		backend: S,
		root: TrieHash<T>,
		proof_recorder: ProofRecorder<T::Hash>,
	) -> Self {
		let recorder = ProofRecorderBackend {
			backend,
			proof_recorder,
		};
		ProvingBackend(TrieBackend::new(recorder, root))
	}

	/// Extract current recording state.
	/// This is sharing a rc over a sync reference.
	pub fn extract_recorder(&self) -> ProofRecorder<T::Hash> {
		self.0.backend_storage().proof_recorder.clone()
	}
}

impl<S: TrieBackendStorage<H>, H: Hasher> TrieBackendStorage<H>
	for ProofRecorderBackend<S, H>
{
	type Overlay = S::Overlay;

	fn get(&self, key: &H::Out, prefix: Prefix) -> Result<Option<DBValue>, String> {
		if let Some(v) = self.proof_recorder.read().get(key) {
			return Ok(v.clone());
		}
		let backend_value =  self.backend.get(key, prefix)?;
		self.proof_recorder.write().insert(key.clone(), backend_value.clone());
		Ok(backend_value)
	}
}

impl<S, T> std::fmt::Debug for ProvingBackend<S, T>
	where
		S: TrieBackendStorage<T::Hash>,
		T: TrieConfiguration,
{
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "ProvingBackend")
	}
}

impl<S, T> ProofRegBackend<T::Hash> for ProvingBackend<S, T>
	where
		S: TrieBackendStorage<T::Hash>,
		T: TrieConfiguration,
		TrieHash<T>: Ord + Codec,
{
	type State = ProofRecorder<T::Hash>;

	fn extract_proof(&self) -> Self::StorageProof {
		let trie_nodes = self.0.essence().backend_storage().proof_recorder
			.read()
			.iter()
			.filter_map(|(_k, v)| v.as_ref().map(|v| v.to_vec()))
			.collect();
		TrieNodesStorageProof::new(trie_nodes)
	}
}

impl<S, T> Backend<T::Hash> for ProvingBackend<S, T>
	where
		S: TrieBackendStorage<T::Hash>,
		T: TrieConfiguration,
		TrieHash<T>: Ord + Codec,
{
	type Error = String;
	type Transaction = S::Overlay;
	type StorageProof = sp_trie::TrieNodesStorageProof;
	type ProofRegBackend = Self;
	type ProofCheckBackend = TrieBackend<MemoryDB<T::Hash>, T>;

	fn storage(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		self.0.storage(key)
	}

	fn child_storage(
		&self,
		child_info: &ChildInfo,
		key: &[u8],
	) -> Result<Option<Vec<u8>>, Self::Error> {
		self.0.child_storage(child_info, key)
	}

	fn for_keys_in_child_storage<F: FnMut(&[u8])>(
		&self,
		child_info: &ChildInfo,
		f: F,
	) {
		self.0.for_keys_in_child_storage(child_info, f)
	}

	fn next_storage_key(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		self.0.next_storage_key(key)
	}

	fn next_child_storage_key(
		&self,
		child_info: &ChildInfo,
		key: &[u8],
	) -> Result<Option<Vec<u8>>, Self::Error> {
		self.0.next_child_storage_key(child_info, key)
	}

	fn for_keys_with_prefix<F: FnMut(&[u8])>(&self, prefix: &[u8], f: F) {
		self.0.for_keys_with_prefix(prefix, f)
	}

	fn for_key_values_with_prefix<F: FnMut(&[u8], &[u8])>(&self, prefix: &[u8], f: F) {
		self.0.for_key_values_with_prefix(prefix, f)
	}

	fn for_child_keys_with_prefix<F: FnMut(&[u8])>(
		&self,
		child_info: &ChildInfo,
		prefix: &[u8],
		f: F,
	) {
		self.0.for_child_keys_with_prefix( child_info, prefix, f)
	}

	fn pairs(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
		self.0.pairs()
	}

	fn keys(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
		self.0.keys(prefix)
	}

	fn child_keys(
		&self,
		child_info: &ChildInfo,
		prefix: &[u8],
	) -> Vec<Vec<u8>> {
		self.0.child_keys(child_info, prefix)
	}

	fn storage_root<'b>(
		&self,
		delta: impl Iterator<Item=(&'b [u8], Option<&'b [u8]>)>,
	) -> (TrieHash<T>, Self::Transaction) where TrieHash<T>: Ord {
		self.0.storage_root(delta)
	}

	fn child_storage_root<'b>(
		&self,
		child_info: &ChildInfo,
		delta: impl Iterator<Item=(&'b [u8], Option<&'b [u8]>)>,
	) -> (TrieHash<T>, bool, Self::Transaction) where TrieHash<T>: Ord {
		self.0.child_storage_root(child_info, delta)
	}

	fn register_overlay_stats(&mut self, _stats: &crate::stats::StateMachineStats) { }

	fn usage_info(&self) -> crate::stats::UsageInfo {
		self.0.usage_info()
	}

	fn as_proof_backend(self) -> Option<Self::ProofRegBackend> {
		Some(self)
	}

	fn from_reg_state(self, previous_recorder: ProofRegStateFor<Self, T::Hash>) -> Option<Self::ProofRegBackend> {
		let root = self.0.essence().root().clone();
		let storage = self.0.into_storage();
		let current_recorder = storage.proof_recorder;
		let backend = storage.backend;
		merge_proof_recorder::<T::Hash>(current_recorder, previous_recorder).map(|merged_recorder|
			ProvingBackend::<S, T>::from_backend_with_recorder(backend, root, merged_recorder)
		)
	}
}

#[cfg(test)]
mod tests {
	use crate::InMemoryBackend;
	use crate::trie_backend::tests::test_trie;
	use super::*;
	use sp_trie::PrefixedMemoryDB;
	use sp_runtime::traits::BlakeTwo256;
	use sp_trie::{StorageProof, Layout};
	use crate::backend::ProofCheckBackend as _;

	type ProofCheckBackend = crate::trie_backend::TrieBackend<
		MemoryDB<BlakeTwo256>,
		Layout<BlakeTwo256>,
	>;

	fn test_proving<'a>(
		trie_backend: &'a TrieBackend<PrefixedMemoryDB<BlakeTwo256>, Layout<BlakeTwo256>>,
	) -> ProvingBackend<&'a PrefixedMemoryDB<BlakeTwo256>, Layout<BlakeTwo256>> {
		ProvingBackend::new(trie_backend)
	}

	#[test]
	fn proof_is_empty_until_value_is_read() {
		let trie_backend = test_trie();
		assert!(test_proving(&trie_backend).extract_proof().is_empty());
	}

	#[test]
	fn proof_is_non_empty_after_value_is_read() {
		let trie_backend = test_trie();
		let backend = test_proving(&trie_backend);
		assert_eq!(backend.storage(b"key").unwrap(), Some(b"value".to_vec()));
		assert!(!backend.extract_proof().is_empty());
	}

	#[test]
	fn proof_is_invalid_when_does_not_contains_root() {
		use sp_core::H256;
		let result = ProofCheckBackend::create_proof_check_backend(
			H256::from_low_u64_be(1),
			TrieNodesStorageProof::empty()
		);
		assert!(result.is_err());
	}

	#[test]
	fn passes_through_backend_calls() {
		let trie_backend = test_trie();
		let proving_backend = test_proving(&trie_backend);
		assert_eq!(trie_backend.storage(b"key").unwrap(), proving_backend.storage(b"key").unwrap());
		assert_eq!(trie_backend.pairs(), proving_backend.pairs());

		let (trie_root, mut trie_mdb) = trie_backend.storage_root(::std::iter::empty());
		let (proving_root, mut proving_mdb) = proving_backend.storage_root(::std::iter::empty());
		assert_eq!(trie_root, proving_root);
		assert_eq!(trie_mdb.drain(), proving_mdb.drain());
	}

	#[test]
	fn proof_recorded_and_checked() {
		let contents = (0..64).map(|i| (vec![i], Some(vec![i]))).collect::<Vec<_>>();
		let in_memory = InMemoryBackend::<Layout<BlakeTwo256>>::default();
		let in_memory = in_memory.update(vec![(None, contents)]);
		let in_memory_root = in_memory.storage_root(::std::iter::empty()).0;
		(0..64).for_each(|i| assert_eq!(in_memory.storage(&[i]).unwrap().unwrap(), vec![i]));

		let trie = &in_memory;
		let trie_root = trie.storage_root(::std::iter::empty()).0;
		assert_eq!(in_memory_root, trie_root);
		(0..64).for_each(|i| assert_eq!(trie.storage(&[i]).unwrap().unwrap(), vec![i]));

		// clone to avoid &TrieBackend implementation
		let proving = trie.clone().as_proof_backend().unwrap();
		assert_eq!(proving.storage(&[42]).unwrap().unwrap(), vec![42]);

		let proof = proving.extract_proof();

		let proof_check = ProofCheckBackend::create_proof_check_backend(in_memory_root.into(), proof).unwrap();
		assert_eq!(proof_check.storage(&[42]).unwrap().unwrap(), vec![42]);
	}

	#[test]
	fn proof_recorded_and_checked_with_child() {
		let child_info_1 = ChildInfo::new_default(b"sub1");
		let child_info_2 = ChildInfo::new_default(b"sub2");
		let child_info_1 = &child_info_1;
		let child_info_2 = &child_info_2;
		let contents = vec![
			(None, (0..64).map(|i| (vec![i], Some(vec![i]))).collect()),
			(Some(child_info_1.clone()),
				(28..65).map(|i| (vec![i], Some(vec![i]))).collect()),
			(Some(child_info_2.clone()),
				(10..15).map(|i| (vec![i], Some(vec![i]))).collect()),
		];
		let in_memory = InMemoryBackend::<Layout<BlakeTwo256>>::default();
		let in_memory = in_memory.update(contents);
		let child_storage_keys = vec![child_info_1.to_owned(), child_info_2.to_owned()];
		let in_memory_root = in_memory.full_storage_root(
			std::iter::empty(),
			child_storage_keys.iter().map(|k|(k, std::iter::empty()))
		).0;
		(0..64).for_each(|i| assert_eq!(
			in_memory.storage(&[i]).unwrap().unwrap(),
			vec![i]
		));
		(28..65).for_each(|i| assert_eq!(
			in_memory.child_storage(child_info_1, &[i]).unwrap().unwrap(),
			vec![i]
		));
		(10..15).for_each(|i| assert_eq!(
			in_memory.child_storage(child_info_2, &[i]).unwrap().unwrap(),
			vec![i]
		));

		let trie = &in_memory;
		let trie_root = trie.storage_root(::std::iter::empty()).0;
		assert_eq!(in_memory_root, trie_root);
		(0..64).for_each(|i| assert_eq!(
			trie.storage(&[i]).unwrap().unwrap(),
			vec![i]
		));

		let proving = trie.clone().as_proof_backend().unwrap();
		assert_eq!(proving.storage(&[42]).unwrap().unwrap(), vec![42]);

		let proof = proving.extract_proof();

		let proof_check = ProofCheckBackend::create_proof_check_backend(
			in_memory_root.into(),
			proof
		).unwrap();
		assert!(proof_check.storage(&[0]).is_err());
		assert_eq!(proof_check.storage(&[42]).unwrap().unwrap(), vec![42]);
		// note that it is include in root because proof close
		assert_eq!(proof_check.storage(&[41]).unwrap().unwrap(), vec![41]);
		assert_eq!(proof_check.storage(&[64]).unwrap(), None);

		let proving = ProvingBackend::new(trie);
		assert_eq!(proving.child_storage(child_info_1, &[64]), Ok(Some(vec![64])));

		let proof = proving.extract_proof();
		let proof_check = ProofCheckBackend::create_proof_check_backend(
			in_memory_root.into(),
			proof
		).unwrap();
		assert_eq!(
			proof_check.child_storage(child_info_1, &[64]).unwrap().unwrap(),
			vec![64]
		);
	}
}
