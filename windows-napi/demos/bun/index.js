// Bun demo: Bun implements Node-API, so it loads the .node addon and the CJS `nswinrt.js`
// helper layer through a plain ESM import. No shims, no flags. See ../../../docs/napi-consumption.md.
import { Windows, toPromise, enableAutoPump, interop, native } from '../../nswinrt.js';
import { runDemo } from '../shared/run-demo.mjs';

await runDemo(`Bun ${Bun.version} (JavaScriptCore)`, { Windows, toPromise, enableAutoPump, interop, native });
