// Deno UI demo: see ../shared/run-ui-demo.mjs for the WinRT calls; loads the local package via
// createRequire exactly like main.js does, since it isn't published to npm here.
//
// Run with:
//   deno run --allow-ffi --allow-read --allow-env --allow-write ui-demo.js
import { createRequire } from 'node:module';
import { runUiDemo } from '../shared/run-ui-demo.mjs';

const require = createRequire(import.meta.url);
const { Windows, toPromise, enableAutoPump } = require('../../nswinrt.js');

try {
  await runUiDemo(`Deno ${Deno.version.deno} (V8)`, { Windows, toPromise, enableAutoPump });
  Deno.exit(0);
} catch (e) {
  console.error(e);
  Deno.exit(1);
}
