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
	Cached(CachedBlake3),
}

impl Blake3State {
	/// Start hashing.
	pub fn new(with_merkle_cache: bool) -> Self {
		if with_merkle_cache {
			let mut hashes: alloc::collections::vec_deque::VecDeque<alloc::vec::Vec<CachedHash>> =
				Default::default();
			hashes.push_front(alloc::vec![Default::default()]); // TODO root of [] instead (but need it as constant
			Blake3State::Cached(CachedBlake3 {
				hashes,
				size: 0,
				all_processed: false,
				last_chunk: [0u8; blake3_hash::guts::CHUNK_LEN],
			})
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

#[derive(Clone, Default)]
struct CachedHash {
	hash: [u8; 32],
	parent_processed: bool,
	register_for_proof: bool,
}

/// Blake3 hashers allowing efficient random modification.
/// Uses more memory as it caches all calculated hashes from
/// the internal binary tree.
#[derive(Clone)]
pub struct CachedBlake3 {
	/// any hashes computed are kept for random changes.
	/// Index is build from (depth, index_in_depth), with root at depth 0.
	/// TODO we only push_front the vecdeque, make it a reverse Vec.
	hashes: alloc::collections::vec_deque::VecDeque<alloc::vec::Vec<CachedHash>>,
	/// size of processed content
	size: u64,
	/// is all tree processed.
	all_processed: bool,
	/// Last chunk data.
	last_chunk: [u8; blake3_hash::guts::CHUNK_LEN],
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

impl CachedBlake3 {
	fn grow(&mut self, size: u64) {
		let previous = self.size / blake3_hash::guts::CHUNK_LEN as u64;
		self.size += size;
		let new = self.size / blake3_hash::guts::CHUNK_LEN as u64;
		let old_depth = depth_and_index(previous).0;
		let new_depth = depth_and_index(new).0;
		for _ in 0..new_depth - old_depth {
			self.hashes.push_front(Default::default());
		}
	}

	fn update_at(&mut self, chunk: Option<&[u8]>, depth: u32, at: u64) {
		self.all_processed = false;
		let mut state = blake3_hash::guts::ChunkState::new(0); // TODO read counter logic and implement
		let chunk = if let Some(chunk) = chunk {
			debug_assert!(chunk.len() == blake3_hash::guts::CHUNK_LEN);
			&chunk[..]
		} else {
			&self.last_chunk[..]
		};
		state.update(chunk);
		let hash = state.finalize(at == 0); // TODO incorrect logic here

		self.hashes[depth as usize][at as usize].hash = *hash.as_bytes();
		self.hashes[depth as usize][at as usize].parent_processed = false;
	}

	fn update(&mut self, mut data: &[u8]) {
		if data.len() == 0 {
			unimplemented!()
		}
		let mut at = self.size / blake3_hash::guts::CHUNK_LEN as u64;
		let used = self.size % blake3_hash::guts::CHUNK_LEN as u64;
		self.grow(data.len() as u64);
		let used = used as usize;
		if used != 0 {
			let remaining = blake3_hash::guts::CHUNK_LEN - used;
			let end = core::cmp::min(used + data.len(), blake3_hash::guts::CHUNK_LEN);
			self.last_chunk[used..end].copy_from_slice(&data[..end - used]);
			data = &data[end - used..][..];
			at += 1;
			let (d, a) = depth_and_index(at);
			self.update_at(None, d, a);
		}
		while data.len() > 0 {
			if data.len() < blake3_hash::guts::CHUNK_LEN {
				self.last_chunk[..data.len()].copy_from_slice(&data[..]);
				for i in data.len()..blake3_hash::guts::CHUNK_LEN {
					self.last_chunk[i] = 0;
				}
				let (d, a) = depth_and_index(at);
				self.update_at(None, d, a);
				data = &[];
			} else {
				let (d, a) = depth_and_index(at);
				self.update_at(Some(&data[..blake3_hash::guts::CHUNK_LEN]), d, a);
				data = &data[blake3_hash::guts::CHUNK_LEN..];
				at += 1;
			}
		}
	}

	fn root(&mut self) -> [u8; 32] {
		if self.size == 0 {
			unimplemented!() // ??
		}
		if self.all_processed {
			return self.hashes[0][0].hash.clone()
		}
		for d in (1..self.hashes.len()).rev() {
			// TODO pair iterate?
			for i in (0..self.hashes.len()).rev() {
				if !self.hashes[d][i].parent_processed {
					self.hashes[d][i].parent_processed = true;
					let pair = if i % 2 == 0 {
						let right = (i + 1 < self.hashes.len()).then(|| {
							self.hashes[d][i + 1].parent_processed = true;
							self.hashes[d][i + 1].hash.clone()
						});
						(Some(self.hashes[d][i].hash.clone()), right)
					} else {
						let left = (i > 0).then(|| {
							self.hashes[d][i - 1].parent_processed = true;
							self.hashes[d][i - 1].hash.clone()
						});
						(left, Some(self.hashes[d][i].hash.clone()))
					};
					let parent_hash = match pair {
						(Some(l), Some(r)) => {
							// TODO should acually store state as left item in persistance.
							let mut state = blake3_hash::guts::ChunkState::new(0); // TODO read counter logic and implement
							state.update(&l[..]);
							state.update(&r[..]);
							*state.finalize(i == 1 || i == 2).as_bytes()
						},
						(Some(l), None) => l,
						(None, Some(h)) => h,
						(None, None) => unreachable!("empty checked above"),
					};
					let parent_depth = &mut self.hashes[d - 1];
					let parent_index = i / 2;
					// TODO there is certainly a vec function that resize
					// with default
					while parent_depth.len() <= parent as usize {
						parent_depth.push(Default::default());
					}
					parent_depth[parent_index as usize].hash = parent_hash;
					parent_depth[parent_index as usize].parent_processed = false;
				}
			}
		}
		self.all_processed = true;
		self.hashes[0][0].hash.clone()
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
