// This file is part of Substrate.

// Copyright (C) 2019-2021 Parity Technologies (UK) Ltd.
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

//! Database upgrade logic.

use log::warn;
use sp_runtime::traits::{Block as BlockT, HashFor};
use std::{
	fmt, fs,
	io::{self, ErrorKind, Read, Write},
	marker::PhantomData,
	path::{Path, PathBuf},
	sync::Arc,
};

use crate::{columns, utils::DatabaseType};
use codec::{Decode, Encode};
use kvdb::KeyValueDB;
use trie_db::{TrieDBNodeIterator, node::{NodePlan, ValuePlan}};
use kvdb_rocksdb::{Database, DatabaseConfig};
use sp_core::storage::{ChildInfo, ChildType, PrefixedStorageKey};
use sp_trie::KeySpacedDB;

/// Version file name.
const VERSION_FILE_NAME: &'static str = "db_version";

/// Current db version.
const CURRENT_VERSION: u32 = 3;

/// Number of columns in v1.
const V1_NUM_COLUMNS: u32 = 11;
const V2_NUM_COLUMNS: u32 = 12;

/// Database upgrade errors.
#[derive(Debug)]
pub enum UpgradeError {
	/// Database version cannot be read from existing db_version file.
	UnknownDatabaseVersion,
	/// Missing database version file.
	MissingDatabaseVersionFile,
	/// Database version no longer supported.
	UnsupportedVersion(u32),
	/// Database version comes from future version of the client.
	FutureDatabaseVersion(u32),
	/// Invalid justification block.
	DecodingJustificationBlock,
	/// Common io error.
	Io(io::Error),
}

pub type UpgradeResult<T> = Result<T, UpgradeError>;

impl From<io::Error> for UpgradeError {
	fn from(err: io::Error) -> Self {
		UpgradeError::Io(err)
	}
}

impl fmt::Display for UpgradeError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			UpgradeError::UnknownDatabaseVersion => {
				write!(f, "Database version cannot be read from exisiting db_version file")
			},
			UpgradeError::MissingDatabaseVersionFile => write!(f, "Missing database version file"),
			UpgradeError::UnsupportedVersion(version) => {
				write!(f, "Database version no longer supported: {}", version)
			},
			UpgradeError::FutureDatabaseVersion(version) => {
				write!(f, "Database version comes from future version of the client: {}", version)
			},
			UpgradeError::DecodingJustificationBlock => {
				write!(f, "Decodoning justification block failed")
			},
			UpgradeError::Io(err) => write!(f, "Io error: {}", err),
		}
	}
}

/// Upgrade database to current version.
pub fn upgrade_db<Block: BlockT>(db_path: &Path, db_type: DatabaseType) -> UpgradeResult<()> {
	let db_version = current_version(db_path)?;
	match db_version {
		0 => return Err(UpgradeError::UnsupportedVersion(db_version)),
		1 => {
			migrate_1_to_2::<Block>(db_path, db_type)?;
			migrate_2_to_3::<Block>(db_path, db_type)?
		},
		2 => migrate_2_to_3::<Block>(db_path, db_type)?,
		CURRENT_VERSION => (),
		42 => {
			check_migration::<Block>(db_path, db_type).map_err(|e| {
				warn!("Simulate migration error {:?}", e);
			}).unwrap();
		},
		_ => return Err(UpgradeError::FutureDatabaseVersion(db_version)),
	}
	update_version(db_path)?;
	Ok(())
}

/// Migration from version1 to version2:
/// 1) the number of columns has changed from 11 to 12;
/// 2) transactions column is added;
fn migrate_1_to_2<Block: BlockT>(db_path: &Path, _db_type: DatabaseType) -> UpgradeResult<()> {
	let db_cfg = DatabaseConfig::with_columns(V1_NUM_COLUMNS);
	let db = Database::open(&db_cfg, db_path)?;
	db.add_column().map_err(Into::into)
}

