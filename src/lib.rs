#![warn(rust_2018_idioms)]
#![warn(missing_docs)]

//! The salsa crate is a crate for incremental recomputation.  It
//! permits you to define a "database" of queries with both inputs and
//! values derived from those inputs; as you set the inputs, you can
//! re-execute the derived queries and it will try to re-use results
//! from previous invocations as appropriate.

mod blocking_future;
mod derived;
mod doctest;
mod durability;
mod input;
mod intern_id;
mod interned;
mod lru;
mod revision;
mod runtime;
mod storage;

pub mod debug;
/// Items in this module are public for implementation reasons,
/// and are exempt from the SemVer guarantees.
#[doc(hidden)]
pub mod plumbing;

use crate::plumbing::DerivedQueryStorageOps;
use crate::plumbing::InputQueryStorageOps;
use crate::plumbing::LruQueryStorageOps;
use crate::plumbing::QueryStorageMassOps;
use crate::plumbing::QueryStorageOps;
#[cfg(feature = "async")]
use crate::plumbing::{AsyncQueryFunction, QueryStorageOpsAsync};
use crate::plumbing::{HasQueryGroup, QueryStorageOpsSync};
pub use crate::revision::Revision;
use std::fmt::{self, Debug};
use std::hash::Hash;
use std::{
    marker::PhantomData,
    sync::{Arc, Mutex},
};

pub use crate::durability::Durability;
pub use crate::intern_id::InternId;
pub use crate::interned::InternKey;
pub use crate::runtime::Runtime;
pub use crate::runtime::RuntimeId;
pub use crate::storage::Storage;

/// The base trait which your "query context" must implement. Gives
/// access to the salsa runtime, which you must embed into your query
/// context (along with whatever other state you may require).
pub trait Database: plumbing::DatabaseOps {
    /// Iterates through all query storage and removes any values that
    /// have not been used since the last revision was created. The
    /// intended use-cycle is that you first execute all of your
    /// "main" queries; this will ensure that all query values they
    /// consume are marked as used.  You then invoke this method to
    /// remove other values that were not needed for your main query
    /// results.
    fn sweep_all(&self, strategy: SweepStrategy) {
        // Note that we do not acquire the query lock (or any locks)
        // here.  Each table is capable of sweeping itself atomically
        // and there is no need to bring things to a halt. That said,
        // users may wish to guarantee atomicity.

        let runtime = self.salsa_runtime();
        self.for_each_query(&mut |query_storage| query_storage.sweep(runtime, strategy));
    }

    /// This function is invoked at key points in the salsa
    /// runtime. It permits the database to be customized and to
    /// inject logging or other custom behavior.
    fn salsa_event(&self, event_fn: Event) {
        #![allow(unused_variables)]
    }

    /// This function is invoked when a dependent query is being computed by the
    /// other thread, and that thread panics.
    fn on_propagated_panic(&self) -> ! {
        panic!("concurrent salsa query panicked")
    }

    /// Gives access to the underlying salsa runtime.
    fn salsa_runtime(&self) -> &Runtime {
        self.ops_salsa_runtime()
    }

    /// Gives access to the underlying salsa runtime.
    fn salsa_runtime_mut(&mut self) -> &mut Runtime {
        self.ops_salsa_runtime_mut()
    }
}

/// The `Event` struct identifies various notable things that can
/// occur during salsa execution. Instances of this struct are given
/// to `salsa_event`.
pub struct Event {
    /// The id of the snapshot that triggered the event.  Usually
    /// 1-to-1 with a thread, as well.
    pub runtime_id: RuntimeId,

    /// What sort of event was it.
    pub kind: EventKind,
}

impl fmt::Debug for Event {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Event")
            .field("runtime_id", &self.runtime_id)
            .field("kind", &self.kind)
            .finish()
    }
}

/// An enum identifying the various kinds of events that can occur.
pub enum EventKind {
    /// Occurs when we found that all inputs to a memoized value are
    /// up-to-date and hence the value can be re-used without
    /// executing the closure.
    ///
    /// Executes before the "re-used" value is returned.
    DidValidateMemoizedValue {
        /// The database-key for the affected value. Implements `Debug`.
        database_key: DatabaseKeyIndex,
    },

