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

use rand::prelude::*;
use std::collections::{BTreeMap, HashMap};

#[macro_use]
extern crate criterion;

use criterion::{black_box, Criterion};

type Tree = radix_tree::Tree<radix_tree::Node256NoBackend<Vec<u8>>>;
//type Tree = radix_tree::Tree<radix_tree::Node256NoBackendART<Vec<u8>>>;

trait Map {
	fn insert(&mut self, key: Vec<u8>, value: Vec<u8>);
	fn get(&self, key: Vec<u8>) -> &Vec<u8>;
	fn remove(&mut self, key: Vec<u8>);
}

impl Map for HashMap<Vec<u8>, Vec<u8>> {
	fn insert(&mut self, key: Vec<u8>, value: Vec<u8>) {
		self.insert(key, value);
	}
	fn get(&self, key: Vec<u8>) -> &Vec<u8> {
		self.get(&key).unwrap()
	}
	fn remove(&mut self, key: Vec<u8>) {
		self.remove(&key);
	}
}

impl Map for BTreeMap<Vec<u8>, Vec<u8>> {
	fn insert(&mut self, key: Vec<u8>, value: Vec<u8>) {
		self.insert(key, value);
	}
	fn get(&self, key: Vec<u8>) -> &Vec<u8> {
		self.get(key.as_slice()).unwrap()
	}
	fn remove(&mut self, key: Vec<u8>) {
		self.remove(&key);
	}
}

impl Map for Tree {
	fn insert(&mut self, key: Vec<u8>, value: Vec<u8>) {
		self.insert(key.as_slice(), value);
	}
	fn get(&self, key: Vec<u8>) -> &Vec<u8> {
		self.get(&key).unwrap()
	}
	fn remove(&mut self, key: Vec<u8>) {
		self.remove(&key);
	}
}

fn do_inserts<M: Map>(mut map: M, to_insert: Vec<(Vec<u8>, Vec<u8>)>) {
	for (k, v) in to_insert {
		map.insert(k, v);
	}
	black_box(map);
}

fn do_gets<M: Map>(map: M, to_get: Vec<Vec<u8>>) {
	for k in to_get {
		let v = map.get(k);
		black_box(v);
	}
	black_box(map);
}

fn do_removes<M: Map>(mut map: M, to_remove: Vec<Vec<u8>>) {
	for k in to_remove {
		map.remove(k);
	}
	black_box(map);
}

fn fill<M: Map>(
	map: &mut M,
	key_size: usize,
	total: usize,
	returned: usize,
	seed: u64,
) -> Vec<Vec<u8>> {
	let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
	let mut result = Vec::with_capacity(returned);
	for _ in 0..total {
		let key = (0..key_size).map(|_| rng.gen()).collect::<Vec<u8>>();
		if result.len() < returned {
			result.push(key.clone());
		}
		map.insert(key.clone(), key);
	}
	result
}
fn gen_to_insert(key_size: usize, n: usize, seed: u64) -> Vec<(Vec<u8>, Vec<u8>)> {
	let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
	(0..n)
		.map(|_| {
			(
				(0..key_size).map(|_| rng.gen()).collect::<Vec<u8>>(),
				(0..key_size).map(|_| rng.gen()).collect::<Vec<u8>>(),
			)
		})
		.collect()
}

