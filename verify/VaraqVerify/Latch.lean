module

public import Veil

/-!
# The hybrid latch

A model of `HybridLatch` (`crates/varaq/src/latch.rs`): one 64-bit word holds
the state (`0` free, `1..=254` sharers, `0xFF` exclusive), a count of waiters
and a version. Optimistic readers note the version, copy the data and check
the version again; shared holders count themselves into the state; an
exclusive holder bumps the version when it lets go.

Every CAS, load and store of the word is one step, and the latch's data is
two words written and read one at a time, so a reader can see a torn value.
The properties are those the tree relies on:

* a reader whose `recheck` passes copied a value no writer was changing
  (both data words agree);
* a shared holder never reads while a writer holds the latch;
* at most one exclusive holder, never alongside sharers.

The version is unbounded here (it is 48 bits in the code, wrapping to 0 after
2^48 - 1 releases); the search is bounded by the number of writes.
-/

veil module VaraqLatch

type thread
instantiate tEnum : Veil.Enumeration thread

/- What each thread is doing.
`optStart`: about to load the word. `optLo`/`optHi`: copying the data under
an optimistic guard. `optCheck`: about to recheck. `sharedLo`/`sharedHi`:
reading under a shared guard. `exclLo`/`exclHi`: writing under the exclusive
guard. `exclDone`: written, about to release. `waiting`: announced as a
waiter. -/
enum Pc = { idle, optStart, optLo, optHi, optCheck,
            sharedLo, sharedHi, exclLo, exclHi, exclDone, waiting }

/- The latch word. -/
individual exclusive : Bool
individual sharers : Nat
individual waiters : Nat
individual version : Nat

/- The protected value, two words; a write sets both to `writes + 1`. -/
individual lo : Nat
individual hi : Nat
individual writes : Nat

/- The search bound: how many writes the threads make in all. -/
immutable individual maxWrites : Nat

function pc : thread → Pc
/- The version an optimistic guard noted. -/
function seen : thread → Nat
/- What a reader copied. -/
function gotLo : thread → Nat
function gotHi : thread → Nat
/- Whether a thread holding the exclusive guard announced itself first. -/
function announced : thread → Bool

#gen_state

after_init {
  exclusive := false
  sharers := 0
  waiters := 0
  version := 0
  lo := 0
  hi := 0
  writes := 0
  pc T := idle
  seen T := 0
  gotLo T := 0
  gotHi T := 0
  announced T := false
}

/- `SHARED_MAX` and `WAITING_MAX`, scaled down. -/
abbrev sharedMax : Nat := 2
abbrev waitingMax : Nat := 2

/-! ## Optimistic reads: `optimistic`, `read`/`peek`, `recheck` -/

action optimistic (t : thread) {
  require pc t = idle
  pc t := optStart
}

/- `HybridLatch::optimistic`: refuse while held exclusively. -/
action optLoad (t : thread) {
  require pc t = optStart
  if exclusive then
    pc t := idle
  else
    seen t := version
    pc t := optLo
}

action optReadLo (t : thread) {
  require pc t = optLo
  gotLo t := lo
  pc t := optHi
}

action optReadHi (t : thread) {
  require pc t = optHi
  gotHi t := hi
  pc t := optCheck
}

/- `OptimisticGuard::recheck` (`unchanged_since`). A reader that passes uses
what it copied. -/
action optRecheck (t : thread) {
  require pc t = optCheck
  if ¬ exclusive ∧ version = seen t then
    assert gotLo t = gotHi t
  pc t := idle
}

/- `OptimisticGuard::try_shared`: add a sharer if the word is not held
exclusively, still at the noted version, and below `SHARED_MAX`. -/
action tryShared (t : thread) {
  require pc t = optCheck
  if ¬ exclusive ∧ version = seen t ∧ sharers < sharedMax then
    sharers := sharers + 1
    pc t := sharedLo
  else
    pc t := idle
}

action sharedReadLo (t : thread) {
  require pc t = sharedLo
  gotLo t := lo
  pc t := sharedHi
}

/- Dropping the `SharedGuard`: a `fetch_sub` of one sharer. -/
action sharedReadHiAndRelease (t : thread) {
  require pc t = sharedHi
  assert lo = gotLo t ∧ hi = gotLo t
  sharers := sharers - 1
  pc t := idle
}

/- `OptimisticGuard::upgrade`: a CAS from the bare noted version, which
fails if the word has sharers, waiters or a newer version. -/
action upgrade (t : thread) {
  require pc t = optCheck
  if ¬ exclusive ∧ sharers = 0 ∧ waiters = 0 ∧ version = seen t then
    exclusive := true
    announced t := false
    pc t := exclLo
  else
    pc t := idle
}

/-! ## Exclusive access: `exclusive`, `try_exclusive`, `announce` + `claim` -/

/- `HybridLatch::exclusive` / `try_exclusive`: state and waiters both 0. -/
action takeExclusive (t : thread) {
  require pc t = idle
  require ¬ exclusive ∧ sharers = 0 ∧ waiters = 0
  exclusive := true
  announced t := false
  pc t := exclLo
}

/- `HybridLatch::announce`: add a waiter unless the count is saturated. -/
action announce (t : thread) {
  require pc t = idle
  require waiters < waitingMax
  waiters := waiters + 1
  pc t := waiting
}

/- `HybridLatch::claim`: a waiter takes the latch once the state is 0,
whatever the count of waiters. -/
action claim (t : thread) {
  require pc t = waiting
  require ¬ exclusive ∧ sharers = 0
  exclusive := true
  announced t := true
  pc t := exclLo
}

/- Dropping `Waiting` without claiming. -/
action giveUp (t : thread) {
  require pc t = waiting
  waiters := waiters - 1
  pc t := idle
}

action writeLo (t : thread) {
  require pc t = exclLo
  assert exclusive ∧ sharers = 0
  lo := writes + 1
  pc t := exclHi
}

action writeHi (t : thread) {
  require pc t = exclHi
  hi := writes + 1
  writes := writes + 1
  pc t := exclDone
}

/- Dropping the `ExclusiveGuard`: `released`, which clears the state, keeps
the waiters and bumps the version; then dropping `Waiting` if the holder
announced itself. -/
action releaseExclusive (t : thread) {
  require pc t = exclDone
  exclusive := false
  version := version + 1
  if announced t then
    waiters := waiters - 1
  announced t := false
  pc t := idle
}

/-! ## Properties -/

safety [one_writer]
  ∀ a b, (pc a = exclLo ∨ pc a = exclHi ∨ pc a = exclDone) →
    (pc b = exclLo ∨ pc b = exclHi ∨ pc b = exclDone) → a = b

safety [writer_alone]
  ∀ a, (pc a = exclLo ∨ pc a = exclHi ∨ pc a = exclDone) →
    exclusive ∧ sharers = 0

safety [sharers_counted]
  sharers = (tEnum.allValues.filter (fun t => pc t = sharedLo ∨ pc t = sharedHi)).length

state_constraint [bounded] writes ≤ maxWrites

#gen_spec

#model_check { thread := Fin 3 } { maxWrites := 2 }

end VaraqLatch
