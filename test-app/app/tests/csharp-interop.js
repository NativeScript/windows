// C# interop: fields, nested types, longs and binary data, parameters, generics, extension methods,
// collections, events, exceptions, object identity and JS classes created by .NET code.
// Fixtures: App_Resources/Windows/src/TestFixtures/Interop.cs.

const I = () => TestFixtures.Interop;

describe("C# fields, constants and enums", () => {
	it("reads constants and static readonly fields", () => {
		expect(I().Fields.Label).toBe("fields");
		expect(I().Fields.Answer).toBe(42);
	});

	it("reads and writes static fields", () => {
		I().Fields.Counter = 5;
		expect(I().Fields.Counter).toBe(5);
	});

	it("reads and writes instance fields", () => {
		const f = new (I().Fields)();
		expect(f.Value).toBe(7);
		f.Value = 21;
		expect(f.Value).toBe(21);
		expect(f.Twice()).toBe(42);
	});

	it("refuses to assign read-only fields", () => {
		const f = new (I().Fields)();
		expect(f.Id).toBe("id-1");
		expect(() => (f.Id = "x")).toThrow();
		expect(() => (I().Fields.Answer = 1)).toThrow();
	});

	it("exposes enum members as their values", () => {
		expect(I().Tint.Red).toBe(1);
		expect(I().Tint.Blue).toBe(4);
		expect(I().Access.Read | I().Access.Write).toBe(3);
		expect(new (I().Fields)().Tint).toBe(I().Tint.Green);
	});

	it("reads value tuple items and struct fields", () => {
		const pair = I().Values.Pair();
		expect(pair.Item1).toBe(1);
		expect(pair.Item2).toBe("one");
		const d = I().Values.Make(3, 4);
		expect(d.Width).toBe(3);
		expect(d.Area).toBe(12);
	});
});

describe("C# nested types", () => {
	it("reaches nested classes and enums through their outer type", () => {
		expect(I().Outer.Inner.Where()).toBe("inner");
		expect(new (I().Outer.Inner)().Hello()).toBe("hi");
		expect(I().Outer.Kind.Second).toBe(1);
	});
});

describe("C# long values and binary data", () => {
	it("returns longs that fit a double as numbers", () => {
		expect(I().Numbers.Small()).toBe(42);
	});

	it("returns larger longs as BigInt, without losing digits", () => {
		expect(I().Numbers.Big()).toBe(9007199254740993n);
		expect(I().Numbers.Huge()).toBe(18446744073709551615n);
	});

	it("passes BigInt arguments as longs", () => {
		expect(I().Numbers.Echo(9007199254740993n)).toBe(9007199254740993n);
		expect(I().Numbers.Text(-9007199254740993n)).toBe("-9007199254740993");
		expect(I().Numbers.EchoUnsigned(18446744073709551615n)).toBe(18446744073709551615n);
		expect(I().Numbers.Describe(5n)).toBe("long:5");
		expect(I().Numbers.Describe(5)).toBe("int:5");
	});

	it("returns byte[] as a Uint8Array", () => {
		const bytes = I().Numbers.Bytes();
		expect(bytes instanceof Uint8Array).toBe(true);
		expect(Array.from(bytes)).toEqual([1, 2, 255]);
	});

	it("keeps other arrays as JS arrays", () => {
		expect(I().Numbers.Ints()).toEqual([1, 2, 3]);
	});

	it("passes typed arrays, ArrayBuffers and DataViews as arrays", () => {
		expect(I().Numbers.Sum(new Uint8Array([1, 2, 3]))).toBe(6);
		expect(I().Numbers.Sum(new Uint8Array([4, 5]).buffer)).toBe(9);
		expect(I().Numbers.Sum(new DataView(new Uint8Array([7]).buffer))).toBe(7);
		expect(I().Numbers.SumInts(new Int32Array([100000, 200000]))).toBe(300000);
		expect(I().Numbers.SumDoubles(new Float64Array([0.5, 0.25]))).toBe(0.75);
	});

	it("passes bytes as Memory<byte> and Stream", () => {
		expect(I().Numbers.Length(new Uint8Array(5))).toBe(5);
		expect(I().Numbers.StreamLength(new Uint8Array(3))).toBe(3);
	});
});

