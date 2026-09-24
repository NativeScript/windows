// Deno demo: Deno implements Node-API behind its Node-compat layer. The package isn't
// published to npm here, so instead of an `npm:` specifier we load the local `nswinrt.js`
// (plain CommonJS) via `createRequire`. The same "ESM via createRequire" path the docs call
// out for Node. See ../../../docs/napi-consumption.md.
//
// Run with:
//   deno run --allow-ffi --allow-read --allow-env --allow-write main.js
// --allow-ffi is required to load the native addon; the rest cover winmd/interop bookkeeping.
import { createRequire } from 'node:module';
import { runDemo } from '../shared/run-demo.mjs';

const require = createRequire(import.meta.url);
const { Windows, toPromise, enableAutoPump, interop, native } = require('../../nswinrt.js');

await runDemo(`Deno ${Deno.version.deno} (V8)`, { Windows, toPromise, enableAutoPump, interop, native });