/// Migration from version2 to version3:
/// - The format of the stored Justification changed to support multiple Justifications.
fn migrate_2_to_3<Block: BlockT>(db_path: &Path, _db_type: DatabaseType) -> UpgradeResult<()> {
	let db_cfg = DatabaseConfig::with_columns(V2_NUM_COLUMNS);
	let db = Database::open(&db_cfg, db_path)?;

	// Get all the keys we need to update
	let keys: Vec<_> = db.iter(columns::JUSTIFICATIONS).map(|entry| entry.0).collect();

	// Read and update each entry
	let mut transaction = db.transaction();
	for key in keys {
		if let Some(justification) = db.get(columns::JUSTIFICATIONS, &key)? {
			// Tag each justification with the hardcoded ID for GRANDPA to avoid the dependency on
			// the GRANDPA crate.
			// NOTE: when storing justifications the previous API would get a `Vec<u8>` and still
			// call encode on it.
			let justification = Vec::<u8>::decode(&mut &justification[..])
				.map_err(|_| UpgradeError::DecodingJustificationBlock)?;
			let justifications = sp_runtime::Justifications::from((*b"FRNK", justification));
			transaction.put_vec(columns::JUSTIFICATIONS, &key, justifications.encode());
		}
	}
	db.write(transaction)?;

	Ok(())
}

/// Reads current database version from the file at given path.
/// If the file does not exist returns 0.
fn current_version(path: &Path) -> UpgradeResult<u32> {
	match fs::File::open(version_file_path(path)) {
		Err(ref err) if err.kind() == ErrorKind::NotFound =>
			Err(UpgradeError::MissingDatabaseVersionFile),
		Err(_) => Err(UpgradeError::UnknownDatabaseVersion),
		Ok(mut file) => {
			let mut s = String::new();
			file.read_to_string(&mut s).map_err(|_| UpgradeError::UnknownDatabaseVersion)?;
			u32::from_str_radix(&s, 10).map_err(|_| UpgradeError::UnknownDatabaseVersion)
		},
	}
}

/// Writes current database version to the file.
/// Creates a new file if the version file does not exist yet.
pub fn update_version(path: &Path) -> io::Result<()> {
	fs::create_dir_all(path)?;
	let mut file = fs::File::create(version_file_path(path))?;
	file.write_all(format!("{}", CURRENT_VERSION).as_bytes())?;
	Ok(())
}

/// Returns the version file path.
fn version_file_path(path: &Path) -> PathBuf {
	let mut file_path = path.to_owned();
	file_path.push(VERSION_FILE_NAME);
	file_path
}

