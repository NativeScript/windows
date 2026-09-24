// Bun UI demo: see ../shared/run-ui-demo.mjs for the WinRT calls; only the loading shim differs
// from index.js (there is none needed: Bun does the CJS interop for a plain ESM import).
import { Windows, toPromise, enableAutoPump } from '../../nswinrt.js';
import { runUiDemo } from '../shared/run-ui-demo.mjs';

try {
  await runUiDemo(`Bun ${Bun.version} (JavaScriptCore)`, { Windows, toPromise, enableAutoPump });
  process.exit(0);
} catch (e) {
  console.error(e);
  process.exit(1);
}