    /// Indicates that another thread (with id `other_runtime_id`) is processing the
    /// given query (`database_key`), so we will block until they
    /// finish.
    ///
    /// Executes after we have registered with the other thread but
    /// before they have answered us.
    ///
    /// (NB: you can find the `id` of the current thread via the
    /// `salsa_runtime`)
    WillBlockOn {
        /// The id of the runtime we will block on.
        other_runtime_id: RuntimeId,

        /// The database-key for the affected value. Implements `Debug`.
        database_key: DatabaseKeyIndex,
    },

    /// Indicates that the function for this query will be executed.
    /// This is either because it has never executed before or because
    /// its inputs may be out of date.
    WillExecute {
        /// The database-key for the affected value. Implements `Debug`.
        database_key: DatabaseKeyIndex,
    },
}

impl fmt::Debug for EventKind {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EventKind::DidValidateMemoizedValue { database_key } => fmt
                .debug_struct("DidValidateMemoizedValue")
                .field("database_key", database_key)
                .finish(),
            EventKind::WillBlockOn {
                other_runtime_id,
                database_key,
            } => fmt
                .debug_struct("WillBlockOn")
                .field("other_runtime_id", other_runtime_id)
                .field("database_key", database_key)
                .finish(),
            EventKind::WillExecute { database_key } => fmt
                .debug_struct("WillExecute")
                .field("database_key", database_key)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DiscardIf {
    Never,
    Outdated,
    Always,
}

impl Default for DiscardIf {
    fn default() -> DiscardIf {
        DiscardIf::Never
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DiscardWhat {
    Nothing,
    Values,
    Everything,
}

impl Default for DiscardWhat {
    fn default() -> DiscardWhat {
        DiscardWhat::Nothing
    }
}

/// The sweep strategy controls what data we will keep/discard when we
/// do a GC-sweep. The default (`SweepStrategy::default`) is a no-op,
/// use `SweepStrategy::discard_outdated` constructor or `discard_*`
/// and `sweep_*` builder functions to construct useful strategies.
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct SweepStrategy {
    discard_if: DiscardIf,
    discard_what: DiscardWhat,
    shrink_to_fit: bool,
}

impl SweepStrategy {
    /// Convenience function that discards all data not used thus far in the
    /// current revision.
    ///
    /// Equivalent to `SweepStrategy::default().discard_everything()`.
    pub fn discard_outdated() -> SweepStrategy {
        SweepStrategy::default()
            .discard_everything()
            .sweep_outdated()
    }

    /// Collects query values.
    ///
    /// Query dependencies are left in the database, which allows to quickly
    /// determine if the query is up to date, and avoid recomputing
    /// dependencies.
    pub fn discard_values(self) -> SweepStrategy {
        SweepStrategy {
            discard_what: self.discard_what.max(DiscardWhat::Values),
            ..self
        }
    }

    /// Collects both values and information about dependencies.
    ///
    /// Dependant queries will be recomputed even if all inputs to this query
    /// stay the same.
    pub fn discard_everything(self) -> SweepStrategy {
        SweepStrategy {
            discard_what: self.discard_what.max(DiscardWhat::Everything),
            ..self
        }
    }

    /// Process all keys, not verefied at the current revision.
    pub fn sweep_outdated(self) -> SweepStrategy {
        SweepStrategy {
            discard_if: self.discard_if.max(DiscardIf::Outdated),
            ..self
        }
    }

    /// Process all keys.
    pub fn sweep_all_revisions(self) -> SweepStrategy {
        SweepStrategy {
            discard_if: self.discard_if.max(DiscardIf::Always),
            ..self
        }
    }
}

/// Indicates a database that also supports parallel query
/// evaluation. All of Salsa's base query support is capable of
/// parallel execution, but for it to work, your query key/value types
/// must also be `Send`, as must any additional data in your database.
pub trait ParallelDatabase: Database + Send {
    /// Creates a second handle to the database that holds the
    /// database fixed at a particular revision. So long as this
    /// "frozen" handle exists, any attempt to [`set`] an input will
    /// block.
    ///
    /// [`set`]: struct.QueryTable.html#method.set
    ///
    /// This is the method you are meant to use most of the time in a
    /// parallel setting where modifications may arise asynchronously
    /// (e.g., a language server). In this context, it is common to
    /// wish to "fork off" a snapshot of the database performing some
    /// series of queries in parallel and arranging the results. Using
    /// this method for that purpose ensures that those queries will
    /// see a consistent view of the database (it is also advisable
    /// for those queries to use the [`is_current_revision_canceled`]
    /// method to check for cancellation).
    ///
    /// [`is_current_revision_canceled`]: struct.Runtime.html#method.is_current_revision_canceled
    ///
    /// # Panics
    ///
    /// It is not permitted to create a snapshot from inside of a
    /// query. Attepting to do so will panic.
    ///
    /// # Deadlock warning
    ///
    /// The intended pattern for snapshots is that, once created, they
    /// are sent to another thread and used from there. As such, the
    /// `snapshot` acquires a "read lock" on the database --
    /// therefore, so long as the `snapshot` is not dropped, any
    /// attempt to `set` a value in the database will block. If the
    /// `snapshot` is owned by the same thread that is attempting to
    /// `set`, this will cause a problem.
    ///
    /// # How to implement this
    ///
    /// Typically, this method will create a second copy of your
    /// database type (`MyDatabaseType`, in the example below),
    /// cloning over each of the fields from `self` into this new
    /// copy. For the field that stores the salsa runtime, you should
    /// use [the `Runtime::snapshot` method][rfm] to create a snapshot of the
    /// runtime. Finally, package up the result using `Snapshot::new`,
    /// which is a simple wrapper type that only gives `&self` access
    /// to the database within (thus preventing the use of methods
    /// that may mutate the inputs):
    ///
    /// [rfm]: struct.Runtime.html#method.snapshot
    ///
    /// ```rust,ignore
    /// impl ParallelDatabase for MyDatabaseType {
    ///     fn snapshot(&self) -> Snapshot<Self> {
    ///         Snapshot::new(
    ///             MyDatabaseType {
    ///                 runtime: self.runtime.snapshot(self),
    ///                 other_field: self.other_field.clone(),
    ///             }
    ///         )
    ///     }
    /// }
    /// ```
    fn snapshot(&self) -> Snapshot<Self>;

    /// Returns a `Snapshot` which can be used to run a query concurrently
    fn fork(&self, state: ForkState) -> Snapshot<Self>;

    /// Returns a [`Forker`] object which can be used to fork new `DB` references that are able to
    /// query the database concurrently. All queries run this way must complete before the
    /// [`Forker`] object goes out of scope or its `Drop` impl will panic.
    fn forker(&self) -> Forker<&Self> {
        forker(self)
    }

    /// Returns a [`Forker`] object which can be used to fork new `DB` references that are able to
    /// query the database concurrently. All queries run this way must complete before the
    /// [`Forker`] object goes out of scope or its `Drop` impl will panic.
    fn forker_mut(&mut self) -> Forker<&mut Self> {
        forker(self)
    }
}

/// TODO
pub fn forker<DB>(db: DB) -> Forker<DB>
where
    DB: std::ops::Deref,
    DB::Target: Database,
{
    let runtime = db.salsa_runtime();
    Forker {
        state: ForkState(Arc::new(ForkStateInner {
            parents: runtime
                .parent
                .iter()
                .flat_map(|state| state.0.parents.iter())
                .cloned()
                .chain(Some(runtime.id()))
                .collect(),
            cycle: Default::default(),
        })),
        db,
    }
}

/// Returned from calling [`ParallelDatabase::forker`]. Used to fork on a database so that
/// multiple queries can run concurrently
pub struct Forker<DB>
where
    DB: std::ops::Deref,
    DB::Target: Database,
{
    /// The database
    pub db: DB,
    /// The state used to tracked forked queries
    pub state: ForkState,
}

///
#[derive(Clone)]
pub struct ForkState(Arc<ForkStateInner>);

struct ForkStateInner {
    parents: Vec<RuntimeId>,
    cycle: Mutex<Vec<DatabaseKeyIndex>>,
}

impl<DB> Drop for Forker<DB>
where
    DB: std::ops::Deref,
    DB::Target: Database,
{
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let cycle = std::mem::replace(
                Arc::get_mut(&mut self.state.0)
                    .expect("Forker dropped before joining forked databases!")
                    .cycle
                    .get_mut()
                    .unwrap(),
                Vec::new(),
            );
            if !cycle.is_empty() {
                self.db.salsa_runtime().mark_cycle_participants(&cycle);
            }
        }
    }
}

impl<DB> Forker<DB>
where
    DB: std::ops::Deref,
    DB::Target: Sized + ParallelDatabase,
{
    /// Returns a `Snapshot` which can be used to run a query concurrently
    pub fn fork(&self) -> Snapshot<DB::Target> {
        self.db.fork(self.state.clone())
    }
}

/// Simple wrapper struct that takes ownership of a database `DB` and
/// only gives `&self` access to it. See [the `snapshot` method][fm]
/// for more details.
///
/// [fm]: trait.ParallelDatabase.html#method.snapshot
#[derive(Debug)]
pub struct Snapshot<DB: ?Sized>
where
    DB: ParallelDatabase,
{
    db: DB,
}

impl<DB> Snapshot<DB>
where
    DB: ParallelDatabase,
{
    /// Creates a `Snapshot` that wraps the given database handle
    /// `db`. From this point forward, only shared references to `db`
    /// will be possible.
    pub fn new(db: DB) -> Self {
        Snapshot { db }
    }

    #[doc(hidden)]
    pub fn __internal_get_db(&mut self) -> &mut DB {
        &mut self.db
    }
}

impl<DB> std::ops::Deref for Snapshot<DB>
where
    DB: ParallelDatabase,
{
    type Target = DB;

    fn deref(&self) -> &DB {
        &self.db
    }
}

/// An integer that uniquely identifies a particular query instance within the
/// database. Used to track dependencies between queries. Fully ordered and
/// equatable but those orderings are arbitrary, and meant to be used only for
/// inserting into maps and the like.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DatabaseKeyIndex {
    group_index: u16,
    query_index: u16,
    key_index: u32,
}