fn check_migration<Block: BlockT>(
	db_path: &Path,
	_db_type: DatabaseType,
) -> sp_blockchain::Result<()> {
	let db_config = kvdb_rocksdb::DatabaseConfig::with_columns(crate::utils::NUM_COLUMNS);
	/*   {
		let option = rocksdb::Options::default();
		 let cfs = rocksdb::DB::list_cf(&option, db_path).unwrap();
		 let db = rocksdb::DB::open_cf(&option, db_path, cfs.clone()).unwrap();
		 for cf in cfs {

			 if let Some(col) = db.cf_handle(&cf) {
				println!("{:?}, {:?}", cf, db.property_int_value_cf(col, "rocksdb.estimate-table-readers-mem"));
				println!("{:?}, {:?}", cf, db.property_int_value_cf(col, "rocksdb.size-all-mem-tables"));
				println!("{:?}, {:?}", cf, db.property_int_value_cf(col, "rocksdb.cur-size-all-mem-tables"));
			 }
		 }
	}*/
	let path = db_path
		.to_str()
		.ok_or_else(|| sp_blockchain::Error::Backend("Invalid database path".into()))?;
	let db = kvdb_rocksdb::Database::open(&db_config, &path)
		.map_err(|err| sp_blockchain::Error::Backend(format!("{}", err)))?;

	let (tree_root, _block_hash) =
		match db.get(crate::utils::COLUMN_META, crate::meta_keys::FINALIZED_BLOCK) {
			Ok(id) => {
				let id = id.unwrap();
				let id = db
					.get(crate::columns::HEADER, &id)
					.expect("s")
					.map(|b| Block::Header::decode(&mut &b[..]).ok());
				use sp_runtime::traits::Header;
				let id = id.unwrap().expect("d");
				warn!("Head is {:?}", id);
				(id.state_root().clone(), id.hash().clone())
			}
			Err(e) => panic!("no best block is bad sign {:?}", e),
		};

	let db = Arc::new(db);
	let threshold: u32 = 33;
	let mut nb_to_migrate = 0;
	let mut nb_to_migrate_child = 0;

	let storage = StorageDb::<Block>(db, PhantomData);
	let trie = sp_trie::trie_types::TrieDB::new(&storage, &tree_root).expect("build trie");
	let iter_node = TrieDBNodeIterator::new(&trie).unwrap();
	for node in iter_node {

//	type Item = Result<(NibbleVec, Option<TrieHash<L>>, Rc<OwnedNode<DBValue>>), TrieHash<L>, CError<L>>;
		match node.unwrap().2.node_plan() {
			NodePlan::Leaf { value, .. } | NodePlan::NibbledBranch { value: Some(value), .. } => {
				if let ValuePlan::Inline(range) = value {
					if (range.end - range.start) as u32 >= threshold {
						nb_to_migrate += 1;
					}
				}
			 },
			 _ => (),
		}
	}

	use trie_db::Trie;
	let mut child_roots: Vec<(ChildInfo, Vec<u8>)> = Vec::new();
	// get all child trie roots
	for key_value in trie.iter().unwrap() {
		let (key, value) = key_value.unwrap();
		if key[..].starts_with(sp_core::storage::well_known_keys::DEFAULT_CHILD_STORAGE_KEY_PREFIX) {
			let prefixed_key = PrefixedStorageKey::new(key);
			let (_type, unprefixed) = ChildType::from_prefixed_key(&prefixed_key).unwrap();
			child_roots.push((ChildInfo::new_default(unprefixed), value));
		}
	}
	for (child_info, root) in child_roots {
		let mut child_root = <HashFor<Block> as hash_db::Hasher>::Out::default();
		let storage = KeySpacedDB::new(&storage, child_info.keyspace());
	
		child_root.as_mut()[..].copy_from_slice(&root[..]);
		let trie = sp_trie::trie_types::TrieDB::new(&storage, &child_root).expect("build child trie");
		let iter_node = TrieDBNodeIterator::new(&trie).unwrap();
		for node in iter_node {
			match node.unwrap().2.node_plan() {
				NodePlan::Leaf { value, .. } | NodePlan::NibbledBranch { value: Some(value), .. } => {
					if let ValuePlan::Inline(range) = value {
						if (range.end - range.start) as u32 >= threshold {
							nb_to_migrate_child += 1;
						}
					}
				 },
				 _ => (),
			}
		}


	}

	warn!("Finished check state at block {:?}, nb to migrate {:?}, nb child to migrate {:?}", nb_to_migrate, nb_to_migrate, nb_to_migrate_child);
	Ok(())
}

struct ArcKVDB<D: KeyValueDB>(Arc<D>);

impl<D: KeyValueDB> parity_util_mem::MallocSizeOf for ArcKVDB<D> {
	fn size_of(&self, ops: &mut parity_util_mem::MallocSizeOfOps) -> usize {
		self.0.size_of(ops)
	}
}

impl<D: KeyValueDB> KeyValueDB for ArcKVDB<D> {
	fn transaction(&self) -> kvdb::DBTransaction {
		self.0.transaction()
	}

	fn get(&self, col: u32, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
		self.0.get(col, key)
	}

	fn get_by_prefix(&self, col: u32, prefix: &[u8]) -> Option<Box<[u8]>> {
		self.0.get_by_prefix(col, prefix)
	}

	fn write(&self, transaction: kvdb::DBTransaction) -> io::Result<()> {
		self.0.write(transaction)
	}

