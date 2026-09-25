// Delegates whose Invoke parameters are declared as sealed runtime classes wrap those arguments
// straight from the declaration (no GetRuntimeClassName per invoke). Built with __nsAsDelegate
// over a closed generic delegate type, then fired through the real COM vtable.
const ns = require('../index.js');
ns.installGlobals();
const Windows = ns.getNamespace('Windows');
const { JsonObject, JsonValue, JsonArray } = Windows.Data.Json;

let pass = 0, fail = 0;
function check(name, ok, detail) {
  if (ok) pass++;
  else { console.log(`FAIL ${name}${detail === undefined ? '' : ': ' + detail}`); fail++; }
}

function delegatePtr(d) {
  return ns.pointerValue(d);
}

// Sealed sender and sealed args: both parameters take the declared-class path.
const jo = new JsonObject();
jo.SetNamedValue('k', JsonValue.CreateNumberValue(7));
const jv = JsonValue.CreateStringValue('hi');
let got = null;
const d = globalThis.__nsAsDelegate(
  'Windows.Foundation.TypedEventHandler`2<Windows.Data.Json.JsonObject, Windows.Data.Json.JsonValue>',
  (sender, args) => { got = { sender, args }; },
);
check('delegate created', d && typeof d === 'object');
const ptr = delegatePtr(d);
check('delegate pointer', ptr > 0);
check('invoke hr', ns.invokeDelegate(ptr, ns.pointerValue(jo), ns.pointerValue(jv), 0) === 0);
check('handler ran', got !== null);
check('sender is the same wrapper', got && got.sender === jo);
check('sender typed', got && got.sender.GetNamedNumber('k') === 7);
check('args typed', got && got.args.GetString() === 'hi');
check('args instanceof', got && got.args instanceof JsonValue);

// A fresh object that has no wrapper yet is wrapped on the way in.
const arr = new JsonArray();
arr.Append(JsonValue.CreateBooleanValue(true));
let fresh = null;
const d2 = globalThis.__nsAsDelegate(
  'Windows.Foundation.TypedEventHandler`2<Windows.Data.Json.JsonArray, Object>',
  (sender, args) => { fresh = { sender, args }; },
);
check('invoke hr (fresh)', ns.invokeDelegate(delegatePtr(d2), ns.pointerValue(arr), 0, 0) === 0);
check('fresh sender typed', fresh && fresh.sender.Size === 1);
check('Object arg null', fresh && fresh.args === null);

// Null for a sealed-class parameter stays null.
got = null;
check('invoke hr (null)', ns.invokeDelegate(ptr, 0, 0, 0) === 0);
check('null sender', got && got.sender === null && got.args === null);

// Repeated invocations do not leak or lose identity.
let n = 0;
const d3 = globalThis.__nsAsDelegate(
  'Windows.Foundation.TypedEventHandler`2<Windows.Data.Json.JsonObject, Windows.Data.Json.JsonValue>',
  (sender) => { if (sender === jo) n++; },
);
const p3 = delegatePtr(d3);
for (let i = 0; i < 1000; i++) ns.invokeDelegate(p3, ns.pointerValue(jo), ns.pointerValue(jv), 0);
check('1000 invokes keep identity', n === 1000, n);

console.log(`${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
