// Node window demo: see ../shared/run-window-demo.mjs for the WinRT calls.
//
// Run with:
//   node window-demo.js
const { Windows, native } = require('../../nswinrt.js');

import('../shared/run-window-demo.mjs')
  .then((m) => m.runWindowDemo(`Node ${process.version} (V8)`, { Windows, native }, { env: process.env }))
  .then(() => process.exit(0), (e) => { console.error(e); process.exit(1); });
