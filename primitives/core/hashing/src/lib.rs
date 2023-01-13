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

//! Hashing Functions.

#![warn(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

use core::hash::Hasher;

extern crate alloc; // TODO probably don't need the cached blake3 here, remove then.

use byteorder::{ByteOrder, LittleEndian};
use digest::{
	consts::{U16, U32, U8},
	Digest,
};

/// Do a Blake3 hash and place result in `dest`.
pub fn blake3_into(data: &[u8], dest: &mut [u8; 32]) {
	dest.copy_from_slice(blake3_hash::hash(data).as_bytes());
}

/// Do a Blake3 hash and return result.
pub fn blake3(data: &[u8]) -> [u8; 32] {
	let mut r = [0; 32];
	blake3_into(data, &mut r);
	r
}

/// State for processing blake3 in multiple steps.
#[derive(Clone)]
pub enum Blake3State {
	/// Standard hashers for blake.
	Inline(blake3_hash::Hasher),
	/// keep trace of merkle info,
	/// allow random changes and proof
	/// construction.
	Cached(CachedBlake3<VecHashes>),
}

impl Blake3State {
	/// Start hashing.
	pub fn new(with_merkle_cache: bool) -> Self {
		if with_merkle_cache {
			let mut hashes: alloc::vec::Vec<VecHashes> = Default::default();
			hashes.push(alloc::vec![Default::default()]); // TODO root of [] instead (but need it as constant
			Blake3State::Cached(CachedBlake3 { hashes, size: 0, last_chunk: None, root: None })
		} else {
			Blake3State::Inline(blake3_hash::Hasher::new())
		}
	}

	/// Hash more content.
	pub fn update(&mut self, data: &[u8]) {
		match self {
			Blake3State::Inline(hasher) => {
				hasher.update(data);
			},
			Blake3State::Cached(cache) => cache.update(data),
		}
	}

	/// Obtain hash.
	pub fn finalize(self) -> [u8; 32] {
		match self {
			Blake3State::Inline(hasher) => hasher.finalize().into(),
			Blake3State::Cached(mut cache) => cache.root(),
		}
	}
}

#[derive(Clone)]
pub struct CachedHash {
	hash: blake3_hash::Hash,
	parent_processed: bool,
	register_for_proof: bool,
}

impl Default for CachedHash {
	fn default() -> Self {
		CachedHash { hash: [0u8; 32].into(), parent_processed: false, register_for_proof: false }
	}
}

/// Blake3 hashers allowing efficient random modification.
/// Uses more memory as it caches all calculated hashes from
/// the internal binary tree.
#[derive(Clone)]
pub struct CachedBlake3<Hashes> {
	/// any hashes computed are kept for random changes.
	/// Index is build from (depth, index_in_depth), with root at depth 0.
	/// TODO the inner vec will grow uselessly for proofs: use some different struct in this case.
	hashes: alloc::vec::Vec<Hashes>,
	/// size of processed content
	size: u64,
	/// Last chunk (only for update).
	last_chunk: Option<blake3_hash::guts::ChunkState>,
	/// Root, cannot use hashes as they are parent flag only.
	root: Option<[u8; 32]>,
}

/// CachedHash at a given depth.
trait CachedHashes {
	fn empty() -> Self;
	fn next_modified(&self, last: Option<usize>) -> Option<usize>;
	fn get_cached(&self, ix: usize) -> Option<&CachedHash>;
	// TODO cehck if can remplace by just ste_hash
	fn get_cached_or_new_mut(&mut self, ix: usize) -> &mut CachedHash;
}

type VecHashes = alloc::vec::Vec<Option<CachedHash>>;

impl CachedHashes for VecHashes {
	fn empty() -> Self {
		Default::default()
	}

	fn next_modified(&self, last: Option<usize>) -> Option<usize> {
		let mut ix = last.map(|l| l + 1).unwrap_or(0);
		loop {
			match self.get(ix) {
				Some(Some(cached)) if !cached.parent_processed => return Some(ix),
				None => return None,
				_ => (),
			}
			ix += 1;
		}
	}

