# Varaq

An embedded, transactional key-value store for Rust.

Varaq is a B+tree that runs at in-memory speed while your data fits in RAM and keeps going when it
does not: pages spill to disk through a buffer manager, and come back when they are needed. Writes
go through a write-ahead log, transactions commit or abort as a unit, and a store reopens after a
crash with every acknowledged write in place.

It is a library, not a server. You link it into your program, give it somewhere to keep its pages
(memory, ordinary files, or the browser's private file system) and call it from your own threads.
There is no async runtime to adopt and no background process to manage.

```toml
[dependencies]
varaq = "0.1"
```

> **Status: 0.1, early.** The API and the on-disk format will change before 1.0. Pin a version
> and expect to recreate stores across upgrades.

## Why Varaq

- **Fast in memory, honest on disk.** Readers never take a lock: they use optimistic latches and
  retry if a writer moved underneath them. Pages that are not needed leave memory; pages that are
  come back with one read.
- **Transactions.** Group inserts and removes, scan inside a snapshot, then commit or abort.
- **Correct or refuse.** Every call either gives the right answer or returns a named error. A torn
  page, a damaged log or a store whose creation never finished is reported as such, never served as
  data.
- **Runs where your code runs.** Linux, macOS and other Unix systems over ordinary files, Linux
  with `io_uring`, and the browser through WebAssembly and OPFS.
- **No runtime.** Plain threads. Nothing to spawn, no executor to pick.

## Quick start

### A tree in memory

The B+tree on its own: ordered keys and values as bytes.

```rust
use varaq::tree::Tree;
use varaq::tree::scan::Step;

fn main() -> Result<(), varaq::Error> {
    // A tree with room for 1,024 pages (4 KiB each) in memory.
    let tree = Tree::with_capacity(1_024)?;

    tree.insert(b"apple", b"red")?;
    tree.insert(b"banana", b"yellow")?;
    assert_eq!(tree.get(b"apple")?, Some(b"red".to_vec()));

    assert!(tree.remove(b"banana")?);

    // Visit every key in order, starting from the first.
    tree.scan(b"", |key, value| {
        println!("{:?} = {:?}", key, value);
        Step::Continue
    })?;
    Ok(())
}
```

### A transactional store

`Db` adds a write-ahead log and transactions on top of the tree.

```rust
use varaq::WorkerId;
use varaq::tree::scan::Step;
use varaq::txn::db::Db;

fn main() -> Result<(), varaq::Error> {
    // One worker, a pool of 1,024 pages.
    let db = Db::with_capacity(1, 1_024)?;

    // Every thread that writes is bound to a worker for as long as it holds the guard.
    let me = WorkerId(0);
    let _bound = varaq::wal::worker::bind(0);

    // Single writes commit on their own.
    db.insert(me, b"apple", b"1")?;
    db.insert(me, b"pear", b"2")?;

    // A transaction: all of it commits, or none of it does.
    let mut tx = db.begin(me)?;
    tx.insert(b"plum", b"3")?;
    tx.remove(b"apple")?;
    tx.commit()?;

    assert_eq!(db.get(me, b"apple")?, None);
    assert_eq!(db.get(me, b"plum")?, Some(b"3".to_vec()));

    // A scan inside a transaction sees one consistent snapshot.
    let tx = db.begin(me)?;
    let mut keys = Vec::new();
    tx.scan(b"", |key, _| {
        keys.push(key.to_vec());
        Step::Continue
    })?;
    tx.commit()?;
    assert_eq!(keys, [b"pear".to_vec(), b"plum".to_vec()]);
    Ok(())
}
```

### A store on disk

A persistent store lives in four files: a catalog, the log, the data and the old versions that
transactions read. `Devices` creates a store over them, or opens the one already there.

```rust
use std::fs::{File, OpenOptions};
use std::path::Path;
use varaq::WorkerId;
use varaq::io::devices::{Devices, Refusal};

fn devices(dir: &Path) -> std::io::Result<Devices<File>> {
    let file = |name: &str| {
        OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join(name))
    };
    Ok(Devices::new(file("catalog")?, file("log")?, file("data")?, file("versions")?))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join(format!("varaq-readme-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let _bound = varaq::wal::worker::bind(0);

    {
        // One worker, 1,024 pages of memory, a 4 MiB log ring per worker.
        let db = devices(&dir)?.create(1, 1_024, 4 << 20)?;
        db.insert(WorkerId(0), b"hello", b"disk")?;
    } // Dropping the store closes it.

    // Open it again: what was acknowledged is there.
    let db = devices(&dir)?.open(1_024)?;
    assert_eq!(db.get(WorkerId(0), b"hello").map_err(Refusal::from)?, Some(b"disk".to_vec()));

    drop(db);
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
```

`open` refuses a directory with no store (`Refusal::NoSuchStore`), a store another process holds
(`Refusal::StoreBusy`) and a create that was cut short (`Refusal::IncompleteCreate`, which
`Devices::destroy` clears). `create` refuses files that already hold a store.

### Many threads

A store has a fixed number of workers, chosen when it is made. Each thread that uses the store binds
itself to a different worker; the store is shared by reference.

```rust
use varaq::WorkerId;
use varaq::txn::db::Db;

fn main() -> Result<(), varaq::Error> {
    let workers = 4;
    let db = Db::with_capacity(workers, 4_096)?;

    std::thread::scope(|s| {
        for w in 0..workers {
            let db = &db;
            s.spawn(move || {
                let _bound = varaq::wal::worker::bind(w as u16);
                let me = WorkerId(w);
                for i in 0u32..1_000 {
                    let key = [&(w as u32).to_be_bytes()[..], &i.to_be_bytes()].concat();
                    db.insert(me, &key, b"value").unwrap();
                }
            });
        }
    });

    let me = WorkerId(0);
    let _bound = varaq::wal::worker::bind(0);
    assert_eq!(db.get(me, &[0, 0, 0, 3, 0, 0, 0, 7])?, Some(b"value".to_vec()));
    Ok(())
}
```

### In the browser

With the `opfs` feature and the `wasm32-unknown-unknown` target, a store lives in the browser's
Origin Private File System. OPFS's synchronous file handles exist only in a dedicated Web Worker, so
run the store there.

```rust,ignore
use varaq::WorkerId;
use varaq::io::devices::Refusal;
use varaq::io::opfs;

async fn run() -> Result<(), Refusal> {
    let root = opfs::root().await?;
    // Open the store named "example", or create it the first time.
    let db = match opfs::acquire(&root, "example", false).await.and_then(|d| d.open(4_096)) {
        Ok(db) => db,
        Err(Refusal::NoSuchStore) => {
            opfs::acquire(&root, "example", true).await?.create(2, 4_096, 4 << 20)?
        }
        Err(refused) => return Err(refused),
    };
    let _bound = varaq::wal::worker::bind(0);
    db.insert(WorkerId(0), b"hello", b"opfs")?;
    Ok(())
}
```

A second engine that tries to open the same store, say from another tab, is refused with
`Refusal::StoreBusy` until the first lets go.

## Concepts

| Term | What it means |
|---|---|
| **Page** | 4 KiB. The unit the tree, the buffer pool and the files work in. |
| **Pool pages** | How many pages are kept in memory. A store larger than this spills to its files. |
| **Worker** | A slot for one thread at a time, with its own log. A thread binds with `varaq::wal::worker::bind(n)` and passes `WorkerId(n)` to the store. Two threads must not share a worker at once. |
| **Log ring** | Each worker's share of the write-ahead log, in bytes. The log is reused as checkpoints move forward. |
| **`Error`** | Something the engine refused: a corrupt page, a full store, a device failure. |
| **`Refusal`** | Why a store could not be created, opened or destroyed over its files; wraps `Error` for everything else. |

Commits are durable by default: a commit returns once its log record is on the device. A store made
with `.with_durability(Durability::Eventual)` answers once the record is written, and syncs in the
background with a bounded amount at risk.

## Cargo features

| Feature | What it adds |
|---|---|
| `opfs` | The browser backend (`varaq::io::opfs`), for `wasm32-unknown-unknown`. |
| `uring` | An `io_uring` backend (`varaq::io::uring`) on Linux. |
| `stats` | Internal counters, kept per thread so they cost little. Off by default. |

## What is inside

| Module | Contents |
|---|---|
| `varaq::tree` | The B+tree: slotted pages, optimistic lock coupling, scans. |
| `varaq::txn` | `Db` and `Transaction`: commits, snapshots, old versions. |
| `varaq::wal` | The write-ahead log, a ring per worker, and worker binding. |
| `varaq::bufmgr` | The buffer manager that moves pages between memory and the files. |
| `varaq::recovery` | Reopening a store from its checkpoint and log. |
| `varaq::io` | Backends: files through `pread` and `pwrite`, memory, `io_uring`, OPFS. |
| `varaq::exec` | Where a worker waits when the page it needs is not in memory. |

The design comes from research on storage engines built for fast SSDs: pages that can be referenced from more
than one place, a buffer pool that cools pages before evicting them, and a log split per worker so
threads do not contend on one tail.

## Examples

The repository has four short programs under `crates/varaq/examples`, each checking its own
results:

```sh
cargo run --release --example tree    # the tree in memory
cargo run --release --example db      # transactions in memory
cargo run --release --example files   # a store over four files
cargo build --target wasm32-unknown-unknown --features opfs --example opfs
```

## Platforms

- **Linux:** files through `pread`/`pwrite`, or `io_uring` with the `uring` feature.
- **macOS and other Unix systems:** files through `pread`/`pwrite`. The operating system's page
  cache sits underneath, so numbers taken there are not device numbers.
- **WebAssembly:** `wasm32-unknown-unknown`, in memory or over OPFS with the `opfs` feature.

The minimum supported Rust version is 1.95.

## License

Apache-2.0.
