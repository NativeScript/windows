// Smoke test: load the .node addon and exercise its surface end to end (namespace proxy, a WinRT
// round-trip, the message pump, error plumbing).
const ns = require('../index.js');

console.log('exports:', Object.keys(ns));

const Windows = ns.getNamespace('Windows');
const obj = new Windows.Data.Json.JsonObject();
obj.SetNamedValue('n', Windows.Data.Json.JsonValue.CreateNumberValue(4));
const json = obj.Stringify();
console.log('json ->', json);
if (json !== '{"n":4}') {
  throw new Error(`unexpected JsonObject round-trip: ${json}`);
}

console.log('pumpMessages ->', ns.pumpMessages());
console.log('lastError ->', ns.lastError());
console.log('done');