	fn get_cached(&self, ix: usize) -> Option<&CachedHash> {
		self.get(ix).and_then(Option::as_ref)
	}

	fn get_cached_or_new_mut(&mut self, ix: usize) -> &mut CachedHash {
		if ix >= self.len() {
			self.resize(ix + 1, None);
		}
		if self[ix].is_none() {
			self[ix] = Some(Default::default());
		}
		self[ix].as_mut().expect("init above")
	}
}

#[inline(always)]
fn depth_and_index(size: u64) -> (u32, u64) {
	let depth = 64 - size.leading_zeros();
	let index = size & !(1 << depth);
	(depth, index)
}

#[inline(always)]
fn index(depth: u32, index: u64) -> u64 {
	index | (1 << depth)
}

#[inline(always)]
fn parent(index: u64) -> Option<u32> {
	let depth = depth_and_index(index).0;
	(depth > 0).then(|| depth - 1)
}

#[inline(always)]
fn chunk_index(len: u64) -> u64 {
	if len == 0 {
		0 // TODO make it unreachable
	} else {
		(len - 1) / blake3_hash::guts::CHUNK_LEN as u64
	}
}

#[inline(always)]
fn chunk_aligned(len: u64) -> bool {
	(len % blake3_hash::guts::CHUNK_LEN as u64) == 0
}

impl<Hashes: CachedHashes> CachedBlake3<Hashes> {
	fn grow(&mut self, new_size: u64) {
		let previous = chunk_index(self.size);
		let new = chunk_index(new_size);
		let old_depth = depth_and_index(previous).0;
		let new_depth = depth_and_index(new).0;
		for _ in 0..new_depth - old_depth {
			// TODO reverse hashes order to push
			self.hashes.insert(0, Hashes::empty());
		}
	}

	/// If not last this expect a full chunk (chunk persistance or fetch
	/// is not at this level).
	/// Expect `grow` to already been called.
	fn update_at(&mut self, chunk: &[u8], at: Option<u64>) {
		debug_assert!(chunk.len() <= blake3_hash::guts::CHUNK_LEN);
		self.root = None;
		let (at, mut state) = if let Some(at) = at {
			debug_assert!(at <= chunk_index(self.size));
			debug_assert!(
				!(at < chunk_index(self.size) && chunk.len() != blake3_hash::guts::CHUNK_LEN)
			);
			if self.last_chunk.is_none() {
				self.last_chunk = Some(blake3_hash::guts::ChunkState::new(at));
			}
			(at, blake3_hash::guts::ChunkState::new(at))
		} else {
			let at = chunk_index(self.size + 1);
			if self.last_chunk.is_none() {
				self.last_chunk = Some(blake3_hash::guts::ChunkState::new(at));
			}
			if chunk_aligned(self.size + chunk.len() as u64) {
				let Some(state) = self.last_chunk.take() else {
					return
				};
				(at, state)
			} else {
				let state = self.last_chunk.as_mut().expect("Initialized above");
				state.update(chunk);
				return
			}
		};
		state.update(chunk);

		if self.hashes.len() == 1 {
			// TODO is there a into_bytes instead of this as_bytes??
			self.root = Some(*state.clone().finalize(true).as_bytes());
		}

		let depth = self.hashes.len() - 1;
		let at = at as usize;
		let mut hash = self.hashes[depth].get_cached_or_new_mut(at);
		hash.hash = state.finalize(false);
		hash.parent_processed = false;
	}

	fn update(&mut self, mut data: &[u8]) {
		if data.len() == 0 {
			return
		}
		let used = self.size % blake3_hash::guts::CHUNK_LEN as u64;
		let mut used = used as usize;
		while data.len() > 0 {
			let len = sp_std::cmp::min(data.len(), blake3_hash::guts::CHUNK_LEN - used);
			let new_size = self.size + len as u64;
			self.grow(new_size);
			self.update_at(&data[..len], None);
			self.size += len as u64;
			data = &data[len..][..];
			used = 0;
		}
	}

