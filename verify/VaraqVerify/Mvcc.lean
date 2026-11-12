module

public import Veil

/-!
# Snapshots, commits and garbage collection

A model of how Varaq decides which committed versions a transaction may see,
and of the garbage collection that runs alongside it:

* `Snapshots` (`crates/varaq/src/txn.rs`): one slot per worker, set to
  `BEGINNING` by a CAS before the clock is read, then to the snapshot.
* `Clock` (`txn/clock.rs`): `snapshot` reads `next`, `take` returns it and
  increments it.
* `Commits` (`txn/commits.rs`): per worker, a list of entries
  `(commit_ts, start_ts, acked)` behind a lock; `visible` (fast path through
  `all_lwm`, then `lcb`), `is_acked`, `ack` (with the `acked_upto` prefix) and
  `compact`.
* `Db::begin`, `Transaction::precommit`, `commit_read_only`, `roll_back`,
  `Db::refresh_visible_to_all` and `Db::collect` (`txn/db.rs`).

Each step of the model is one atomic step of the code: one atomic operation,
or one critical section of a worker's commit lock. The garbage collector's
scan of the snapshot slots takes one step per slot, in any order.

The property checked is snapshot isolation's visibility rule: a transaction
whose snapshot is `S` sees exactly the versions of transactions that
committed with a commit timestamp below `S`, plus its own writes. The model
compares what `Commits::visible` answers, from whatever state the collector
left behind, against that rule, computed from a history that is never
compacted. It also checks that `Commits::is_acked` agrees with the history.
-/

veil module VaraqMvcc

type worker
instantiate wEnum : Veil.Enumeration worker

/- One entry of a worker's list of commits. -/
@[veil_decl] structure Entry where
  commitTs : Nat
  startTs : Nat
  acked : Bool

/- A worker's snapshot slot: `IDLE`, `BEGINNING`, or a published timestamp. -/
enum SlotKind = { slotIdle, slotBeginning, slotPublished }

/- Where a worker's transaction is.
`beginning`: the slot holds `BEGINNING`, the clock is not read yet.
`snapped`: the clock was read, the slot still holds `BEGINNING`.
`active`: the snapshot is published.
`committed`: `Commits::commit` returned, the slot is not released yet. -/
enum Phase = { txIdle, txBeginning, txSnapped, txActive, txCommitted }

/- The collector's progress through `refresh_visible_to_all` / `collect`. -/
enum GcPhase = { gcIdle, gcScanning, gcReady }

individual clock : Nat

/- The search bound: how many timestamps the clock hands out. -/
immutable individual maxClock : Nat

function slotKind : worker → SlotKind
function slotAt : worker → Nat
function phase : worker → Phase
/- The transaction's snapshot, which is also its start timestamp: the tag
its versions carry. -/
function snap : worker → Nat
function wrote : worker → Bool

/- `Commits`: each worker's entries, `all_lwm` and `acked_upto`. -/
function entries : worker → List Entry
function allLwm : worker → Nat
function ackedUpto : worker → Nat

/- Ghost: every commit ever made, never compacted. -/
function hist : worker → List Entry

/- The collector's locals. -/
individual gc : GcPhase
individual gcFloor : Nat
individual gcOldest : Option Nat
individual gcBound : Nat
relation gcScanned : worker → Bool
relation gcRefreshed : worker → Bool

#gen_state

after_init {
  clock := 1
  slotKind X := slotIdle
  slotAt X := 0
  phase X := txIdle
  snap X := 0
  wrote X := false
  entries X := []
  allLwm X := 0
  ackedUpto X := 0
  hist X := []
  gc := gcIdle
  gcFloor := 0
  gcOldest := none
  gcBound := 0
  gcScanned X := false
  gcRefreshed X := false
}

/-! ## The code's decisions, as functions of the state -/

/- `cut`: the start timestamp of the last entry committed below `snapshot`. -/
@[inline] def cut (es : List Entry) (snapshot : Nat) : Option Nat :=
  ((es.filter (fun e => e.commitTs < snapshot)).getLast?).map (·.startTs)

/- `Commits::visible`. -/
@[inline] def codeVisible (lwm : Nat) (es : List Entry) (tag snapshot : Nat) : Bool :=
  tag < lwm || (match cut es snapshot with
    | none => false
    | some c => tag ≤ c)

/- `Commits::is_acked`. -/
@[inline] def codeAcked (upto : Nat) (es : List Entry) (tag : Nat) : Bool :=
  tag ≤ upto || (match es.find? (fun e => e.startTs == tag) with
    | none => true
    | some e => e.acked)

/- `Commits::compact`. -/
@[inline] def compactAt (es : List Entry) (bound : Nat) : List Entry :=
  let below := (es.filter (fun e => e.commitTs < bound)).length
  let unacked := match es.findIdx? (fun e => !e.acked) with
    | none => es.length
    | some i => i
  es.drop (min (below - 1) unacked)

/- The new `acked_upto` after `Commits::ack`. -/
@[inline] def ackedPrefix (es : List Entry) (from_ : Nat) : Nat :=
  match ((es.filter (fun e => e.startTs > from_)).takeWhile (·.acked)).getLast? with
  | none => from_
  | some e => max from_ e.startTs

@[inline] def setAcked (es : List Entry) (tag : Nat) : List Entry :=
  es.map (fun e => if e.startTs == tag then { e with acked := true } else e)

/-! ## A worker's transaction -/

/- `Db::begin`: `Snapshots::beginning`, a CAS from `IDLE` to `BEGINNING`. -/
action begin (w : worker) {
  require phase w = txIdle
  require slotKind w = slotIdle
  slotKind w := slotBeginning
  phase w := txBeginning
}

