// ES-module smoke app for the standalone hosts' script mode:
//   nativescript-windows packages/test/esm/main.mjs
// Exercises the module runner (QuickJS/Hermes/JSC) or V8's module loader: named, default and
// namespace imports, live bindings, an import cycle, `export *`, import.meta, dynamic import and
// top-level await. Prints one line per check, then `esm: ok` or `esm: FAILED`.
import greet, { counter, increment } from './counter.mjs';
import * as star from './star.mjs';
import { fromA } from './cycle-a.mjs';

var failures = 0;
function check(name, actual, expected) {
  var ok = actual === expected;
  if (!ok) { failures++; }
  console.log('esm: ' + name + ' ' + (ok ? 'ok' : 'FAILED (got ' + String(actual) + ', expected ' + String(expected) + ')'));
}

check('default import', greet('esm'), 'hello esm');
increment();
increment();
check('live binding', counter, 2);
check('export star', star.twice(21), 42);
check('export star skips default', 'default' in star, false);
check('import cycle', fromA(), 'a sees b, b sees a');
check('import.meta.url', /^file:\/\/\/.*main\.mjs$/.test(import.meta.url), true);

var waited = await new Promise(function (resolve) { setTimeout(function () { resolve('late'); }, 10); });
check('top-level await', waited, 'late');

var lazy = await import('./lazy.mjs');
check('dynamic import', lazy.value, 'lazy value');
check('dynamic import after tla', lazy.settled, true);

var missing = await import('./nope.mjs').then(function () { return 'loaded'; }, function (e) { return String(e.message || e); });
check('missing module rejects', missing.indexOf("Cannot find module './nope.mjs'") >= 0, true);

console.log(failures === 0 ? 'esm: ok' : 'esm: FAILED');