	fn root(&mut self) -> [u8; 32] {
		if let Some(root) = self.root.as_ref() {
			return *root
		}

		if self.hashes.len() == 1 {
			if self.last_chunk.is_none() {
				let mut state = blake3_hash::guts::ChunkState::new(0);

				self.last_chunk = Some(state);
			}
			let state = self.last_chunk.as_ref().expect("init above");
			let result = *state.clone().finalize(true).as_bytes(); // TODO facto
			self.root = Some(result.clone());
			return result
		}

		if let Some(last) = &mut self.last_chunk {
			let hash_last = last.clone().finalize(false);
			let hashes = self.hashes.last_mut().expect("`grow` has been called");
			let at = chunk_index(self.size) as usize;
			let mut hash = hashes.get_cached_or_new_mut(at);
			hash.hash = hash_last;
			hash.parent_processed = false;
		}

		let last_chunk = chunk_index(self.size);
		for d in (1..self.hashes.len()).rev() {
			let mut skip_ix = None;
			let len = (last_chunk >> (self.hashes.len() - 1 - d)) as usize + 1;
			let mut last = None;
			while let Some(i) = self.hashes[d].next_modified(last) {
				last = Some(i);
				let mut hashes = &mut self.hashes[d];
				if let Some(skip) = skip_ix.take() {
					if i == skip {
						continue
					}
				}

				// allow iter on ordered modified only.
				let i = if i % 2 == 0 {
					skip_ix = Some(i + 1);
					i
				} else {
					i - 1
				};

				let parent_hash = if i == len - 1 {
					if hashes.get_cached(i).map(|h| h.parent_processed).unwrap_or(true) {
						continue
					}
					let mut hash = hashes.get_cached_or_new_mut(i);
					hash.parent_processed = true;
					hash.hash.clone()
				} else {
					if hashes.get_cached(i).map(|h| h.parent_processed).unwrap_or(true) &&
						hashes.get_cached(i + 1).map(|h| h.parent_processed).unwrap_or(true)
					{
						continue
					}
					hashes.get_cached_or_new_mut(i).parent_processed = true;
					hashes.get_cached_or_new_mut(i + 1).parent_processed = true;
					let l = &hashes.get_cached(i).expect("new above").hash;
					let r = &hashes.get_cached(i + 1).expect("new above").hash;

					if d == 1 {
						self.root = Some(*blake3_hash::guts::parent_cv(l, r, true).as_bytes());
					}
					blake3_hash::guts::parent_cv(l, r, false)
				};
				let parent_depth = &mut self.hashes[d - 1];
				let parent_index = i / 2;
				let hash = parent_depth.get_cached_or_new_mut(parent_index);
				hash.hash = parent_hash;
				hash.parent_processed = false;
			}
		}
		if let Some(root) = &self.root {
			*root
		} else {
			// depth 1
			let result = self.hashes[0].get_cached(0).expect("TODO").hash.as_bytes();
			self.root = Some(result.clone());
			*result
		}
	}
}

/// Do a Blake2 512-bit hash and place result in `dest`.
pub fn blake2_512_into(data: &[u8], dest: &mut [u8; 64]) {
	dest.copy_from_slice(blake2::Blake2b512::digest(data).as_slice());
}

/// Do a Blake2 512-bit hash and return result.
pub fn blake2_512(data: &[u8]) -> [u8; 64] {
	let mut r = [0; 64];
	blake2_512_into(data, &mut r);
	r
}

type Blake2b256 = blake2::Blake2b<U32>;

/// Do a Blake2 256-bit hash and place result in `dest`.
pub fn blake2_256_into(data: &[u8], dest: &mut [u8; 32]) {
	dest.copy_from_slice(Blake2b256::digest(data).as_slice());
}

/// Do a Blake2 256-bit hash and return result.
pub fn blake2_256(data: &[u8]) -> [u8; 32] {
	let mut r = [0; 32];
	blake2_256_into(data, &mut r);
	r
}

/// State for processing blake2b 256 in multiple steps.
#[derive(Clone)]
pub struct Blake2b256State(Blake2b256);

impl Blake2b256State {
	/// Start hashing.
	pub fn new() -> Self {
		Blake2b256State(Blake2b256::new())
	}

	/// Hash more content.
	pub fn update(&mut self, data: &[u8]) {
		self.0.update(data)
	}

