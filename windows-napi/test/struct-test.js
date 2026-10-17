// E2E: WinRT value-struct returns marshaled into plain JS objects (with correct alignment),
// and round-tripped back as arguments through the struct serializer.
const ns = require('../index.js');

let pass = 0, fail = 0;
function check(name, got, expected) {
  const ok = typeof expected === 'function' ? expected(got) : Object.is(got, expected);
  if (ok) pass++;
  else { console.log(`FAIL ${name}: got ${JSON.stringify(got)}, expected ${String(expected)}`); fail++; }
}

const Windows = ns.getNamespace('Windows');

// ColorHelper.FromArgb(a,r,g,b) → Windows.UI.Color struct { A,R,G,B: u8 }.
const ColorHelper = Windows.UI.ColorHelper;
const color = ColorHelper.FromArgb(255, 128, 64, 32);
check('struct return is object', typeof color, 'object');
check('struct field A', color.A, 255);
check('struct field R', color.R, 128);
check('struct field G', color.G, 64);
check('struct field B', color.B, 32);

// Static color property returning a Color struct.
const red = Windows.UI.Colors.Red;
check('Colors.Red A', red.A, 255);
check('Colors.Red R', red.R, 255);
check('Colors.Red G', red.G, 0);
check('Colors.Red B', red.B, 0);

// GlobalizationPreferences-free struct via GridLength-like: use Point round-trip through a
// method that accepts a struct arg. PointHelper isn't universally present, so validate the
// object shape is spreadable/serializable (proves it's a real plain object, not a proxy).
check('struct spreads', JSON.stringify({ ...color }), '{"A":255,"R":128,"G":64,"B":32}');
check('struct JSON', JSON.stringify(color), s => s.includes('"A":255'));

// Struct constructors from namespaces; missing fields are zero.
const F = Windows.Foundation;
check('struct ctor named', JSON.stringify(new F.Rect({ X: 1, Y: 2, Width: 3, Height: 4 })), '{"X":1,"Y":2,"Width":3,"Height":4}');
check('struct ctor positional', JSON.stringify(new F.Rect(5, 6, 7, 8)), '{"X":5,"Y":6,"Width":7,"Height":8}');
check('struct ctor empty', JSON.stringify(new F.Rect()), '{"X":0,"Y":0,"Width":0,"Height":0}');
check('struct ctor partial', JSON.stringify(new F.Rect({ X: 1 })), '{"X":1,"Y":0,"Width":0,"Height":0}');
check('struct ctor nested', JSON.stringify(new Windows.UI.Xaml.Duration({ TimeSpan: { Duration: 5 }, Type: 1 })), '{"TimeSpan":{"Duration":5},"Type":1}');

// Point passed by value round-trips.
const inkPoint = new Windows.UI.Input.Inking.InkPoint(new F.Point(3, 4), 0.5);
check('by-value Point', JSON.stringify(inkPoint.Position), '{"X":3,"Y":4}');

// Boxed structs (IReference<Point> etc.) unbox to plain objects and box back by shape.
const PV = F.PropertyValue;
check('PropertyValue.CreatePoint', JSON.stringify(PV.CreatePoint({ X: 1.5, Y: 2.5 })), '{"X":1.5,"Y":2.5}');
const ps = new F.Collections.PropertySet();
ps.Insert('size', PV.CreateSize({ Width: 9, Height: 8 }));
ps.Insert('point', { X: 1, Y: 2 });
ps.Insert('rect', new F.Rect(1, 2, 3, 4));
ps.Insert('span', { Duration: 10000000 });
check('boxed Size', JSON.stringify(ps.Lookup('size')), '{"Width":9,"Height":8}');
check('boxed Point', JSON.stringify(ps.Lookup('point')), '{"X":1,"Y":2}');
check('boxed Rect', JSON.stringify(ps.Lookup('rect')), '{"X":1,"Y":2,"Width":3,"Height":4}');
check('boxed TimeSpan', JSON.stringify(ps.Lookup('span')), '{"Duration":10000000}');

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
