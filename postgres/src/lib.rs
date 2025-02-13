//! Deadpool simple async pool for PostgreSQL connections.
//!
//! This crate implements a [`deadpool`](https://crates.io/crates/deadpool)
//! manager for [`tokio-postgres`](https://crates.io/crates/tokio-postgres)
//! and also provides a `statement` cache by wrapping `tokio_postgres::Client`
//! and `tokio_postgres::Transaction`.
//!
//! You should not need to use `deadpool` directly. Use the `Pool` type
//! provided by this crate instead.
//!
//! # Example
//!
//! ```rust
//! use std::env;
//!
//! use deadpool_postgres::{Manager, Pool};
//! use tokio_postgres::{Config, NoTls};
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut cfg = Config::new();
//!     cfg.host("/var/run/postgresql");
//!     cfg.user(env::var("USER").unwrap().as_str());
//!     cfg.dbname("deadpool");
//!     let mgr = Manager::new(cfg, tokio_postgres::NoTls);
//!     let pool = Pool::new(mgr, 16);
//!     for i in 1..10 {
//!         let mut client = pool.get().await.unwrap();
//!         let stmt = client.prepare("SELECT 1 + $1").await.unwrap();
//!         let rows = client.query(&stmt, &[&i]).await.unwrap();
//!         let value: i32 = rows[0].get(0);
//!         assert_eq!(value, i + 1);
//!     }
//! }
//! ```
#![warn(missing_docs)]

use std::collections::HashMap;
use std::ops::Deref;

use async_trait::async_trait;
use futures::FutureExt;
use log::{info, warn};
use tokio::spawn;
use tokio_postgres::{
    tls::MakeTlsConnect, tls::TlsConnect, Client as PgClient, Config as PgConfig, Error, Socket,
    Statement, Transaction as PgTransaction,
};

/// A type alias for using `deadpool::Pool` with `tokio_postgres`
pub type Pool = deadpool::Pool<Client, tokio_postgres::Error>;

/// The manager for creating and recyling postgresql connections
pub struct Manager<T: MakeTlsConnect<Socket>> {
    config: PgConfig,
    tls: T,
}

impl<T: MakeTlsConnect<Socket>> Manager<T> {
    /// Create manager using `PgConfig` and a `TlsConnector`
    pub fn new(config: PgConfig, tls: T) -> Manager<T> {
        Manager {
            config: config,
            tls: tls,
        }
    }
}

#[async_trait]
impl<T> deadpool::Manager<Client, Error> for Manager<T>
where
    T: MakeTlsConnect<Socket> + Clone + Sync + Send + 'static,
    T::Stream: Sync + Send,
    T::TlsConnect: Sync + Send,
    <T::TlsConnect as TlsConnect<Socket>>::Future: Send,
{
    async fn create(&self) -> Result<Client, Error> {
        let (client, connection) = self.config.connect(self.tls.clone()).await?;
        let connection = connection.map(|r| {
            if let Err(e) = r {
                warn!(target: "deadpool.postgres", "Connection error: {}", e);
            }
        });
        spawn(connection);
        Ok(Client::new(client))
    }
    async fn recycle(&self, client: &mut Client) -> Result<(), Error> {
        match client.simple_query("").await {
            Ok(_) => Ok(()),
            Err(e) => {
                info!(target: "deadpool.postgres", "Connection could not be recycled: {}", e);
                Err(e)
            }
        }
    }
}

/// This structure holds the cached statements and provides access to
/// functions for retrieving the current size and clearing the cache.
pub struct StatementCache {
    map: HashMap<String, Statement>,
}

impl StatementCache {
    fn new() -> StatementCache {
        StatementCache {
            map: HashMap::new()
        }
    }
    /// Retrieve current size of the cache
    pub fn size(&self) -> usize {
        self.map.len()
    }
    /// Clear cache
    pub fn clear(&mut self) {
        self.map.clear()
    }
}

/// A wrapper for `tokio_postgres::Client` which includes a statement cache.
pub struct Client {
    client: PgClient,
    /// The statement cache
    pub statement_cache: StatementCache,
}

impl Client {
    /// Create new wrapper instance using an existing `tokio_postgres::Client`
    pub fn new(client: PgClient) -> Client {
        Client {
            client: client,
            statement_cache: StatementCache::new(),
        }
    }
    /// Creates a new prepared statement using the statement cache if possible.
    ///
    /// See [`tokio_postgres::Client::prepare`](#method.prepare-1)
    pub async fn prepare(&mut self, query: &str) -> Result<Statement, Error> {
        let query_owned = query.to_owned();
        match self.statement_cache.map.get(&query_owned) {
            Some(statement) => Ok(statement.clone()),
            None => {
                let stmt = self.client.prepare(query).await?;
                self.statement_cache.map
                    .insert(query_owned.clone(), stmt.clone());
                Ok(stmt)
            }
        }
    }
    /// Begins a new database transaction which supports the statement cache.
    ///
    /// See [`tokio_postgres::Client::transaction`](#method.transaction-1)
    pub async fn transaction<'a>(&'a mut self) -> Result<Transaction<'a>, Error> {
        Ok(Transaction {
            txn: PgClient::transaction(&mut self.client).await?,
            statement_cache: &mut self.statement_cache,
        })
    }
}

impl Deref for Client {
    type Target = PgClient;
    fn deref(&self) -> &PgClient {
        &self.client
    }
}

/// A wrapper for `tokio_postgres::Transaction` which uses the statement cache
/// from the client object it was created by.
pub struct Transaction<'a> {
    txn: PgTransaction<'a>,
    /// The statement cache
    pub statement_cache: &'a mut StatementCache,
}

impl<'a> Transaction<'a> {
    /// Creates a new prepared statement using the statement cache if possible.
    ///
    /// See [`tokio_postgres::Transaction::prepare`](#method.prepare-1)
    pub async fn prepare(&mut self, query: &str) -> Result<Statement, Error> {
        let query_owned = query.to_owned();
        match self.statement_cache.map.get(&query_owned) {
            Some(statement) => Ok(statement.clone()),
            None => {
                let stmt = self.txn.prepare(query).await?;
                self.statement_cache.map
                    .insert(query_owned.clone(), stmt.clone());
                Ok(stmt)
            }
        }
    }
    /// Like `tokio_postgres::Transaction::commit`
    pub async fn commit(self) -> Result<(), Error> {
        self.txn.commit().await
    }
    /// Like `tokio_postgres::Transaction::rollback`
    pub async fn rollback(self) -> Result<(), Error> {
        self.txn.rollback().await
    }
}

impl<'a> Deref for Transaction<'a> {
    type Target = PgTransaction<'a>;
    fn deref(&self) -> &PgTransaction<'a> {
        &self.txn
    }
}