fn criterion_benchmark(c: &mut Criterion) {
	let seed = 5u64;
	let seed_insert = 8u64;
	for &filled in &[1_000, 10_000, 100_000] {
		//let key_size = 1;
		//let key_size = 2;
		let key_size = 32;
		let returned_size = 100;

		let name = format!("RADIX GET filled={}", filled);
		c.bench_function(&name, |b| {
			b.iter_batched(
				|| {
					let mut map = Tree::new(());
					let returned = fill(&mut map, key_size, filled, returned_size, seed);
					(map.clone(), returned.clone())
				},
				|(map, returned)| do_gets(black_box(map), black_box(returned)),
				criterion::BatchSize::LargeInput,
			);
		});
		/*
				let name = format!("HASHMAP GET filled={}", filled);
				c.bench_function(&name, |b| {
					b.iter_batched(
						|| {
							let mut map = HashMap::new();
							let returned = fill(&mut map, key_size, filled, returned_size, seed);
							(map.clone(), returned.clone())
						},
						|(map, returned)| do_gets(black_box(map), black_box(returned)),
						criterion::BatchSize::LargeInput,
					);
				});

				let name = format!("BTREEMAP GET filled={}", filled);
				c.bench_function(&name, |b| {
					b.iter_batched(
						|| {
							let mut map = BTreeMap::new();
							let returned = fill(&mut map, key_size, filled, returned_size, seed);
							(map.clone(), returned.clone())
						},
						|(map, returned)| do_gets(black_box(map), black_box(returned)),
						criterion::BatchSize::LargeInput,
					);
				});
		*/
		let name = format!("RADIX REMOVE filled={}", filled);
		c.bench_function(&name, |b| {
			b.iter_batched(
				|| {
					let mut map = Tree::new(());
					let returned = fill(&mut map, key_size, filled, returned_size, seed);
					(map.clone(), returned.clone())
				},
				|(map, returned)| do_removes(black_box(map), black_box(returned)),
				criterion::BatchSize::LargeInput,
			);
		});
		/*
				let name = format!("HASHMAP REMOVE filled={}", filled);
				c.bench_function(&name, |b| {
					b.iter_batched(
						|| {
							let mut map = HashMap::new();
							let returned = fill(&mut map, key_size, filled, returned_size, seed);
							(map.clone(), returned.clone())
						},
						|(map, returned)| do_removes(black_box(map), black_box(returned)),
						criterion::BatchSize::LargeInput,
					);
				});

				let name = format!("BTREEMAP REMOVE filled={}", filled);
				c.bench_function(&name, |b| {
					b.iter_batched(
						|| {
							let mut map = BTreeMap::new();
							let returned = fill(&mut map, key_size, filled, returned_size, seed);
							(map.clone(), returned.clone())
						},
						|(map, returned)| do_removes(black_box(map), black_box(returned)),
						criterion::BatchSize::LargeInput,
					);
				});
		*/
		let name = format!("RADIX INSERT filled={}", filled);
		c.bench_function(&name, |b| {
			b.iter_batched(
				|| {
					let mut map = Tree::new(());
					let _ = fill(&mut map, key_size, filled, 0, seed);
					let to_insert = gen_to_insert(key_size, returned_size, seed_insert);
					(map.clone(), to_insert.clone())
				},
				|(map, to_insert)| do_inserts(black_box(map), black_box(to_insert)),
				criterion::BatchSize::LargeInput,
			);
		});
		/*
				let name = format!("HASHMAP INSERT filled={}", filled);
				c.bench_function(&name, |b| {
					b.iter_batched(
						|| {
							let mut map = HashMap::new();
							let _ = fill(&mut map, key_size, filled, 0, seed);
							let to_insert = gen_to_insert(key_size, returned_size, seed_insert);
							(map.clone(), to_insert.clone())
						},
						|(map, to_insert)| do_inserts(black_box(map), black_box(to_insert)),
						criterion::BatchSize::LargeInput,
					);
				});

				let name = format!("BTREEMAP INSERT filled={}", filled);
				c.bench_function(&name, |b| {
					b.iter_batched(
						|| {
							let mut map = BTreeMap::new();
							let _ = fill(&mut map, key_size, filled, 0, seed);
							let to_insert = gen_to_insert(key_size, returned_size, seed_insert);
							(map.clone(), to_insert.clone())
						},
						|(map, to_insert)| do_inserts(black_box(map), black_box(to_insert)),
						criterion::BatchSize::LargeInput,
					);
				});
		*/
	}
}

criterion_group!(
	name = benches;
	config = Criterion::default()
	.measurement_time(core::time::Duration::from_secs(10))
	.sample_size(30);
	targets = criterion_benchmark
);
criterion_main!(benches);