impl DatabaseKeyIndex {
    /// Returns the index of the query group containing this key.
    #[inline]
    pub fn group_index(self) -> u16 {
        self.group_index
    }

    /// Returns the index of the query within its query group.
    #[inline]
    pub fn query_index(self) -> u16 {
        self.query_index
    }

    /// Returns the index of this particular query key within the query.
    #[inline]
    pub fn key_index(self) -> u32 {
        self.key_index
    }

    /// Returns a type that gives a user-readable debug output.
    /// Use like `println!("{:?}", index.debug(db))`.
    pub fn debug<D: ?Sized>(self, db: &D) -> impl std::fmt::Debug + '_
    where
        D: plumbing::DatabaseOps,
    {
        DatabaseKeyIndexDebug { index: self, db }
    }
}

/// Helper type for `DatabaseKeyIndex::debug`
struct DatabaseKeyIndexDebug<'me, D: ?Sized>
where
    D: plumbing::DatabaseOps,
{
    index: DatabaseKeyIndex,
    db: &'me D,
}

impl<D: ?Sized> std::fmt::Debug for DatabaseKeyIndexDebug<'_, D>
where
    D: plumbing::DatabaseOps,
{
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.db.fmt_index(self.index, fmt)
    }
}

/// Internal trait for conversion to a mutable database. Necessary for maybe_changed_since so it
/// can select the async version when necessary
#[doc(hidden)]
pub trait AsAsyncDatabase<T: ?Sized> {
    #[doc(hidden)]
    fn as_async_db(&mut self) -> Option<&mut T>;
}