	/// Obtain hash.
	pub fn finalize(self) -> [u8; 32] {
		self.0.finalize().into()
	}
}

/// Do a Blake2 128-bit hash and place result in `dest`.
pub fn blake2_128_into(data: &[u8], dest: &mut [u8; 16]) {
	type Blake2b128 = blake2::Blake2b<U16>;
	dest.copy_from_slice(Blake2b128::digest(data).as_slice());
}

/// Do a Blake2 128-bit hash and return result.
pub fn blake2_128(data: &[u8]) -> [u8; 16] {
	let mut r = [0; 16];
	blake2_128_into(data, &mut r);
	r
}

/// Do a Blake2 64-bit hash and place result in `dest`.
pub fn blake2_64_into(data: &[u8], dest: &mut [u8; 8]) {
	type Blake2b64 = blake2::Blake2b<U8>;
	dest.copy_from_slice(Blake2b64::digest(data).as_slice());
}

/// Do a Blake2 64-bit hash and return result.
pub fn blake2_64(data: &[u8]) -> [u8; 8] {
	let mut r = [0; 8];
	blake2_64_into(data, &mut r);
	r
}

/// Do a XX 64-bit hash and place result in `dest`.
pub fn twox_64_into(data: &[u8], dest: &mut [u8; 8]) {
	let r0 = twox_hash::XxHash::with_seed(0).chain_update(data).finish();
	LittleEndian::write_u64(&mut dest[0..8], r0);
}

/// Do a XX 64-bit hash and return result.
pub fn twox_64(data: &[u8]) -> [u8; 8] {
	let mut r: [u8; 8] = [0; 8];
	twox_64_into(data, &mut r);
	r
}

/// Do a XX 128-bit hash and place result in `dest`.
pub fn twox_128_into(data: &[u8], dest: &mut [u8; 16]) {
	let r0 = twox_hash::XxHash::with_seed(0).chain_update(data).finish();
	let r1 = twox_hash::XxHash::with_seed(1).chain_update(data).finish();
	LittleEndian::write_u64(&mut dest[0..8], r0);
	LittleEndian::write_u64(&mut dest[8..16], r1);
}

/// Do a XX 128-bit hash and return result.
pub fn twox_128(data: &[u8]) -> [u8; 16] {
	let mut r: [u8; 16] = [0; 16];
	twox_128_into(data, &mut r);
	r
}

/// Do a XX 256-bit hash and place result in `dest`.
pub fn twox_256_into(data: &[u8], dest: &mut [u8; 32]) {
	let r0 = twox_hash::XxHash::with_seed(0).chain_update(data).finish();
	let r1 = twox_hash::XxHash::with_seed(1).chain_update(data).finish();
	let r2 = twox_hash::XxHash::with_seed(2).chain_update(data).finish();
	let r3 = twox_hash::XxHash::with_seed(3).chain_update(data).finish();
	LittleEndian::write_u64(&mut dest[0..8], r0);
	LittleEndian::write_u64(&mut dest[8..16], r1);
	LittleEndian::write_u64(&mut dest[16..24], r2);
	LittleEndian::write_u64(&mut dest[24..32], r3);
}

/// Do a XX 256-bit hash and return result.
pub fn twox_256(data: &[u8]) -> [u8; 32] {
	let mut r: [u8; 32] = [0; 32];
	twox_256_into(data, &mut r);
	r
}

/// Do a keccak 256-bit hash and return result.
pub fn keccak_256(data: &[u8]) -> [u8; 32] {
	let mut output = [0u8; 32];
	output.copy_from_slice(sha3::Keccak256::digest(data).as_slice());
	output
}

/// Do a keccak 512-bit hash and return result.
pub fn keccak_512(data: &[u8]) -> [u8; 64] {
	let mut output = [0u8; 64];
	output.copy_from_slice(sha3::Keccak512::digest(data).as_slice());
	output
}

/// Do a sha2 256-bit hash and return result.
pub fn sha2_256(data: &[u8]) -> [u8; 32] {
	let mut output = [0u8; 32];
	output.copy_from_slice(sha2::Sha256::digest(data).as_slice());
	output
}
