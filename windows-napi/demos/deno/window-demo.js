// Deno window demo: see ../shared/run-window-demo.mjs for the WinRT calls; loads the local
// package via createRequire exactly like main.js does, since it isn't published to npm here.
//
// Run with:
//   deno run --allow-ffi --allow-read --allow-env --allow-write window-demo.js
import { createRequire } from 'node:module';
import { runWindowDemo } from '../shared/run-window-demo.mjs';

const require = createRequire(import.meta.url);
const { Windows, native } = require('../../nswinrt.js');

try {
  await runWindowDemo(`Deno ${Deno.version.deno} (V8)`, { Windows, native }, { env: Deno.env.toObject() });
  Deno.exit(0);
} catch (e) {
  console.error(e);
  Deno.exit(1);
}
