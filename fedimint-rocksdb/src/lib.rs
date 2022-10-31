use std::path::Path;

use anyhow::Result;
use fedimint_api::db::{DatabaseTransaction, PrefixIter};
use fedimint_api::db::{IDatabase, IDatabaseTransaction};
pub use rocksdb;
use rocksdb::{OptimisticTransactionDB, OptimisticTransactionOptions, WriteOptions};
use tracing::warn;

#[derive(Debug)]
pub struct RocksDb(rocksdb::OptimisticTransactionDB);

pub struct RocksDbTransaction<'a>(rocksdb::Transaction<'a, rocksdb::OptimisticTransactionDB>);

impl RocksDb {
    pub fn open(db_path: impl AsRef<Path>) -> Result<RocksDb, rocksdb::Error> {
        let db: rocksdb::OptimisticTransactionDB =
            rocksdb::OptimisticTransactionDB::<rocksdb::SingleThreaded>::open_default(&db_path)?;
        Ok(RocksDb(db))
    }

    pub fn inner(&self) -> &rocksdb::OptimisticTransactionDB {
        &self.0
    }
}

impl From<rocksdb::OptimisticTransactionDB> for RocksDb {
    fn from(db: OptimisticTransactionDB) -> Self {
        RocksDb(db)
    }
}

impl From<RocksDb> for rocksdb::OptimisticTransactionDB {
    fn from(db: RocksDb) -> Self {
        db.0
    }
}

impl IDatabase for RocksDb {
    fn begin_transaction(&self) -> DatabaseTransaction {
        let mut optimistic_options = OptimisticTransactionOptions::default();
        optimistic_options.set_snapshot(true);
        let mut tx: DatabaseTransaction = RocksDbTransaction(
            self.0
                .transaction_opt(&WriteOptions::default(), &optimistic_options),
        )
        .into();
        tx.set_tx_savepoint();
        tx
    }
}

impl<'a> IDatabaseTransaction<'a> for RocksDbTransaction<'a> {
    fn raw_insert_bytes(&mut self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>> {
        let val = self.0.get(key).unwrap();
        self.0.put(key, value)?;
        Ok(val)
    }

    fn raw_get_bytes(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(key)?)
    }

    fn raw_remove_entry(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let val = self.0.get(key).unwrap();
        self.0.delete(key)?;
        Ok(val)
    }

    fn raw_find_by_prefix(&self, key_prefix: &[u8]) -> PrefixIter<'_> {
        let prefix = key_prefix.to_vec();
        Box::new(
            self.0
                .prefix_iterator(prefix.clone())
                .map_while(move |res| {
                    let (key_bytes, value_bytes) = res.expect("DB error");
                    key_bytes
                        .starts_with(&prefix)
                        .then_some((key_bytes, value_bytes))
                })
                .map(|(key_bytes, value_bytes)| (key_bytes.to_vec(), value_bytes.to_vec()))
                .map(Ok),
        )
    }

    fn commit_tx(self: Box<Self>) -> Result<()> {
        self.0.commit()?;
        Ok(())
    }

    fn rollback_tx_to_savepoint(&mut self) {
        match self.0.rollback_to_savepoint() {
            Ok(()) => {}
            _ => {
                warn!("Rolling back database transaction without a set savepoint");
            }
        }
    }

    fn set_tx_savepoint(&mut self) {
        self.0.set_savepoint();
    }
}

#[cfg(test)]
mod tests {
    use crate::RocksDb;

    #[test_log::test]
    fn test_basic_dbtx_rw() {
        let path = tempfile::Builder::new()
            .prefix("fcb-rocksdb-test")
            .tempdir()
            .unwrap();

        let db = RocksDb::open(path).unwrap();
        fedimint_api::db::test_dbtx_impl(db.into());
    }
}
