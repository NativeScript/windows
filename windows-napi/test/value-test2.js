// Slice-2 parity tests: pointer/handle extraction, PropertyValue boxing, buffer/struct
// parsing, write/read native slots, out-params, struct field bytes.
const ns = require('../index.js');

// PropertyValue boxing needs WinRT initialized on this thread (installInterop brings it up).
ns.installInterop();

let pass = 0, fail = 0;
function check(name, got, expected) {
  const ok = typeof expected === 'function' ? expected(got) : Object.is(got, expected);
  if (ok) pass++;
  else { console.log(`FAIL ${name}: got ${String(got)}, expected ${String(expected)}`); fail++; }
}
function throws(name, fn) {
  try { fn(); console.log(`FAIL ${name}: did not throw`); fail++; }
  catch { pass++; }
}

// pointer parsing: null/undefined -> 0
check('ptr(null)', ns.pointerValue(null), 0);
check('ptr(undefined)', ns.pointerValue(undefined), 0);
// handle property forms
check('ptr({handle:null})', ns.pointerValue({ handle: null }), 0);
check('ptr({handle:123})', ns.pointerValue({ handle: 123 }), 123);
check('ptr({handle:321n})', ns.pointerValue({ handle: 321n }), 321);
check('ptr({handle:fn->77})', ns.pointerValue({ handle: () => 77 }), 77);
check('ptr({handle:fn->null})', ns.pointerValue({ handle: () => null }), 0);
// external round-trip, direct and via handle
const ext = ns.makeExternal(4096);
check('ptr(external)', ns.pointerValue(ext), 4096);
check('ptr({handle:external})', ns.pointerValue({ handle: ext }), 4096);
// __native_ptr forms
check('ptr({__native_ptr:"0x10"})', ns.pointerValue({ __native_ptr: '0x10' }), 16);
check('ptr({__native_ptr:"42"})', ns.pointerValue({ __native_ptr: '42' }), 42);
check('ptr({__native_ptr:99})', ns.pointerValue({ __native_ptr: 99 }), 99);
check('ptr({__native_ptr:7n})', ns.pointerValue({ __native_ptr: 7n }), 7);
// primitive boxing to IPropertyValue (nonzero COM pointers)
check('ptr("hello") boxed', ns.pointerValue('hello'), p => p > 0);
check('ptr(5) boxed Int32', ns.pointerValue(5), p => p > 0);
check('ptr(5.5) boxed Double', ns.pointerValue(5.5), p => p > 0);
check('ptr(true) boxed Boolean', ns.pointerValue(true), p => p > 0);
// invalid
throws('ptr(Symbol) throws', () => ns.pointerValue(Symbol('x')));

// buffers
const ab = new ArrayBuffer(16);
const abPtr = ns.bufferInfo(ab)[0];
check('buffer(ab).len', ns.bufferInfo(ab)[1], 16);
check('buffer(ab).ptr', abPtr, p => p > 0);
const u8 = new Uint8Array(ab, 4, 8);
check('view(u8,4,8).len', ns.bufferInfo(u8)[1], 8);
check('view(u8,4,8).ptr = ab+4', ns.bufferInfo(u8)[0], abPtr + 4);
const f64arr = new Float64Array(3);
check('view(f64x3).len bytes', ns.bufferInfo(f64arr)[1], 24);
const dv = new DataView(ab, 2, 5);
check('dataview(2,5).len', ns.bufferInfo(dv)[1], 5);
check('dataview(2,5).ptr = ab+2', ns.bufferInfo(dv)[0], abPtr + 2);
check('buffer(null)', ns.bufferInfo(null)[1], 0);
throws('buffer("x") throws', () => ns.bufferInfo('x'));
throws('buffer(undefined) throws', () => ns.bufferInfo(undefined));

// structs
check('struct(ab).ptr', ns.structPtr(ab), abPtr);
check('struct(view+4).ptr', ns.structPtr(u8), abPtr + 4);
throws('struct(null) throws', () => ns.structPtr(null));
throws('struct(5) throws', () => ns.structPtr(5));

// write -> read native slot round-trips
for (const [ty, v] of [['bool', true], ['u8', 200], ['i8', -5], ['u16', 60000], ['i16', -12345],
                       ['u32', 4000000000], ['i32', -2000000000], ['f64', 3.25], ['string', 'héllo✓']]) {
  check(`writeRead ${ty}`, ns.writeReadPtr(v, ty), v);
}
check('writeRead f32', ns.writeReadPtr(0.1, 'f32'), Math.fround(0.1));
check('writeRead u64 big', ns.writeReadPtr(2n ** 60n, 'u64'), b => b === 2n ** 60n);
check('writeRead i64 neg', ns.writeReadPtr(-42, 'i64'), -42);
check('writeRead pointer null', ns.writeReadPtr(null, 'pointer'), null);

// out-params
check('out unwraps', ns.outParamValue({ __nswinrt_out_param__: true, value: 7 }), 7);
check('out marker truthy string', ns.outParamValue({ __nswinrt_out_param__: 'yes', value: 3 }), 3);
check('out plain object', ns.outParamValue({ value: 7 }), '<none>');
check('out non-object', ns.outParamValue(42), '<none>');
const wrapper = { __nswinrt_out_param__: true, value: 1 };
check('setOutParam ok', ns.setOutParam(wrapper, 99), true);
check('setOutParam wrote', wrapper.value, 99);

// struct field bytes (little-endian)
check('bytes f32 0.5', JSON.stringify(ns.structFieldBytes(0.5, 'f32')), JSON.stringify([0, 0, 0, 63]));
check('bytes u8 255', JSON.stringify(ns.structFieldBytes(255, 'u8')), JSON.stringify([255]));
check('bytes bool true', JSON.stringify(ns.structFieldBytes(true, 'bool')), JSON.stringify([1]));
check('bytes i64 2^60', JSON.stringify(ns.structFieldBytes(2n ** 60n, 'i64')),
      JSON.stringify([0, 0, 0, 0, 0, 0, 0, 16]));
check('bytes i32 -1', JSON.stringify(ns.structFieldBytes(-1, 'i32')), JSON.stringify([255, 255, 255, 255]));

// PropertyValue boxing by type name
for (const [v, ty] of [[42.5, 'Double'], [42, 'Int32'], [7, 'UInt8'], ['x', 'Char16'],
                       [true, 'Boolean'], ['hi', 'String'], [123456789, 'DateTime'],
                       [1500, 'TimeSpan'], [2n ** 60n, 'Int64'],
                       ['74686973-6973-6e6f-7461-677569640000', 'Guid'],
                       [42, 'Windows.Foundation.Double']]) {
  check(`box ${ty}`, ns.boxTyped(v, ty), p => p > 0);
}
check('box TimeSpan struct', ns.boxTyped({ Duration: 10000n }, 'TimeSpan'), p => p > 0);
check('box DateTime struct', ns.boxTyped({ UniversalTime: 2n ** 60n }, 'DateTime'), p => p > 0);
check('box unknown type', ns.boxTyped(1, 'NoSuchType'), 0);
check('box Guid invalid', ns.boxTyped('not-a-guid', 'Guid'), 0);

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
