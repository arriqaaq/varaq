import init, { Store } from './pkg/web.js';
import { startWorker } from './share.js';

startWorker({ init, Store });