describe("C# parameters", () => {
	it("fills in optional parameters", () => {
		expect(I().Params.Optional("a")).toBe("aB3");
		expect(I().Params.Optional("a", "x")).toBe("ax3");
		expect(I().Params.Tinted()).toBe("Blue");
	});

	it("passes arguments by name", () => {
		expect(I().Params.Point(NSWinRT.dotnet.named({ y: 2, label: "q" }))).toBe("q(0,2)");
		expect(I().Params.Point(1, NSWinRT.dotnet.named({ label: "r" }))).toBe("r(1,0)");
	});

	it("fills in optional constructor parameters", () => {
		const s = new (I().Sized)("n");
		expect(s.Size).toBe(10);
		expect(new (I().Sized)("n", NSWinRT.dotnet.named({ size: 2 })).Size).toBe(2);
	});

	it("returns out parameters that are left off with the result", () => {
		expect(I().Params.TryDouble("21")).toEqual([true, 42]);
		expect(I().Params.TryDouble("x")).toEqual([false, 0]);
		expect(I().Params.Split("Ada Lovelace")).toEqual(["Ada", "Lovelace"]);
	});

	it("fills in out and ref references", () => {
		const result = NSWinRT.interop.out();
		expect(I().Params.TryDouble("5", result)).toBe(true);
		expect(result.value).toBe(10);

		const a = NSWinRT.interop.out(1);
		const b = NSWinRT.interop.out(2);
		I().Params.Swap(a, b);
		expect([a.value, b.value]).toEqual([2, 1]);
	});

	it("reaches BCL methods with out parameters", () => {
		const parsed = NSWinRT.interop.out();
		expect(System.Int32.TryParse("123", parsed)).toBe(true);
		expect(parsed.value).toBe(123);
	});
});

describe("C# generics", () => {
	it("constructs closed generic types with .of()", () => {
		const List = System.Collections.Generic.List.of(System.String);
		const list = new List();
		list.Add("a");
		list.Add("b");
		expect(list.Count).toBe(2);
		expect(I().Generics.Describe(list)).toBe("String:2");
	});

	it("takes JS constructors as type arguments", () => {
		const Dict = System.Collections.Generic.Dictionary.of(String, Number);
		const d = new Dict();
		d.Add("x", 1.5);
		expect(d.get_Item("x")).toBe(1.5);
	});

	it("constructs app generic types", () => {
		const box = new (I().Box.of(System.Int32))(5);
		expect(box.Value).toBe(5);
		expect(box.TypeArgument).toBe("Int32");
	});

	it("infers generic method type arguments from the arguments", () => {
		expect(I().Generics.Identity("same")).toBe("same");
		const list = I().Generics.Repeat("z", 3);
		expect(list.Count).toBe(3);
		expect(I().Generics.CountOf(list)).toBe(3);
		expect(I().Generics.CountOf([1, 2, 3, 4])).toBe(4);
	});

	it("calls generic methods with explicit type arguments", () => {
		expect(I().Generics.NameOf.of(System.DateTime)()).toBe("DateTime");
		expect(System.Linq.Enumerable.Empty.of(System.String)().length).toBe(0);
	});
});

describe("C# extension methods", () => {
	it("calls extension methods on instances", () => {
		const f = new (I().Fields)();
		expect(f.Thrice()).toBe(21);
	});

	it("calls LINQ on .NET collections", () => {
		const list = I().Generics.Repeat(2, 3);
		expect(list.Sum()).toBe(6);
		expect(list.Select((x) => x * 10).ToList().get_Item(1)).toBe(20);
		expect(list.Where((x) => x > 1).Count()).toBe(3);
	});
});

describe("C# collections", () => {
	it("reads and writes string-keyed dictionaries by key", () => {
		const scores = I().Registry.Shared.Scores;
		expect(scores.alice).toBe(3);
		scores.bob = 5;
		expect(scores["bob"]).toBe(5);
		expect(scores.Count).toBe(2);
		expect(scores.ContainsKey("bob")).toBe(true);
	});
});

describe("C# events", () => {
	it("subscribes and unsubscribes JS functions with add_/remove_", () => {
		const t = new (I().Talker)();
		const seen = [];
		const handler = (sender, message) => seen.push(message);
		t.add_Said(handler);
		expect(t.Say("one")).toBe(1);
		t.remove_Said(handler);
		expect(t.Say("two")).toBe(0);
		expect(seen).toEqual(["one"]);
	});

	it("replaces the handler assigned to an event", () => {
		const t = new (I().Talker)();
		const seen = [];
		t.Said = (sender, m) => seen.push("a:" + m);
		t.Said = (sender, m) => seen.push("b:" + m);
		expect(t.Say("x")).toBe(1);
		expect(typeof t.Said).toBe("function");
		t.Said = null;
		expect(t.Handlers).toBe(0);
		expect(seen).toEqual(["b:x"]);
	});

	it("handles static events", () => {
		const seen = [];
		const handler = (n) => seen.push(n);
		I().Talker.add_Ticked(handler);
		expect(I().Talker.Tick(3)).toBe(1);
		I().Talker.remove_Ticked(handler);
		expect(I().Talker.Tick(4)).toBe(0);
		I().Talker.Ticked = (n) => seen.push(-n);
		I().Talker.Tick(5);
		I().Talker.Ticked = null;
		expect(seen).toEqual([3, -5]);
	});

	it("passes the sender as the same JS object", () => {
		const t = new (I().Talker)();
		let sender = null;
		t.Said = (s) => (sender = s);
		t.Say("x");
		expect(sender).toBe(t);
		t.Said = null;
	});
});