impl<T> AsAsyncDatabase<T> for &'_ T
where
    T: ?Sized + Database,
{
    fn as_async_db(&mut self) -> Option<&mut T> {
        None
    }
}

impl<T> AsAsyncDatabase<T> for OwnedDb<'_, T>
where
    T: ?Sized + Database,
{
    fn as_async_db(&mut self) -> Option<&mut T> {
        Some(self.db)
    }
}

/// Trait implements by all of the "special types" associated with
/// each of your queries.
///
/// Base trait of `Query` that has a lifetime parameter to allow the `DynDb` to be non-'static.
pub trait QueryDb<'d>: QueryBase {
    /// Dyn version of the associated trait for this query group.
    type DynDb: ?Sized + Database + HasQueryGroup<Self::Group> + 'd;

    /// Sized version of `DynDb`, &'d Self::DynDb for synchronous queries
    type Db: std::ops::Deref<Target = Self::DynDb> + AsAsyncDatabase<Self::DynDb>;
}

/// Trait implements by all of the "special types" associated with
/// each of your queries.
pub trait QueryBase: Debug + Default + Sized {
    /// Type that you you give as a parameter -- for queries with zero
    /// or more than one input, this will be a tuple.
    type Key: Clone + Debug + Hash + Eq;

    /// What value does the query return?
    type Value: Clone + Debug;

