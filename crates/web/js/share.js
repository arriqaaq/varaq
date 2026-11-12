const BUSY_DELAYS_MS = [25, 50, 100, 200, 400, 800, 1000, 1000, 1000, 1000];

const COMMITS = new Set(['commit', 'put', 'delete']);

const TX_CALLS = new Set(['tx_get', 'tx_put', 'tx_delete', 'tx_scan', 'commit', 'abort']);

const GONE = new Set(['UnknownTransaction', 'LeaderChanged']);

const CALLS = new Set(['get', 'put', 'delete', 'scan', 'begin', ...TX_CALLS]);

const named = (name, message) => ({ name, message });
const refusal = (error) => ({
  error: { name: error?.name ?? 'Error', message: error?.message ?? String(error) },
});
const trapped = (e) => e instanceof WebAssembly.RuntimeError;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function hold(name) {
  return new Promise((granted) => {
    navigator.locks.request(name, () => {
      granted();
      return new Promise(() => {});
    });
  });
}

async function whenFree(open) {
  for (const delay of BUSY_DELAYS_MS) {
    try {
      return await open();
    } catch (error) {
      if (error?.name !== 'StoreBusy') throw error;
      await sleep(delay);
    }
  }
  return open();
}

export function startWorker({ init, Store, hooks = {} }) {

  const ready = init().then(() => null, (error) => String(error));
  const tab = crypto.randomUUID();
  let closed = null;
  let role = 'idle';
  let store = null;
  let term = null;
  let prefix = null;
  let channels = null;
  let joining = null;
  const waiting = [];

  const handles = new Map();
  let nextHandle = 1;

  const pending = new Map();
  let nextRid = 1;
  let learning = Promise.resolve();

  const owners = new Map();
  const owned = new Map();
  const watched = new Set();
  const replyTo = new Map();

  function toPage(id, body) {
    self.postMessage({ id, ...body });
  }

  function close(name, message) {
    if (closed) return;
    closed = named(name, message);
    store = null;

    endPending(null);
    for (const call of waiting.splice(0)) toPage(call.id, refusal(closed));
    if (joining) joining.reject(closed);
    joining = null;
    self.postMessage({ closed });
    self.close();
  }

  self.addEventListener('error', (event) => {
    if (trapped(event.error)) close('Poisoned', String(event.error));
  });
  self.addEventListener('unhandledrejection', (event) => {
    if (trapped(event.reason)) close('Poisoned', String(event.reason));
  });

  function serve(from, callTerm, op, args) {
    if (callTerm !== term) {
      return refusal(named('LeaderChanged', 'the call was made to a leader that is gone'));
    }
    if (TX_CALLS.has(op) && owners.get(args[0]) !== from) {
      return refusal(named('UnknownTransaction', `no open transaction ${args[0]} of this tab`));
    }
    try {
      const result = store[op](...args);
      if (op === 'begin') {
        owners.set(result, from);
        if (!owned.has(from)) owned.set(from, new Set());
        owned.get(from).add(result);
      }
      return { result };
    } catch (error) {
      if (trapped(error)) throw error;
      return refusal(error);
    } finally {

      if (op === 'commit' || op === 'abort') {
        owners.delete(args[0]);
        owned.get(from)?.delete(args[0]);
      }
    }
  }

  function watch(other) {
    if (other === tab || watched.has(other)) return;
    watched.add(other);
    navigator.locks.request(`${prefix}:tab:${other}`, () => {
      watched.delete(other);
      if (closed) return;
      for (const id of owned.get(other) ?? []) {
        owners.delete(id);
        try {
          store.abort(id);
        } catch (error) {
          if (trapped(error)) close('Poisoned', String(error));
        }
      }
      owned.delete(other);
      replyTo.get(other)?.close();
      replyTo.delete(other);
    });
  }

  function toLeader(message) {
    const channel = new BroadcastChannel(`${prefix}:leader`);
    channel.postMessage(message);
    channel.close();
  }

  function replyChannel(other) {
    if (!replyTo.has(other)) replyTo.set(other, new BroadcastChannel(`${prefix}:tab:${other}`));
    return replyTo.get(other);
  }

  function onLeaderChannel({ data }) {
    if (closed) return;
    watch(data.tab);
    if (data.type === 'hello') {
      replyChannel(data.tab).postMessage({ type: 'term', term });
      return;
    }
    if (data.type !== 'call') return;
    let out;
    try {
      out = CALLS.has(data.op)
        ? serve(data.tab, data.term, data.op, data.args)
        : refusal(named('Internal', `no call named ${data.op}`));
    } catch (error) {
      close('Poisoned', String(error));
      return;
    }

    if (hooks.afterCommit && COMMITS.has(data.op) && !out.error && hooks.afterCommit()) {
      self.close();
      return;
    }
    replyChannel(data.tab).postMessage({ type: 'reply', rid: data.rid, ...out });
  }

  function endPending(now) {
    for (const [rid, call] of pending) {
      if (call.term === now) continue;
      pending.delete(rid);
      if (call.handle !== undefined) handles.delete(call.handle);
      toPage(call.id, refusal(COMMITS.has(call.op)
        ? named('InDoubt', 'the leader serving this commit is gone; a read now decides it')
        : named('LeaderChanged', 'the leader serving this call is gone; it did not complete')));
    }
  }

  function joined(answer) {
    if (!joining) return;
    if (answer.error) joining.reject(answer.error);
    else joining.resolve();
    joining = null;
  }

  function drain() {
    for (const call of waiting.splice(0)) route(call.id, call.op, call.args);
  }

  async function adopt(t) {
    if (t === term || (role !== 'joining' && role !== 'follower')) return;
    const { held = [] } = await navigator.locks.query();
    if (!held.some((lock) => lock.name === `${prefix}:term:${t}`)) return;
    if (t === term || (role !== 'joining' && role !== 'follower')) return;
    term = t;
    role = 'follower';
    endPending(t);
    joined(joining?.op === 'create'
      ? refusal(named('StoreExists', 'another tab serves this store'))
      : { result: null });
    drain();
  }

  function onTabChannel({ data }) {
    if (closed) return;
    if (data.type === 'term') {
      learning = learning.then(() => adopt(data.term)).catch(() => {});
    } else if (data.type === 'reply') {
      const call = pending.get(data.rid);
      if (!call) return;
      pending.delete(data.rid);
      finish(call.id, call.op, call.handle, data, call.term);
    }
  }

  async function elected(op, args) {
    if (closed) return;
    role = 'candidate';
    try {

      if (hooks.beforeOpen) await hooks.beforeOpen(args[0]);

      store = await whenFree(() => (op === 'create' && term === null
        ? Store.create(...args)
        : Store.open(args[0], op === 'create' ? args[2] : args[1])));
    } catch (error) {
      if (trapped(error)) throw error;
      close(error?.name ?? 'Internal', error?.message ?? String(error));
      return;
    }
    const t = crypto.randomUUID();
    await hold(`${prefix}:term:${t}`);
    term = t;
    role = 'leader';
    channels.leader = new BroadcastChannel(`${prefix}:leader`);
    channels.leader.onmessage = onLeaderChannel;
    endPending(t);
    channels.tabs.postMessage({ type: 'term', term: t });
    joined({ result: null });
    drain();
    await new Promise(() => {});
  }

  async function join(op, args) {

    role = 'joining';
    prefix = `varaq:${args[0]}`;
    await hold(`${prefix}:tab:${tab}`);
    channels = {
      tabs: new BroadcastChannel(`${prefix}:tabs`),
      mine: new BroadcastChannel(`${prefix}:tab:${tab}`),
    };
    channels.tabs.onmessage = onTabChannel;
    channels.mine.onmessage = onTabChannel;
    const answered = new Promise((resolve, reject) => {
      joining = { op, resolve, reject };
    });
    navigator.locks.request(prefix, () => elected(op, args));
    toLeader({ type: 'hello', tab });
    return answered;
  }

  function destroy(name) {
    return navigator.locks.request(`varaq:${name}`, { ifAvailable: true }, async (lock) => {
      if (lock === null) throw named('StoreBusy', 'a tab holds this store');
      await whenFree(() => Store.destroy(name));
    });
  }

  function finish(id, op, handle, out, callTerm) {
    if (op === 'begin' && !out.error) {
      const local = nextHandle++;
      handles.set(local, { term: callTerm, id: out.result });
      toPage(id, { result: local });
      return;
    }
    if (handle !== undefined && (op === 'commit' || op === 'abort' || GONE.has(out.error?.name))) {
      handles.delete(handle);
    }
    toPage(id, out);
  }

  function route(id, op, args) {
    if (closed) return toPage(id, refusal(closed));
    if (role === 'joining' || role === 'candidate') {
      waiting.push({ id, op, args });
      return;
    }
    let handle;
    if (TX_CALLS.has(op)) {
      handle = args[0];
      const h = handles.get(handle);
      if (!h) {
        return toPage(id, refusal(named('UnknownTransaction', `no open transaction ${handle}`)));
      }
      if (h.term !== term) {
        handles.delete(handle);
        return toPage(id, refusal(named('LeaderChanged', 'the transaction ended with its leader')));
      }
      args = [h.id, ...args.slice(1)];
    }
    if (role === 'leader') {
      let out;
      try {
        out = serve(tab, term, op, args);
      } catch (error) {
        toPage(id, refusal(named('Poisoned', String(error))));
        close('Poisoned', String(error));
        return;
      }
      finish(id, op, handle, out, term);
      return;
    }
    const rid = nextRid++;
    pending.set(rid, { id, op, handle, term });
    toLeader({ type: 'call', tab, term, rid, op, args });
  }

  self.addEventListener('message', async ({ data }) => {
    if (data?.lane !== undefined) return;
    const { id, op, args = [] } = data;
    if (closed) return toPage(id, refusal(closed));

    const failed = await ready;
    if (failed !== null) {
      return toPage(id, refusal(named('Poisoned', `the module did not load: ${failed}`)));
    }
    try {
      if (op === 'create' || op === 'open') {
        if (role !== 'idle') throw named('Internal', 'this Worker already has a store');
        await join(op, args);
        toPage(id, { result: null });
      } else if (op === 'destroy') {
        await destroy(args[0]);
        toPage(id, { result: null });
      } else if (op === 'trap' && role === 'leader' && typeof store.trap === 'function') {

        store.trap();
      } else if (op === 'trap_async' && typeof Store.trap_async === 'function') {
        await Store.trap_async();
      } else if (CALLS.has(op)) {
        if (role === 'idle') throw named('NoSuchStore', 'no store is open in this Worker');
        route(id, op, args);
      } else {
        throw named('Internal', `no call named ${op}`);
      }
    } catch (error) {
      if (trapped(error)) {
        toPage(id, refusal(named('Poisoned', String(error))));
        close('Poisoned', String(error));
        return;
      }
      toPage(id, refusal(error));
    }
  });
}