	fn iter<'a>(&'a self, col: u32) -> Box<dyn Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a> {
		self.0.iter(col)
	}

	fn iter_with_prefix<'a>(
		&'a self,
		col: u32,
		prefix: &'a [u8],
	) -> Box<dyn Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a> {
		self.0.iter_with_prefix(col, prefix)
	}

	fn restore(&self, new_db: &str) -> io::Result<()> {
		self.0.restore(new_db)
	}
}

struct StorageDb<Block>(Arc<kvdb_rocksdb::Database>, PhantomData<Block>);

impl<Block: BlockT> hash_db::HashDBRef<HashFor<Block>, Vec<u8>> for StorageDb<Block> {
	fn contains(
		&self,
		key: &<HashFor<Block> as hash_db::Hasher>::Out,
		prefix: hash_db::Prefix,
	) -> bool {
		self.get(key, prefix).is_some()
	}

	fn get(
		&self,
		key: &<HashFor<Block> as hash_db::Hasher>::Out,
		prefix: hash_db::Prefix,
	) -> Option<sp_trie::DBValue> {
		let key = sp_trie::prefixed_key::<HashFor<Block>>(key, prefix);
		self.0.get(crate::columns::STATE, key.as_slice()).expect("bad script")
	}
}

impl<Block: BlockT> sp_state_machine::Storage<HashFor<Block>> for StorageDb<Block> {
	fn get(
		&self,
		key: &Block::Hash,
		prefix: hash_db::Prefix,
	) -> Result<Option<sp_trie::DBValue>, String> {
		let key = sp_trie::prefixed_key::<HashFor<Block>>(key, prefix);
		Ok(self.0.get(crate::columns::STATE_META, key.as_slice()).expect("bad script"))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		tests::Block, DatabaseSettings, DatabaseSource, KeepBlocks, TransactionStorageMode,
	};
	use sc_state_db::PruningMode;

	fn create_db(db_path: &Path, version: Option<u32>) {
		if let Some(version) = version {
			fs::create_dir_all(db_path).unwrap();
			let mut file = fs::File::create(version_file_path(db_path)).unwrap();
			file.write_all(format!("{}", version).as_bytes()).unwrap();
		}
	}

	fn open_database(db_path: &Path, db_type: DatabaseType) -> sp_blockchain::Result<()> {
		crate::utils::open_database::<Block>(
			&DatabaseSettings {
				state_cache_size: 0,
				state_cache_child_ratio: None,
				state_pruning: PruningMode::ArchiveAll,
				source: DatabaseSource::RocksDb { path: db_path.to_owned(), cache_size: 128 },
				keep_blocks: KeepBlocks::All,
				transaction_storage: TransactionStorageMode::BlockBody,
			},
			db_type,
		)
		.map(|_| ())
	}

	#[test]
	fn downgrade_never_happens() {
		let db_dir = tempfile::TempDir::new().unwrap();
		create_db(db_dir.path(), Some(CURRENT_VERSION + 1));
		assert!(open_database(db_dir.path(), DatabaseType::Full).is_err());
	}

	#[test]
	fn open_empty_database_works() {
		let db_type = DatabaseType::Full;
		let db_dir = tempfile::TempDir::new().unwrap();
		let db_dir = db_dir.path().join(db_type.as_str());
		open_database(&db_dir, db_type).unwrap();
		open_database(&db_dir, db_type).unwrap();
		assert_eq!(current_version(&db_dir).unwrap(), CURRENT_VERSION);
	}

	#[test]
	fn upgrade_to_3_works() {
		let db_type = DatabaseType::Full;
		for version_from_file in &[None, Some(1), Some(2)] {
			let db_dir = tempfile::TempDir::new().unwrap();
			let db_path = db_dir.path().join(db_type.as_str());
			create_db(&db_path, *version_from_file);
			open_database(&db_path, db_type).unwrap();
			assert_eq!(current_version(&db_path).unwrap(), CURRENT_VERSION);
		}
	}
}