    /// Internal struct storing the values for the query.
    // type Storage: plumbing::QueryStorageOps<Self>;
    type Storage;

    /// A unique index identifying this query within the group.
    const QUERY_INDEX: u16;

    /// Name of the query method (e.g., `foo`)
    const QUERY_NAME: &'static str;

    /// Associate query group struct.
    type Group: plumbing::QueryGroup<GroupStorage = Self::GroupStorage>;

    /// Generated struct that contains storage for all queries in a group.
    type GroupStorage;

    /// Extact storage for this query from the storage for its group.
    fn query_storage<'a>(group_storage: &'a Self::GroupStorage) -> &'a Arc<Self::Storage>;
}

/// Trait implements by all of the "special types" associated with
/// each of your queries.
pub trait Query: for<'d> QueryDb<'d> {}

impl<Q> Query for Q where Q: for<'d> QueryDb<'d> {}

/// Return value from [the `query` method] on `Database`.
/// Gives access to various less common operations on queries.
///
/// [the `query` method]: trait.Database.html#method.query
pub struct QueryTable<'me, Q, DB>
where
    Q: Query,
{
    db: DB,
    storage: Arc<Q::Storage>,
    _marker: PhantomData<&'me ()>,
}

impl<'me, Q> QueryTable<'me, Q, &'me <Q as QueryDb<'me>>::DynDb>
where
    Q: Query,
    Q::Storage: QueryStorageOps<Q>,
{
    /// Constructs a new `QueryTable`.
    pub fn new(db: &'me <Q as QueryDb<'me>>::DynDb, storage: Arc<Q::Storage>) -> Self {
        Self {
            db,
            storage,
            _marker: PhantomData,
        }
    }
}

impl<'me, Q> QueryTable<'me, Q, <Q as QueryDb<'me>>::Db>
where
    Q: Query,
    Q::Storage: QueryStorageOps<Q>,
{
    /// Constructs a new `QueryTable`.
    pub fn new_async(db: <Q as QueryDb<'me>>::Db, storage: Arc<Q::Storage>) -> Self {
        Self {
            db,
            storage,
            _marker: PhantomData,
        }
    }
}

impl<'me, Q> QueryTable<'me, Q, &'me <Q as QueryDb<'me>>::DynDb>
where
    Q: Query,
    Q::Storage: QueryStorageOps<Q>,
{
    /// Remove all values for this query that have not been used in
    /// the most recent revision.
    pub fn sweep(&self, strategy: SweepStrategy)
    where
        Q::Storage: plumbing::QueryStorageMassOps,
    {
        self.storage.sweep(self.db.salsa_runtime(), strategy);
    }

    /// Peeks at the value at `Q::Key`. If it is currently in cache then it returns
    /// `Some`, otherwise `None`
    pub fn peek(&self, key: &Q::Key) -> Option<Q::Value> {
        self.storage.peek(self.db, key)
    }
}

impl<'me, Q> QueryTable<'me, Q, <Q as QueryDb<'me>>::Db>
where
    Q: Query,
    Q::Storage: QueryStorageOpsSync<Q>,
{
    /// Execute the query on a given input. Usually it's easier to
    /// invoke the trait method directly. Note that for variadic
    /// queries (those with no inputs, or those with more than one
    /// input) the key will be a tuple.
    pub fn get(&mut self, key: Q::Key) -> Q::Value {
        self.try_get(key).unwrap_or_else(|err| panic!("{}", err))
    }

    fn try_get(&mut self, key: Q::Key) -> Result<Q::Value, CycleError<DatabaseKeyIndex>> {
        self.storage.try_fetch(&mut self.db, &key)
    }
}

