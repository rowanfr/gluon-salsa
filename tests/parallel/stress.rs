use rand::seq::SliceRandom;
use rand::Rng;

use salsa::Database;
use salsa::ParallelDatabase;
use salsa::Snapshot;
use salsa::SweepStrategy;

// Number of operations a reader performs
const N_MUTATOR_OPS: usize = 100;
const N_READER_OPS: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Canceled;
type Cancelable<T> = Result<T, Canceled>;

#[salsa::query_group(Stress)]
trait StressDatabase: salsa::Database {
    #[salsa::input]
    fn a(&self, key: usize) -> usize;

    fn b(&self, key: usize) -> Cancelable<usize>;

    fn c(&self, key: usize) -> Cancelable<usize>;
}

fn b(db: &dyn StressDatabase, key: usize) -> Cancelable<usize> {
    if db.salsa_runtime().is_current_revision_canceled() {
        return Err(Canceled);
    }
    Ok(db.a(key))
}

fn c(db: &dyn StressDatabase, key: usize) -> Cancelable<usize> {
    db.b(key)
}

#[salsa::database(Stress)]
#[derive(Default)]
struct StressDatabaseImpl {
    storage: salsa::Storage<Self>,
}

impl salsa::Database for StressDatabaseImpl {}

impl salsa::ParallelDatabase for StressDatabaseImpl {
    fn snapshot(&self) -> Snapshot<StressDatabaseImpl> {
        Snapshot::new(StressDatabaseImpl {
            storage: self.storage.snapshot(),
        })
    }

    fn fork(&self, forker: salsa::ForkState) -> salsa::Snapshot<Self> {
        salsa::Snapshot::new(Self {
            storage: self.storage.fork(forker),
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum Query {
    A,
    B,
    C,
}

enum MutatorOp {
    WriteOp(WriteOp),
    LaunchReader {
        ops: Vec<ReadOp>,
        check_cancellation: bool,
    },
}

#[derive(Debug)]
enum WriteOp {
    SetA(usize, usize),
}

#[derive(Debug)]
enum ReadOp {
    Get(Query, usize),
    Gc(Query, SweepStrategy),
    GcAll(SweepStrategy),
}

impl rand::distributions::Distribution<Query> for rand::distributions::Standard {
    fn sample<R: rand::Rng + ?Sized>(&self, rng: &mut R) -> Query {
        *[Query::A, Query::B, Query::C].choose(rng).unwrap()
    }
}

impl rand::distributions::Distribution<MutatorOp> for rand::distributions::Standard {
    fn sample<R: rand::Rng + ?Sized>(&self, rng: &mut R) -> MutatorOp {
        if rng.gen_bool(0.5) {
            MutatorOp::WriteOp(rng.gen())
        } else {
            MutatorOp::LaunchReader {
                ops: (0..N_READER_OPS).map(|_| rng.gen()).collect(),
                check_cancellation: rng.gen(),
            }
        }
    }
}

impl rand::distributions::Distribution<WriteOp> for rand::distributions::Standard {
    fn sample<R: rand::Rng + ?Sized>(&self, rng: &mut R) -> WriteOp {
        let key = rng.gen::<usize>() % 10;
        let value = rng.gen::<usize>() % 10;
        return WriteOp::SetA(key, value);
    }
}

impl rand::distributions::Distribution<ReadOp> for rand::distributions::Standard {
    fn sample<R: rand::Rng + ?Sized>(&self, rng: &mut R) -> ReadOp {
        if rng.gen_bool(0.5) {
            let query = rng.gen::<Query>();
            let key = rng.gen::<usize>() % 10;
            return ReadOp::Get(query, key);
        }
        let mut strategy = SweepStrategy::discard_outdated();
        if rng.gen_bool(0.5) {
            strategy = strategy.discard_values();
        }
        if rng.gen_bool(0.5) {
            ReadOp::Gc(rng.gen::<Query>(), strategy)
        } else {
            ReadOp::GcAll(strategy)
        }
    }
}

fn db_reader_thread(db: &StressDatabaseImpl, ops: Vec<ReadOp>, check_cancellation: bool) {
    for op in ops {
        if check_cancellation {
            if db.salsa_runtime().is_current_revision_canceled() {
                return;
            }
        }
        op.execute(db);
    }
}

impl WriteOp {
    fn execute(self, db: &mut StressDatabaseImpl) {
        match self {
            WriteOp::SetA(key, value) => {
                db.set_a(key, value);
            }
        }
    }
}

impl ReadOp {
    fn execute(self, db: &StressDatabaseImpl) {
        match self {
            ReadOp::Get(query, key) => match query {
                Query::A => {
                    db.a(key);
                }
                Query::B => {
                    let _ = db.b(key);
                }
                Query::C => {
                    let _ = db.c(key);
                }
            },
            ReadOp::Gc(query, strategy) => match query {
                Query::A => {
                    AQuery.in_db(db).sweep(strategy);
                }
                Query::B => {
                    BQuery.in_db(db).sweep(strategy);
                }
                Query::C => {
                    CQuery.in_db(db).sweep(strategy);
                }
            },
            ReadOp::GcAll(strategy) => {
                db.sweep_all(strategy);
            }
        }
    }
}

#[test]
fn stress_test() {
    let mut db = StressDatabaseImpl::default();
    for i in 0..10 {
        db.set_a(i, i);
    }

    let mut rng = rand::thread_rng();

    // generate the ops that the mutator thread will perform
    let write_ops: Vec<MutatorOp> = (0..N_MUTATOR_OPS).map(|_| rng.gen()).collect();

    // execute the "main thread", which sometimes snapshots off other threads
    let mut all_threads = vec![];
    for op in write_ops {
        match op {
            MutatorOp::WriteOp(w) => w.execute(&mut db),
            MutatorOp::LaunchReader {
                ops,
                check_cancellation,
            } => all_threads.push(std::thread::spawn({
                let db = db.snapshot();
                move || db_reader_thread(&db, ops, check_cancellation)
            })),
        }
    }

    for thread in all_threads {
        thread.join().unwrap();
    }
}