/- `Db::begin`: `Clock::snapshot`. -/
action readClock (w : worker) {
  require phase w = txBeginning
  snap w := clock
  wrote w := false
  phase w := txSnapped
}

/- `Db::begin`: `Snapshots::publish`. -/
action publish (w : worker) {
  require phase w = txSnapped
  slotKind w := slotPublished
  slotAt w := snap w
  phase w := txActive
}

/- A write installs a version tagged with the writer and its start timestamp. -/
action write (w : worker) {
  require phase w = txActive
  wrote w := true
}

/- `Commits::commit`, called by `precommit`: one critical section of the
worker's lock. The two checks are the ones that return `Error::Corrupt`. -/
action commit (w : worker) {
  require phase w = txActive
  require wrote w
  let es := entries w
  assert ((es.getLast?).all (fun l => decide (l.startTs < snap w)))
  let ts := clock
  clock := clock + 1
  assert ((es.getLast?).all (fun l => decide (l.commitTs < ts)))
  entries w := es ++ [{ commitTs := ts, startTs := snap w, acked := false }]
  hist w := hist w ++ [{ commitTs := ts, startTs := snap w, acked := false }]
  phase w := txCommitted
}

/- `give_back_worker` after `precommit`: `Snapshots::release`. -/
action releaseAfterCommit (w : worker) {
  require phase w = txCommitted
  slotKind w := slotIdle
  phase w := txIdle
}

/- `commit_read_only`: no entry, the slot is released. -/
action commitReadOnly (w : worker) {
  require phase w = txActive
  require ¬ wrote w
  slotKind w := slotIdle
  phase w := txIdle
}

/- `roll_back` then `give_back_worker`: a transaction that wrote takes a
timestamp so the next one on its worker starts later. Its versions are
undone, so no reader that starts afterwards can meet them. -/
action abort (w : worker) {
  require phase w = txActive
  if wrote w then
    clock := clock + 1
  slotKind w := slotIdle
  phase w := txIdle
}

/- `Commits::ack` for one committed transaction, in any order (the log
decides the real order; any order is a superset). -/
action ack (w : worker) (i : Fin 4) {
  let h := hist w
  require i.val < h.length
  let tag := (h.getD i.val { commitTs := 0, startTs := 0, acked := true }).startTs
  require (h.any (fun e => e.startTs == tag && !e.acked))
  hist w := setAcked h tag
  if tag > ackedUpto w then
    let es := setAcked (entries w) tag
    entries w := es
    ackedUpto w := max (ackedUpto w) (ackedPrefix es (ackedUpto w))
}

/-! ## The collector: `refresh_visible_to_all` and `collect` -/

/- Read the floor, `Clock::snapshot`. -/
action gcStart {
  require gc = gcIdle
  gcFloor := clock
  gcOldest := none
  gcScanned X := false
  gcRefreshed X := false
  gc := gcScanning
}

/- `Snapshots::watermark`: one slot per step. A slot holding `BEGINNING`
abandons the pass (`Watermark::Beginning`). -/
action gcScan (w : worker) {
  require gc = gcScanning
  require ¬ gcScanned w
  if slotKind w = slotBeginning then
    gc := gcIdle
  else
    if slotKind w = slotPublished then
      gcOldest := some (match gcOldest with
        | none => slotAt w
        | some o => min o (slotAt w))
    gcScanned w := true
}

/- `collect_bound_at`: the older of the oldest snapshot and the floor. -/
action gcBoundReady {
  require gc = gcScanning
  require ∀ w, gcScanned w
  gcBound := (match gcOldest with
    | none => gcFloor
    | some o => min o gcFloor)
  gc := gcReady
}

/- `refresh_visible_to_all` for one worker: `lcb` under the lock, then
`publish_all_lwm` (a `fetch_max`). The two are separate steps in the code;
taking them together here only removes interleavings in which the mark is
published later, which `fetch_max` makes harmless. -/
action gcRefresh (w : worker) {
  require gc = gcReady
  require ¬ gcRefreshed w
  allLwm w := (match cut (entries w) gcBound with
    | none => allLwm w
    | some c => max (allLwm w) (c + 1))
  gcRefreshed w := true
}

/- `collect` ends with `Commits::compact` under the lock. -/
action gcCompact (w : worker) {
  require gc = gcReady
  entries w := compactAt (entries w) gcBound
}

action gcFinish {
  require gc = gcReady
  gc := gcIdle
}

/-! ## Properties -/

/- A worker's entries stay ordered by start and by commit timestamp, which
the binary searches in `Commits` rely on. -/
safety [entries_sorted]
  ∀ w, ((entries w).zip ((entries w).drop 1)).all
    (fun p => p.1.startTs < p.2.startTs && p.1.commitTs < p.2.commitTs)

/- Snapshot isolation: a transaction with snapshot `S` sees a committed
version exactly when its writer committed below `S`. -/
safety [visibility_committed]
  ∀ r w, phase r = txActive → r ≠ w →
    (hist w).all (fun e =>
      codeVisible (allLwm w) (entries w) e.startTs (snap r) == decide (e.commitTs < snap r))

/- No transaction sees the versions of a transaction still running on
another worker. -/
safety [visibility_running]
  ∀ r w, phase r = txActive → r ≠ w → phase w = txActive → wrote w →
    codeVisible (allLwm w) (entries w) (snap w) (snap r) = false

/- `is_acked` agrees with what was acknowledged. -/
safety [acked_agrees]
  ∀ w, (hist w).all (fun e => codeAcked (ackedUpto w) (entries w) e.startTs == e.acked)

/- Keep the search finite: a bounded number of timestamps. -/
state_constraint [bounded] clock ≤ maxClock

#gen_spec

#model_check { worker := Fin 2 } { maxClock := 4 }

end VaraqMvcc
