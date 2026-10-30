// Standalone-engine check that untrusted-shaped values can't reach native code as pointers or
// mistyped objects, and that the safe-looking APIs handle views, NULs and randomness correctly:
//   nativescript-windows packages/test/security-check.js
// Prints "security-check: N passed, M failed"; throws (nonzero exit) on any failure.
'use strict';

var pass = 0, fail = 0;
function check(name, ok) {
  if (ok) { pass += 1; }
  else { fail += 1; console.error('FAIL ' + name); }
}
function throws(fn) {
  try { fn(); } catch (_) { return true; }
  return false;
}

// JSON can carry numbers and strings, which must never be read as native pointers.
var ps = new Windows.Foundation.Collections.PropertySet();
ps.Insert('ok', 'a string boxes fine');
check('string boxes', ps.Lookup('ok') === 'a string boxes fine');
['{"handle": 1094795585}', '{"handle": "0x41414141"}', '{"__native_ptr": "0x4141414141"}',
 '{"__native_ptr": 1094795585}'].forEach(function (json) {
  check('rejects ' + json, throws(function () { ps.Insert('k', JSON.parse(json)); }));
});

// Interface parameters only take objects implementing the interface.
var client = new Windows.Web.Http.HttpClient();
client.GetStringAsync(new Windows.Foundation.Uri('https://example.invalid/')).Cancel();
check('string is not a Uri', throws(function () { client.GetStringAsync('https://example.com'); }));
check('number is not a Uri', throws(function () { client.GetStringAsync(42); }));
check('PropertySet is not a Uri', throws(function () { client.GetStringAsync(new Windows.Foundation.Collections.PropertySet()); }));

// Struct buffers must cover the struct.
var PV = Windows.Foundation.PropertyValue;
check('8-byte Point accepted', !throws(function () { PV.CreatePoint(new Float32Array([1, 2])); }));
check('2-byte Point rejected', throws(function () { PV.CreatePoint(new Uint8Array(2)); }));

// Delegate parameters only take delegates.
var pool = Windows.System.Threading.ThreadPool;
check('function delegate', !throws(function () { pool.RunAsync(function () {}); }));
check('asDelegate handle', !throws(function () {
  pool.RunAsync(NSWinRT.asDelegate('Windows.System.Threading.WorkItemHandler', function () {}));
}));
check('PropertySet is not a delegate', throws(function () { pool.RunAsync(new Windows.Foundation.Collections.PropertySet()); }));

// A view's bytes start at its byteOffset.
var CB = Windows.Security.Cryptography.CryptographicBuffer;
var bytes = new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8]);
var hex = CB.EncodeToHexString(CB.CreateFromByteArray(bytes.subarray(4)));
check('view offset honored (' + hex + ')', hex === '05060708');

// Strings keep embedded NULs.
var withNul = Windows.Data.Json.JsonValue.CreateStringValue('a\u0000b').GetString();
check('embedded NUL kept', withNul.length === 3);

// Web Crypto randomness.
var r1 = crypto.getRandomValues(new Uint8Array(32));
var r2 = crypto.getRandomValues(new Uint8Array(32));
check('random fills differ', r1.some(function (v, i) { return v !== r2[i]; }));
check('float array rejected', throws(function () { crypto.getRandomValues(new Float32Array(2)); }));
check('quota enforced', throws(function () { crypto.getRandomValues(new Uint8Array(65537)); }));
check('randomUUID v4', /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(crypto.randomUUID()));

console.log('security-check: ' + pass + ' passed, ' + fail + ' failed');
if (fail) { throw new Error('security-check: ' + fail + ' failed'); }