#[cfg(feature = "async")]
impl<'me, Q> QueryTable<'me, Q, <Q as QueryDb<'me>>::Db>
where
    Q: QueryBase,
    Q::Key: Send + Sync,
    Q::Value: Send + Sync,
    Q::Storage: QueryStorageOpsAsync<Q>,
    Q: for<'f, 'd> AsyncQueryFunction<'f, 'd>,
{
    /// Execute the query on a given input. Usually it's easier to
    /// invoke the trait method directly. Note that for variadic
    /// queries (those with no inputs, or those with more than one
    /// input) the key will be a tuple.
    pub async fn get_async(&mut self, key: Q::Key) -> Q::Value {
        self.try_get_async(key)
            .await
            .unwrap_or_else(|err| panic!("{}", err))
    }

    async fn try_get_async(
        &mut self,
        key: Q::Key,
    ) -> Result<Q::Value, CycleError<DatabaseKeyIndex>> {
        self.storage.try_fetch_async(&mut self.db, &key).await
    }
    /// Completely clears the storage for this query.
    ///
    /// This method breaks internal invariants of salsa, so any further queries
    /// might return nonsense results. It is useful only in very specific
    /// circumstances -- for example, when one wants to observe which values
    /// dropped together with the table
    pub fn purge(&self)
    where
        Q::Storage: plumbing::QueryStorageMassOps,
    {
        self.storage.purge();
    }
}

/// Return value from [the `query_mut` method] on `Database`.
/// Gives access to the `set` method, notably, that is used to
/// set the value of an input query.
///
/// [the `query_mut` method]: trait.Database.html#method.query_mut
pub struct QueryTableMut<'me, Q>
where
    Q: Query + 'me,
{
    db: &'me mut <Q as QueryDb<'me>>::DynDb,
    storage: Arc<Q::Storage>,
}

impl<'me, Q> QueryTableMut<'me, Q>
where
    Q: Query,
{
    /// Constructs a new `QueryTableMut`.
    pub fn new(db: &'me mut <Q as QueryDb<'me>>::DynDb, storage: Arc<Q::Storage>) -> Self {
        Self { db, storage }
    }

    /// Assign a value to an "input query". Must be used outside of
    /// an active query computation.
    ///
    /// If you are using `snapshot`, see the notes on blocking
    /// and cancellation on [the `query_mut` method].
    ///
    /// [the `query_mut` method]: trait.Database.html#method.query_mut
    pub fn set(&mut self, key: Q::Key, value: Q::Value)
    where
        Q::Storage: plumbing::InputQueryStorageOps<Q>,
    {
        self.set_with_durability(key, value, Durability::LOW);
    }

    /// Assign a value to an "input query", with the additional
    /// promise that this value will **never change**. Must be used
    /// outside of an active query computation.
    ///
    /// If you are using `snapshot`, see the notes on blocking
    /// and cancellation on [the `query_mut` method].
    ///
    /// [the `query_mut` method]: trait.Database.html#method.query_mut
    pub fn set_with_durability(&mut self, key: Q::Key, value: Q::Value, durability: Durability)
    where
        Q::Storage: plumbing::InputQueryStorageOps<Q>,
    {
        self.storage.set(self.db, &key, value, durability);
    }

    /// Sets the size of LRU cache of values for this query table.
    ///
    /// That is, at most `cap` values will be preset in the table at the same
    /// time. This helps with keeping maximum memory usage under control, at the
    /// cost of potential extra recalculations of evicted values.
    ///
    /// If `cap` is zero, all values are preserved, this is the default.
    pub fn set_lru_capacity(&self, cap: usize)
    where
        Q::Storage: plumbing::LruQueryStorageOps,
    {
        self.storage.set_lru_capacity(cap);
    }

    /// Marks the computed value as outdated.
    ///
    /// This causes salsa to re-execute the query function on the next access to
    /// the query, even if all dependencies are up to date.
    ///
    /// This is most commonly used as part of the [on-demand input
    /// pattern](https://salsa-rs.github.io/salsa/common_patterns/on_demand_inputs.html).
    pub fn invalidate(&mut self, key: &Q::Key)
    where
        Q::Storage: plumbing::DerivedQueryStorageOps<Q>,
    {
        self.storage.invalidate(self.db, key)
    }
}

