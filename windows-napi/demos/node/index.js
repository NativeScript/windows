// Node console demo: the baseline runtime. `nswinrt.js` is plain CommonJS, so it loads with a
// normal require(); the shared demo body is ESM, reached through dynamic import().
//
// Run with:
//   node index.js
const { Windows, toPromise, enableAutoPump, interop, native } = require('../../nswinrt.js');

import('../shared/run-demo.mjs')
  .then((m) => m.runDemo(`Node ${process.version} (V8)`, { Windows, toPromise, enableAutoPump, interop, native }))
  .then(() => process.exit(0), (e) => { console.error(e); process.exit(1); });
