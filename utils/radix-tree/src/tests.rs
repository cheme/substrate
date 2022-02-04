// This file is part of Substrate.

// Copyright (C) 2020-2020 Parity Technologies (UK) Ltd.
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

//! Tests an fuzzing base code.

#[cfg(any(test, feature = "fuzzer"))]
use crate::{Node256NoBackend, Node256NoBackendART, Node256TestBackendART,
	Node16NoBackend, Node4NoBackend, Node4LimitTestBackend, Node256TestBackend, ArtTestBackend};
#[cfg(any(test, feature = "fuzzer"))]
#[cfg(feature = "hash_backend")]
use crate::Node256HashedTestBackend;

#[cfg(any(test, feature = "fuzzer"))]
type BackendFor<N> = <N as crate::TreeConf>::Backend;

macro_rules! test_for {
	($module_name: ident, $backend_conf: ident, $check_backend_ser: expr) => {


#[cfg(any(test, feature = "fuzzer"))]
pub mod $module_name {
	use crate::*;
	use super::BackendFor;
	use alloc::collections::btree_map::BTreeMap;
	#[cfg(test)]
	use alloc::vec;

	const CHECK_BACKEND: bool = $check_backend_ser;
	type TreeConf = super::$backend_conf<Vec<u8>>;

	fn new_backend() -> BackendFor<TreeConf> {
		BackendFor::<TreeConf>::default()
	}

	#[test]
	fn empty_are_equals() {
		let backend = new_backend();
		let mut t1 = Tree::<TreeConf>::new(backend.clone());
		let mut t2 = Tree::<TreeConf>::new(backend.clone());
		assert!(compare_tree(&mut t1, &mut t2, Some(0)));
	}

	#[test]
	fn inserts_are_equals() {
		let backend = new_backend();
		let mut t1 = Tree::<TreeConf>::new(backend.clone());
		let mut t2 = Tree::<TreeConf>::new(backend.clone());
		let value1 = b"value1".to_vec();
		assert_eq!(None, t1.insert(b"key1", value1.clone()));
		assert_eq!(None, t2.insert(b"key1", value1.clone()));
		assert!(compare_tree(&mut t1, &mut t2, Some(1)));
		assert_eq!(Some(value1.clone()), t1.insert(b"key1", b"value2".to_vec()));
		assert_eq!(Some(value1.clone()), t2.insert(b"key1", b"value2".to_vec()));
		assert!(compare_tree(&mut t1, &mut t2, Some(1)));
		assert_eq!(None, t1.insert(b"key2", value1.clone()));
		assert_eq!(None, t2.insert(b"key2", value1.clone()));
		assert!(compare_tree(&mut t1, &mut t2, Some(2)));
		assert_eq!(None, t2.insert(b"key3", value1.clone()));
		assert!(!compare_tree(&mut t1, &mut t2, None));
	}

	// TODO this compare tree is a bit useless.
	#[cfg(test)]
	fn compare_tree(left: &mut Tree::<TreeConf>, right: &mut Tree::<TreeConf>, mut nb_elt: Option<usize>) -> bool {
		if CHECK_BACKEND {
			let left_node = left.iter_mut();
			let left = left_node.value_iter();
			let right_node = right.iter_mut();
			let mut right = right_node.value_iter();
			for l in left {
				if nb_elt == Some(0) {
					return false;
				} else {
					nb_elt = nb_elt.map(|nb_elt| nb_elt - 1);
				}
				if let Some(r) = right.next() {
					if &l.0[..] != &r.0[..] {
						return false;
					}
					if &l.1[..] != &r.1[..] {
						return false;
					}
				} else {
					return false;
				}
			}
			if right.next().is_some() || nb_elt.map(|nb| nb > 0).unwrap_or(false) {
				return false;
			}
			true
		} else {
			let left_node = left.iter();
			let left = left_node.value_iter();
			let right_node = right.iter();
			let mut right = right_node.value_iter();
			for l in left {
				if nb_elt == Some(0) {
					return false;
				} else {
					nb_elt = nb_elt.map(|nb_elt| nb_elt - 1);
				}
				if let Some(r) = right.next() {
					if &l.0[..] != &r.0[..] {
						return false;
					}
					if &l.1[..] != &r.1[..] {
						return false;
					}
				} else {
					return false;
				}
			}
			if right.next().is_some() || nb_elt.map(|nb| nb > 0).unwrap_or(false) {
				return false;
			}
			true
		}
	}

	fn compare_iter<K: Borrow<[u8]>>(left: &mut Tree::<TreeConf>, right: &BTreeMap<K, Vec<u8>>) -> bool {
		let mut right = right.iter();
		if CHECK_BACKEND {
			let left_node = left.iter_mut();
			for l in left_node.value_iter() {
				if let Some(r) = right.next() {
					if &l.0[..] != &r.0.borrow()[..] {
						return false;
					}
					if &l.1[..] != &r.1[..] {
						return false;
					}
				} else {
					return false;
				}
			}
		} else {
			let left_node = left.iter();
			for l in left_node.value_iter() {
				if let Some(r) = right.next() {
					if &l.0[..] != &r.0.borrow()[..] {
						return false;
					}
					if &l.1[..] != &r.1[..] {
						return false;
					}
				} else {
					return false;
				}
			}
		};

		if right.next().is_some() {
			return false;
		}
		true
	}

	#[test]
	fn compare_btree() {
		let backend = new_backend();
		let mut t1 = Tree::<TreeConf>::new(backend.clone());
		let mut t2 = BTreeMap::<&'static [u8], Vec<u8>>::new();
		let value1 = b"value1".to_vec();
		assert_eq!(None, t1.insert(b"key1", value1.clone()));
		assert_eq!(None, t2.insert(b"key1", value1.clone()));
		assert!(compare_iter(&mut t1, &t2));
		assert_eq!(Some(value1.clone()), t1.insert(b"key1", b"value2".to_vec()));
		assert_eq!(Some(value1.clone()), t2.insert(b"key1", b"value2".to_vec()));
		assert!(compare_iter(&mut t1, &t2));
		assert_eq!(None, t1.insert(b"key2", value1.clone()));
		assert_eq!(None, t2.insert(b"key2", value1.clone()));
		assert!(compare_iter(&mut t1, &t2));
		assert_eq!(None, t1.insert(b"key3", value1.clone()));
		assert!(!compare_iter(&mut t1, &t2));
		t1.commit();
		if CHECK_BACKEND {
			assert_eq!(None, t2.insert(b"key3", value1.clone()));
			let mut t3 = Tree::<TreeConf>::from_backend(backend.clone());
			assert!(compare_iter(&mut t3, &mut t2));
		}
	}

	#[test]
	fn test_seek_iter() {
		// TODO add iter_prefix!! and mut variant!!!
		fn test(
			keys: &[Vec<u8>],
			key_seek_path: &[Vec<u8>],
			key_iter_path: &[Vec<u8>],
			key_iter_prefix_path: &[Vec<u8>],
		) {
			let backend = new_backend();
			let mut t1 = Tree::<TreeConf>::new(backend.clone());
			let value1 = b"value1".to_vec();
			for key in keys.iter() {
				assert_eq!(None, t1.insert(key.as_slice(), value1.clone()));
			}
			let dest = &b"key1"[..];
			let mut index = 0;
			if !CHECK_BACKEND {
				let mut seek_iter = t1.seek_iter(dest).value_iter();
				while let Some((key, _value)) = seek_iter.next() {
					assert_eq!(key, key_seek_path[index]);
					index += 1;
				}
				assert_eq!(index, key_seek_path.len());
				index = 0;
				let mut iter = seek_iter.node_iter().iter().value_iter();
				while let Some((key, _value)) = iter.next() {
					assert_eq!(key, key_iter_path[index]);
					index += 1;
				}
				assert_eq!(index, key_iter_path.len());

				let mut seek_iter = t1.seek_iter(dest).value_iter();
				while seek_iter.next().is_some() { }
				index = 0;
				let mut iter = seek_iter.node_iter().iter_prefix().value_iter();
				while let Some((key, _value)) = iter.next() {
					assert_eq!(key, key_iter_prefix_path[index]);
					index += 1;
				}
				assert_eq!(index, key_iter_prefix_path.len());
			}
			let mut seek_iter = t1.seek_iter_mut(dest).value_iter();
			let mut index = 0;
			while let Some((key, _value)) = seek_iter.next() {
				assert_eq!(key, key_seek_path[index]);
				index += 1;
			}
			assert_eq!(index, key_seek_path.len());
			index = 0;
			let mut iter = seek_iter.node_iter().iter_prefix().value_iter();
			while let Some((key, _value)) = iter.next() {
				assert_eq!(key, key_iter_prefix_path[index]);
				index += 1;
			}
			assert_eq!(index, key_iter_prefix_path.len());
		}

		let keys = &[b"k".to_vec(), b"key123".to_vec(), b"key3".to_vec()][..];
		let key_seek_path = &[b"k".to_vec()][..];
		let key_iter_path = &[b"key123".to_vec(), b"key3".to_vec()][..];
		let key_iter_prefix_path = &[b"key123".to_vec()][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);
		let keys = &[b"key".to_vec(), b"key2".to_vec()][..];
		let key_seek_path = &[b"key".to_vec()][..];
		let key_iter_path = &[b"key2".to_vec()][..];
		let key_iter_prefix_path = &[][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);
		let keys = &[b"key".to_vec(), b"key123".to_vec(), b"key2".to_vec()][..];
		let key_seek_path = &[b"key".to_vec()][..];
		let key_iter_path = &[b"key123".to_vec(), b"key2".to_vec()][..];
		let key_iter_prefix_path = &[b"key123".to_vec()][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);
		let keys = &[b"key".to_vec()][..];
		let key_seek_path = &[b"key".to_vec()][..];
		let key_iter_path = &[][..];
		let key_iter_prefix_path = &[][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);
		let keys = &[b"key".to_vec(), b"l".to_vec()][..];
		let key_seek_path = &[b"key".to_vec()][..];
		let key_iter_path = &[b"l".to_vec()][..];
		let key_iter_prefix_path = &[][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);
		let keys = &[b"ab".to_vec(), b"key".to_vec(), b"l".to_vec()][..];
		let key_seek_path = &[b"key".to_vec()][..];
		let key_iter_path = &[b"l".to_vec()][..];
		let key_iter_prefix_path = &[][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);
		let keys = &[b"ke".to_vec(), b"key".to_vec(), b"lllllll".to_vec()][..];
		let key_seek_path = &[b"ke".to_vec(), b"key".to_vec()][..];
		let key_iter_path = &[b"lllllll".to_vec()][..];
		let key_iter_prefix_path = &[][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);
		let keys = &[b"key1".to_vec()][..];
		let key_seek_path = &[b"key1".to_vec()][..];
		let key_iter_path = &[][..];
		let key_iter_prefix_path = &[][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);
		let keys = &[b"ke".to_vec(), b"key".to_vec(), b"key12".to_vec()][..];
		let key_seek_path = &[b"ke".to_vec(), b"key".to_vec()][..];
		let key_iter_path = &[b"key12".to_vec()][..];
		let key_iter_prefix_path = &[b"key12".to_vec()][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);
		let keys = &[b"k".to_vec(), b"key1".to_vec()][..];
		let key_seek_path = &[b"k".to_vec(), b"key1".to_vec()][..];
		let key_iter_path = &[][..];
		let key_iter_prefix_path = &[][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);
		let keys = &[b"ke".to_vec(), b"key1".to_vec(), b"key12".to_vec()][..];
		let key_seek_path = &[b"ke".to_vec(), b"key1".to_vec()][..];
		let key_iter_path = &[b"key12".to_vec()][..];
		let key_iter_prefix_path = &[b"key12".to_vec()][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);
		let keys = &[b"key12".to_vec()][..];
		let key_seek_path = &[][..];
		let key_iter_path = &[b"key12".to_vec()][..];
		let key_iter_prefix_path = &[b"key12".to_vec()][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);
		let keys = &[b"i".to_vec(), b"j".to_vec()][..];
		let key_seek_path = &[][..];
		let key_iter_path = &[][..];
		let key_iter_prefix_path = &[][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);
		let keys = &[b"i".to_vec()][..];
		let key_seek_path = &[][..];
		let key_iter_path = &[][..];
		let key_iter_prefix_path = &[][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);

		// middle seek for empty stack
		let keys = &[b"key0".to_vec()][..];
		let key_seek_path = &[][..];
		let key_iter_path = &[][..];
		let key_iter_prefix_path = &[][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);
		let keys = &[b"key2".to_vec()][..];
		let key_seek_path = &[][..];
		let key_iter_path = &[b"key2".to_vec()][..];
		let key_iter_prefix_path = &[][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);

		// middle seek for non empty stack
		let keys = &[b"ke".to_vec(), b"key0".to_vec()][..];
		let key_seek_path = &[b"ke".to_vec()][..];
		let key_iter_path = &[][..];
		let key_iter_prefix_path = &[][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);
		let keys = &[b"ke".to_vec(), b"key2".to_vec()][..];
		let key_seek_path = &[b"ke".to_vec()][..];
		let key_iter_path = &[b"key2".to_vec()][..];
		let key_iter_prefix_path = &[][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);
		let keys = &[b"k".to_vec(), b"key2".to_vec()][..];
		let key_seek_path = &[b"k".to_vec()][..];
		let key_iter_path = &[b"key2".to_vec()][..];
		let key_iter_prefix_path = &[][..];
		test(keys, key_seek_path, key_iter_path, key_iter_prefix_path);
	}

	fn fuzz_to_data(input: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
		let mut result = Vec::new();
		// enc = (minkeylen, maxkeylen (min max up to 32), datas)
		// fix data len 2 bytes
		let mut minkeylen = if let Some(v) = input.get(0) {
			let mut v = *v & 31u8;
			v = v + 1;
			v
		} else { return result; };
		let mut maxkeylen = if let Some(v) = input.get(1) {
			let mut v = *v & 31u8;
			v = v + 1;
			v
		} else { return result; };

		if maxkeylen < minkeylen {
			let v = minkeylen;
			minkeylen = maxkeylen;
			maxkeylen = v;
		}
		let mut ix = 2;
		loop {
			let keylen = if let Some(v) = input.get(ix) {
				let mut v = *v & 31u8;
				v = v + 1;
				v = core::cmp::max(minkeylen, v);
				v = core::cmp::min(maxkeylen, v);
				v as usize
			} else { break };
			let key = if input.len() > ix + keylen {
				input[ix..ix+keylen].to_vec()
			} else { break };
			ix += keylen;
			let val = if input.len() > ix + 2 {
				input[ix..ix+2].to_vec()
			} else { break };
			result.push((key,val));
		}
		result
	}

	fn fuzz_removal(data: Vec<(Vec<u8>, Vec<u8>)>) -> Vec<Do> {
		let mut res = Vec::new();
		let mut existing = None;
		for (a, d) in data.into_iter().enumerate() {
			if existing == None {
				existing = Some(a%2);
			}
			if existing.unwrap() == 0 {
				if a % 9 == 6
				|| a % 9 == 7
				|| a % 9 == 8 {
					// a random removal some time
					res.push(Do::Insert(d.0, d.1));
					continue;
				}
			}
			res.push(Do::Remove(d.0));
		}
		res
	}

	pub fn fuzz_insert_remove(input: &[u8]) {
		let data = fuzz_to_data(input);
		let data = fuzz_removal(data);
		test_insert_remove(data)
	}

	#[derive(Clone)]
	pub enum Do {
		Insert(Vec<u8>, Vec<u8>),
		Remove(Vec<u8>),
		Commit,
	}

	pub fn test_insert_remove(data: Vec<Do>) {
		test_insert_remove_inner(data.clone(), false);
		// This could be remove for fuzzing (slow).
		if CHECK_BACKEND {
			test_insert_remove_inner(data, true);
		}
	}

	fn test_insert_remove_inner(mut data: Vec<Do>, with_clean: bool) {
		// ensure test at end
		data.push(Do::Commit);
		let backend = new_backend();
		let mut a = 0;
		let mut t1 = Tree::<TreeConf>::new(backend.clone());
		let mut t2 = BTreeMap::<Vec<u8>, Vec<u8>>::new();
		while a < data.len() {
			match &data[a] {
				Do::Remove(key) => {
					t1.remove(&key[..], false);
					t2.remove(&key[..]);
				},
				Do::Insert(key, value) => {
					t1.insert(&key[..], value.clone());
					t2.insert(key.clone(), value.clone());
				},
				Do::Commit => {
					assert!(compare_iter(&mut t1, &mut t2));
					if with_clean {
						t1.commit_and_clean_local_nodes();
					} else {
						t1.commit();
					}
					if CHECK_BACKEND {
						let mut t3 = Tree::<TreeConf>::from_backend(backend.clone());
						assert!(compare_iter(&mut t3, &mut t2));
					}
				},
			}
			a += 1;
		}
	}

	#[test]
	fn replay_insert_remove_fuzzing() {
		let datas = [
			vec![0, 0, 5, 75, 9, 1, 48, 58, 17, 9, 17, 9, 0],
			/*vec![0x0,0xa0,0xef,0x8,0xd1,0x72,0x1,0x75,0xd,0xfa,0xea,0x10,0x7,0x4a,0xff,0xf7,0xff,0xff,0x8d,0xd9,0xe2,0xd9,0xff,0xff,0x8d],
			vec![100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 121, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 0, 0, 0, 0, 0, 251, 0, 0, 0, 4],
			vec![0x0,0x0,0x6a,0x6a,0x41,0xa,0xff,0x0,0x0,0x0,0x0,0x0,0x0,0x0,0x0,0x0,0x6a,0x6a,0x41,0xa,0xff,0x0,0x0,0x0,0x0,0x0,0x0,0x0,],
			vec![0x0,0x40,0x3f,0x81,0x7e,0x7e,0x7e,0x7e,0x7e,0x7e,0x7e,0x7e,0x7e,0x7e,0x7e,0x7e,0x7e,0x60,0x0,0xa0,0xef,0x8,0xd1,0x72,0x1,0x75,0xd,0xfa,0xea,0x10,0x7,0x4a,0xff,0xf7,0xff,0xff,0x8d,0xd9,0xe2,0xd9,0xff,0xff,0x8d,0xd9,0xd9,0x1,0x2,0xf5,0xfc,0x21,0x0,0x0,0x46,0xff,0xf8,0x0,0xfe,0xc5,0xfe,0xff,0xa5,0x32,0x48,0x41,0x7d,0x1,0x2d,0x40,0x0,0x48,0x41,0x7d,0x1,0xa8,0xa8,0xa8,0xa8,0xa9,0xa8,0xa4,0xfe,0xd6,0xff,0xc5,0xa5,0x0,0x1,0x74,0x72,0xff,0xff,0x9e,0x0,0x0,0x42,0x42,0x42,0x42,0x42,0x42,0x42,0x42,0x0,0xf8,0x10,0x93,0x0,0x0,0x3d,0x4,0xfb,0x0,0x2b,0x4,0x0,0x7,0xff,0x1,0x3a,0xff,0x1,0x2,0xf5,0xfc,0x21,0x0,0x0,0xbf,0x0,0x7,0xff,0x1,0x3a,0x1,0x0,0x36,0xa5,0x32,0x48,0x41,0x7d,0x1,0x2d,0x0,0x0,0xb8,0xbe,0x82,0xfe,0xd2,0xbf,0xff,0x0,0x63,0x22,0x9c,0x3f,0x31,0x35,0x84,0x0,0x3,0xff,0x1,0x2,0xf5,0x0,0x0,0x0,],
			vec![0x0,0x0,0x3b,0x60,0x32,0xff,0xff,0xff,0x60,0x0,0x3b,0x60,0x32,0xae,],
			vec![0, 1, 0, 45, 0, 0, 0, 0, 0, 0, 0, 0, 75, 0],
			vec![0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 96, 0, 16, 96],
			vec![0, 0, 0, 0, 0, 0, 0, 195, 0, 0, 195, 0, 0, 0],
			vec![0, 0, 5, 75, 9, 1, 48, 58, 17, 9, 17, 9, 0],
			vec![0, 0, 8, 0, 0, 0, 0, 0],
			vec![0, 0, 70, 0, 3, 61, 0, 0],
			vec![0u8, 202, 1, 4, 64, 49, 0, 0],*/
		];
		for data in datas.iter() {
			fuzz_insert_remove(&data[..]);
		}
	}

	#[test]
	fn insert_middle() {
		test_insert_remove(vec![
			Do::Insert(b"start_long".to_vec(), b"value1".to_vec()),
			Do::Insert(b"start".to_vec(), b"value2".to_vec()),
			Do::Commit,
			Do::Remove(b"start".to_vec()),
		]);
	}

	#[test]
	fn insert_deep() {
		test_insert_remove(vec![
			Do::Insert(b"s".to_vec(), b"value2".to_vec()),
			Do::Insert(b"start".to_vec(), b"value2".to_vec()),
			Do::Insert(b"start_long".to_vec(), b"value1".to_vec()),
			Do::Commit,
			Do::Insert(b"start_long_long".to_vec(), b"value3".to_vec()),
		]);
	}

	#[test]
	fn remove_deep() {
		test_insert_remove(vec![
			Do::Insert(b"s".to_vec(), b"value2".to_vec()),
			Do::Insert(b"start".to_vec(), b"value2".to_vec()),
			Do::Insert(b"start_long".to_vec(), b"value1".to_vec()),
			Do::Commit,
			Do::Remove(b"start_long".to_vec()),
		]);
	}
}

}
}

test_for!(test_4_limit, Node4LimitTestBackend, true);
test_for!(test_256, Node256NoBackend, false);
test_for!(test_4, Node4NoBackend, false);
test_for!(test_16, Node16NoBackend, false);
test_for!(test_256_art, Node256NoBackendART, false);
test_for!(test_256_backend, Node256TestBackend, true);
test_for!(test_art_backend, ArtTestBackend, true);
test_for!(test_256_backend_art, Node256TestBackendART, true);
#[cfg(feature = "hash_backend")]
test_for!(test_256_hashed_backend, Node256HashedTestBackend, true);
