// Node UI demo: see ../shared/run-ui-demo.mjs for the WinRT calls.
//
// Run with:
//   node ui-demo.js
const { Windows, toPromise, enableAutoPump } = require('../../nswinrt.js');

import('../shared/run-ui-demo.mjs')
  .then((m) => m.runUiDemo(`Node ${process.version} (V8)`, { Windows, toPromise, enableAutoPump }))
  .then(() => process.exit(0), (e) => { console.error(e); process.exit(1); });
