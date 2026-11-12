export class Varaq {
  #worker;
  #next = 1;
  #pending = new Map();
  #closed = null;

  constructor(worker) {
    this.#worker = worker;
    worker.onmessage = ({ data }) => {
      if (data.closed) {
        this.#close(data.closed);
        return;
      }
      const waiting = this.#pending.get(data.id);
      if (!waiting) return;
      this.#pending.delete(data.id);
      if (data.error) {
        const error = new Error(data.error.message);
        error.name = data.error.name;
        waiting.reject(error);
      } else {
        waiting.resolve(data.result);
      }
    };
  }

  #close(why) {
    this.#closed ??= why;
    for (const { reject } of this.#pending.values()) reject(closedError(this.#closed));
    this.#pending.clear();
  }

  #call(op, ...args) {
    if (this.#closed) return Promise.reject(closedError(this.#closed));
    const id = this.#next++;
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      this.#worker.postMessage({ id, op, args });
    });
  }

  static #start({ worker, workerUrl }) {
    return new Varaq(worker ?? new Worker(workerUrl, { type: 'module' }));
  }

  static async create(name, { workerUrl, worker, workers = 2, poolPages = 4096, ringBytes = 4 << 20 }) {
    const db = Varaq.#start({ worker, workerUrl });
    try {
      await db.#call('create', name, workers, poolPages, ringBytes);
    } catch (error) {

      db.close();
      throw error;
    }
    return db;
  }

  static async open(name, { workerUrl, worker, poolPages = 4096 }) {
    const db = Varaq.#start({ worker, workerUrl });
    try {
      await db.#call('open', name, poolPages);
    } catch (error) {

      db.close();
      throw error;
    }
    return db;
  }

  static async destroy(name, { workerUrl, worker }) {
    const db = Varaq.#start({ worker, workerUrl });
    try {
      await db.#call('destroy', name);
    } finally {
      db.close();
    }
  }

  get(key) { return this.#call('get', key); }
  put(key, value) { return this.#call('put', key, value); }
  delete(key) { return this.#call('delete', key); }
  scan(from, limit) { return this.#call('scan', from, limit); }

  async transaction(body) {
    const id = await this.#call('begin');
    const tx = {
      get: (key) => this.#call('tx_get', id, key),
      put: (key, value) => this.#call('tx_put', id, key, value),
      delete: (key) => this.#call('tx_delete', id, key),
      scan: (from, limit) => this.#call('tx_scan', id, from, limit),
    };
    let answer;
    try {
      answer = await body(tx);
    } catch (error) {
      await this.#call('abort', id).catch(() => {});
      throw error;
    }

    await this.#call('commit', id);
    return answer;
  }

  close() {
    this.#worker.terminate();
    this.#close({ name: 'Poisoned', message: 'this store\'s Worker was closed; open the store again' });
  }
}

function closedError({ name, message }) {
  const error = new Error(message);
  error.name = name;
  return error;
}
