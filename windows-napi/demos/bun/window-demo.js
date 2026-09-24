// Bun window demo: see ../shared/run-window-demo.mjs for the WinRT calls; only the loading shim
// differs from index.js (there is none needed: Bun does the CJS interop for a plain ESM import).
import { Windows, native } from '../../nswinrt.js';
import { runWindowDemo } from '../shared/run-window-demo.mjs';

try {
  await runWindowDemo(`Bun ${Bun.version} (JavaScriptCore)`, { Windows, native }, { env: process.env });
  process.exit(0);
} catch (e) {
  console.error(e);
  process.exit(1);
}