describe("C# exceptions", () => {
	it("throws a JS Error with the .NET exception", () => {
		let error = null;
		try {
			I().Failing.Throw("bad state");
		} catch (e) {
			error = e;
		}
		expect(error instanceof Error).toBe(true);
		expect(error.message).toBe("bad state");
		expect(error.dotnetType).toBe("System.InvalidOperationException");
		expect(error.nativeException.Message).toBe("bad state");
		expect(error.stack).toContain("Failing.Throw");
	});

	it("exposes inner exceptions", () => {
		let error = null;
		try {
			I().Failing.ThrowWithInner();
		} catch (e) {
			error = e;
		}
		expect(error.nativeException.InnerException.Message).toBe("inner");
	});

	it("rejects faulted Tasks with an Error", async () => {
		let error = null;
		try {
			await I().Failing.FailAsync("late");
		} catch (e) {
			error = e;
		}
		expect(error instanceof Error).toBe(true);
		expect(error.message).toBe("late");
		expect(error.dotnetType).toBe("System.FormatException");
	});

	it("hands JS errors to the C# caller with their stack", () => {
		const outcome = I().Failing.Catch(function failingCallback() {
			throw new Error("from js");
		});
		expect(outcome).toContain("from js");
		expect(outcome).toContain("failingCallback");
	});

	it("throws JS errors into C# from void callbacks too", () => {
		expect(I().Failing.CatchVoid(() => {
			throw new Error("void js");
		})).toContain("void js");
	});
});

describe("C# objects in JS", () => {
	it("returns the same JS object for the same .NET object", () => {
		const r = I().Registry.Shared;
		expect(r.Self()).toBe(r);
		expect(I().Registry.Shared).toBe(r);
		expect(r.Names).toBe(r.Names);
	});

	it("converts to strings with ToString()", () => {
		const b = new System.Text.StringBuilder("abc");
		expect(String(b)).toBe("abc");
		expect(`${new (I().Fields)()}`).toBe("TestFixtures.Interop.Fields");
	});
});

describe("C# subclasses", () => {
	it("reach protected members of the base class", () => {
		class Square extends I().Shape {
			get Kind() {
				return "square";
			}
			Draw() {
				this.Sides = 4;
				return this.Describe(2) + "," + this.Describe("x");
			}
		}
		const s = new Square();
		expect(s.Draw()).toBe("n2,sx");
		expect(s.Report()).toBe("square:4");
	});

	it("override generic virtual methods", () => {
		class Loud extends I().Shape {
			Draw() {
				return "";
			}
			Convert(value) {
				return value + "!";
			}
		}
		expect(new Loud().UseConvert()).toBe("x!");
	});
});

describe("JS classes created by .NET code", () => {
	class Robot extends I().Shape {
		init() {
			this.ready = true;
		}
		Draw() {
			return this.ready ? "robot" : "not ready";
		}
	}

	it("gives a JS class a .NET type named after it", () => {
		const type = NSWinRT.dotnet.typeOf(Robot);
		expect(type.FullName).toBe("NSWinRTDynamicProxies.Robot");
		expect(I().Factories.TypeName(new Robot())).toBe("NSWinRTDynamicProxies.Robot");
	});

	it("makes a JS object for an instance .NET code creates", () => {
		const made = I().Factories.Create(NSWinRT.dotnet.typeOf(Robot));
		expect(made instanceof Robot).toBe(true);
		expect(made.ready).toBe(true);
		expect(I().Factories.Draw(made)).toBe("robot");
	});

	it("lets .NET code find the type by name", () => {
		NSWinRT.dotnet.typeOf(Robot);
		const made = I().Factories.CreateByName("NSWinRTDynamicProxies.Robot, NSWinRTDynamicProxies");
		expect(made instanceof Robot).toBe(true);
		expect(made.Draw()).toBe("robot");
	});
});

describe("Loading assemblies", () => {
	it("loads an assembly at runtime and makes its types reachable", () => {
		const dir = System.Runtime.InteropServices.RuntimeEnvironment.GetRuntimeDirectory();
		const namespaces = NSWinRT.dotnet.loadAssembly(System.IO.Path.Combine(dir, "System.Web.HttpUtility.dll"));
		expect(namespaces).toContain("System.Web");
		expect(System.Web.HttpUtility.UrlEncode("a b")).toBe("a+b");
	});
});