/// The error returned when a query could not be resolved due to a cycle
#[derive(Eq, PartialEq, Clone, Debug)]
pub struct CycleError<K> {
    /// The queries that were part of the cycle
    cycle: Vec<K>,
    changed_at: Revision,
    durability: Durability,
}

impl<K> fmt::Display for CycleError<K>
where
    K: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Internal error, cycle detected:\n")?;
        for i in &self.cycle {
            writeln!(f, "{:?}", i)?;
        }
        Ok(())
    }
}

/// A boxed future used in the salsa traits
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Encapsulates a mutable reference to a database while only giving out shared references.
/// Use for asynchronous queries to make the database references passed `Send`
#[allow(explicit_outlives_requirements)] // https://github.com/rust-lang/rust/issues/60993
pub struct OwnedDb<'a, T>
where
    T: ?Sized,
{
    db: &'a mut T,
}

impl<'a, T> std::ops::Deref for OwnedDb<'a, T>
where
    T: ?Sized,
{
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.db
    }
}

impl<'a, T> OwnedDb<'a, T>
where
    T: ?Sized,
{
    #[doc(hidden)]
    pub fn new(db: &'a mut T) -> Self {
        Self { db }
    }

    #[doc(hidden)]
    pub fn __internal_into_db(self) -> &'a mut T {
        self.db
    }

    #[doc(hidden)]
    pub fn __internal_get_db(&mut self) -> &mut T {
        self.db
    }
}

impl<'a, T: ?Sized> From<&'a mut T> for OwnedDb<'a, T> {
    fn from(t: &'a mut T) -> Self {
        OwnedDb::new(t)
    }
}

impl<'a, 'b, T: ?Sized> From<&'a mut OwnedDb<'b, T>> for OwnedDb<'a, T> {
    fn from(t: &'a mut OwnedDb<'b, T>) -> Self {
        OwnedDb::new(t.db)
    }
}

impl<'a, 'b, T: ParallelDatabase> From<&'a mut Snapshot<T>> for OwnedDb<'a, T> {
    fn from(t: &'a mut Snapshot<T>) -> Self {
        OwnedDb::new(&mut t.db)
    }
}

impl<T> plumbing::DatabaseOps for OwnedDb<'_, T>
where
    T: ?Sized + plumbing::DatabaseOps,
{
    fn ops_database(&self) -> &dyn Database {
        self.db.ops_database()
    }

    fn ops_salsa_runtime(&self) -> &Runtime {
        self.db.ops_salsa_runtime()
    }

    fn ops_salsa_runtime_mut(&mut self) -> &mut Runtime {
        self.db.ops_salsa_runtime_mut()
    }

    fn fmt_index(
        &self,
        index: DatabaseKeyIndex,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        self.db.fmt_index(index, fmt)
    }

    fn maybe_changed_since(&self, input: DatabaseKeyIndex, revision: Revision) -> bool {
        self.db.maybe_changed_since(input, revision)
    }

    fn maybe_changed_since_async(
        &mut self,
        input: DatabaseKeyIndex,
        revision: Revision,
    ) -> BoxFuture<'_, bool> {
        self.db.maybe_changed_since_async(input, revision)
    }

    fn for_each_query(&self, op: &mut dyn FnMut(&dyn QueryStorageMassOps)) {
        self.db.for_each_query(op)
    }
}

impl<T> Database for OwnedDb<'_, T>
where
    T: ?Sized + Database,
{
    fn sweep_all(&self, strategy: SweepStrategy) {
        self.db.sweep_all(strategy)
    }

    fn salsa_event(&self, event_fn: Event) {
        self.db.salsa_event(event_fn)
    }

    fn on_propagated_panic(&self) -> ! {
        self.db.on_propagated_panic()
    }

    fn salsa_runtime(&self) -> &Runtime {
        self.db.salsa_runtime()
    }

    fn salsa_runtime_mut(&mut self) -> &mut Runtime {
        self.db.salsa_runtime_mut()
    }
}

/// TODO
#[macro_export]
macro_rules! cast_owned_db {
    ($db: expr => $ty: ty) => {
        $crate::OwnedDb::new($db.__internal_into_db() as $ty)
    };
}

// Re-export the procedural macros.
#[allow(unused_imports)]
#[macro_use]
extern crate gluon_salsa_macros;
#[doc(hidden)]
pub use gluon_salsa_macros::*;
