// E2E: natural NativeScript-style WinRT syntax from Node via the napi ns_proxy layer —
// namespaces, `new`, static methods, instance methods, typed returns, proxies as arguments.
const ns = require('../index.js');

let pass = 0, fail = 0;
function check(name, got, expected) {
  const ok = typeof expected === 'function' ? expected(got) : Object.is(got, expected);
  if (ok) pass++;
  else { console.log(`FAIL ${name}: got ${JSON.stringify(got)}, expected ${String(expected)}`); fail++; }
}
function throws(name, fn) {
  try { fn(); console.log(`FAIL ${name}: did not throw`); fail++; }
  catch { pass++; }
}

const Windows = ns.getNamespace('Windows');

// Namespace walking, lazily via metadata.
check('namespace typeName', Windows.Data.Json.__typeName__, 'Windows.Data.Json');
check('unknown member', Windows.Data.Json.NoSuchThing, undefined);
check('has trait', 'Json' in Windows.Data, true);

// Resolved members are cached as read-only own properties (one object per name).
check('namespace identity', Windows.Data.Json === Windows.Data.Json, true);
check('enum identity', Windows.UI.Xaml.Visibility === Windows.UI.Xaml.Visibility, true);
check('cached own property', Object.prototype.hasOwnProperty.call(Windows.Data, 'Json'), true);
check('not enumerable', Object.keys(Windows.Data).length, 0);
Windows.Data.Json = 5;
check('read-only', typeof Windows.Data.Json, 'object');

const { JsonObject, JsonValue } = Windows.Data.Json;
check('class typeName', JsonObject.__typeName__, 'Windows.Data.Json.JsonObject');

// Parameterless construction through IActivationFactory.
const obj = new JsonObject();
check('new JsonObject()', obj.__typeName__, 'Windows.Data.Json.JsonObject');
check('toString', String(obj.toString()), 'Windows.Data.Json.JsonObject');

// Static method on a ctor proxy → typed instance proxy return (GetRuntimeClassName wrap).
const five = JsonValue.CreateNumberValue(5);
check('static returns typed proxy', five.__typeName__, 'Windows.Data.Json.JsonValue');
check('instance method on typed return', five.GetNumber(), 5);

// Instance proxy passed AS AN ARGUMENT — marshals via the get-trap `handle` external
// through napi_parse_query_interface (IJsonValue parameter).
obj.SetNamedValue('a', five);
check('GetNamedNumber after set', obj.GetNamedNumber('a'), 5);
obj.SetNamedValue('s', JsonValue.CreateStringValue('héllo'));
check('GetNamedString unicode', obj.GetNamedString('s'), 'héllo');
check('Stringify', obj.Stringify(), s => s.includes('"a"') && s.includes('5') && s.includes('"s"'));

// Typed return chain: Parse → JsonObject proxy → methods work on it.
const parsed = JsonObject.Parse('{"x": 2.5}');
check('Parse returns typed proxy', parsed.__typeName__, 'Windows.Data.Json.JsonObject');
check('chained GetNamedNumber', parsed.GetNamedNumber('x'), 2.5);

// Method identity across the boundary: unknown members are undefined, not errors.
check('unknown instance member', obj.notAMethod, undefined);
check('has method', 'Stringify' in obj, true);
check('has unknown', 'nope' in obj, false);

// Errors: bad JSON HRESULT → JS exception; calling ctor without new → error.
throws('Parse invalid throws', () => JsonObject.Parse('nope'));
throws('ctor without new throws', () => JsonObject());
throws('ctor arity mismatch throws', () => new JsonObject('x'));

// console.log of a proxy must not crash (symbol probes hit the get trap).
const before = pass + fail;
void `${obj}`;
check('template literal of proxy', true, true);

// Enums: plain JS objects of name → numeric value.
const { JsonValueType } = Windows.Data.Json;
check('enum member Number', JsonValueType.Number, 2);
check('enum member Object', JsonValueType.Object, 5);
check('enum member Null', JsonValueType.Null, 0);

// Instance property getter with an enum return type (ValueType → JsonValueType).
check('property get enum', five.ValueType, JsonValueType.Number);
check('property get enum (string value)', JsonValue.CreateStringValue('x').ValueType, JsonValueType.String);

// Read/write instance property (Calendar.Year is a get/set Int32).
const Calendar = ns.getNamespace('Windows').Globalization.Calendar;
const cal = new Calendar();
const y = cal.Year;
check('property get i32', typeof y, 'number');
cal.Year = 2000;
check('property set round-trip', cal.Year, 2000);

// String property getter (Calendar.NumeralSystem).
check('property get string', typeof cal.NumeralSystem, 'string');

// Events: wire/read/replace/unsubscribe through the real add_/remove_ ABI methods
// (token round-trips through COM; the Invoke path is covered by delegate-test.js).
const { MediaPlayer } = ns.getNamespace('Windows').Media.Playback;
const mp = new MediaPlayer();
check('event unset reads null', mp.MediaEnded, null);
const h1 = () => {};
mp.MediaEnded = h1;
check('event reads back handler', mp.MediaEnded === h1, true);
const h2 = () => {};
mp.MediaEnded = h2;
check('event replace', mp.MediaEnded === h2, true);
mp.MediaEnded = null;
check('event unsubscribe', mp.MediaEnded, null);

// Instance identity: the same underlying COM object yields the same JS proxy (===).
const idObj = JsonObject.Parse('{}');
idObj.SetNamedValue('k', JsonValue.CreateNumberValue(9));
const g1 = idObj.GetNamedValue('k');
const g2 = idObj.GetNamedValue('k');
check('instance identity ===', g1 === g2, true);
check('identity proxy usable', g1.GetNumber(), 9);

// Parameterized constructors: Uri(String) and Uri(String, String) via the class factory.
const Uri = ns.getNamespace('Windows').Foundation.Uri;
const uri = new Uri('http://example.com/p?q=1');
check('param ctor typeName', uri.__typeName__, 'Windows.Foundation.Uri');
check('param ctor property', uri.Host, 'example.com');
check('param ctor 2 args', new Uri('http://example.com', '/x').AbsoluteUri, 'http://example.com/x');

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
