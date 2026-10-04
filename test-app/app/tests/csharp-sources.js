// C# sources dropped into App_Resources/Windows (or shipped in a plugin's platforms/windows) are
// compiled into the app and usable from JavaScript by namespace, like Java/Kotlin/Objective-C/
// Swift sources are on Android and iOS. Fixtures: App_Resources/Windows/src/TestFixtures/Basics.cs,
// plugins/test-plugin/platforms/windows/src/PluginGreeter.cs.

describe("C# sources in App_Resources", () => {
	it("exposes the root namespace of app C# types as a global without registration", () => {
		expect(typeof TestFixtures).not.toBe("undefined");
		expect(typeof TestFixtures.Basics.Greeter).toBe("function");
	});

	it("constructs with the default constructor", () => {
		const g = new TestFixtures.Basics.Greeter();
		expect(g.Name).toBe("World");
		expect(g.Greet()).toBe("Hello, World!");
	});

	it("constructs with arguments", () => {
		const g = new TestFixtures.Basics.Greeter("JS");
		expect(g.Greet()).toBe("Hello, JS!");
	});

	it("selects overloads by argument count", () => {
		const g = new TestFixtures.Basics.Greeter("JS");
		expect(g.Greet("C#")).toBe("Hello, C#, from JS!");
	});

	it("reads and writes instance properties", () => {
		const g = new TestFixtures.Basics.Greeter();
		g.Name = "Changed";
		expect(g.Name).toBe("Changed");
		expect(g.Greet()).toBe("Hello, Changed!");
	});

	it("calls static methods", () => {
		expect(TestFixtures.Basics.Greeter.Add(2, 3)).toBe(5);
		expect(TestFixtures.Basics.Greeter.Scale(1.5, 2)).toBe(3);
	});

	it("reads static properties", () => {
		expect(TestFixtures.Basics.Greeter.Version).toBe("1.0");
	});

	it("marshals arrays", () => {
		const g = new TestFixtures.Basics.Greeter();
		expect(g.Numbers()).toEqual([1, 2, 3]);
	});

	it("marshals enums as numbers", () => {
		const g = new TestFixtures.Basics.Greeter();
		expect(g.Mood).toBe(0);
		g.Mood = 10;
		expect(g.Mood).toBe(10);
	});

	it("resolves nested namespaces", () => {
		expect(TestFixtures.Basics.Nested.Deeper.Deep.Where()).toBe("deep");
	});

	it("reaches internal types", () => {
		expect(TestFixtures.Basics.InternalHelper.Ping()).toBe("pong");
	});

	it("passes JavaScript functions as Func<> delegates", () => {
		expect(TestFixtures.Basics.Callbacks.Apply((x) => x * 2, 21)).toBe(42);
		expect(TestFixtures.Basics.Callbacks.Join((a, b) => a + "+" + b, "x", "y")).toBe("x+y");
	});

	it("passes JavaScript functions as Action<> delegates", () => {
		const seen = [];
		const n = TestFixtures.Basics.Callbacks.Repeat((i) => seen.push(i), 3);
		expect(n).toBe(3);
		expect(seen).toEqual([0, 1, 2]);
	});

	it("subscribes to C# events with add_/remove_", () => {
		const g = new TestFixtures.Basics.Greeter();
		const messages = [];
		const handler = (sender, message) => messages.push(message);
		g.add_Greeted(handler);
		g.RaiseGreeted("one");
		expect(messages).toEqual(["one"]);
	});

	it("returns Task<T> as an awaitable", async () => {
		expect(await TestFixtures.Basics.Callbacks.AddLaterAsync(20, 22, 10)).toBe(42);
	});

	it("converts a Task to a promise with taskToPromise", async () => {
		const task = TestFixtures.Basics.Callbacks.AddLaterAsync(1, 2, 10);
		expect(await NSWinRT.dotnet.taskToPromise(task)).toBe(3);
		// The same Task can be awaited again.
		expect(await task).toBe(3);
	});

	it("does not block the UI thread while a Task runs", async () => {
		let timerRan = false;
		setTimeout(() => (timerRan = true), 0);
		const result = await TestFixtures.Basics.Callbacks.AddLaterAsync(1, 1, 200);
		expect(result).toBe(2);
		expect(timerRan).toBe(true);
	});

	it("rejects when a Task faults", async () => {
		let error = null;
		try {
			await TestFixtures.Basics.Callbacks.FailLaterAsync("boom", 10);
		} catch (e) {
			error = e;
		}
		expect(String(error && (error.message || error))).toContain("boom");
	});

	it("keeps System the .NET namespace when app code assigns it", () => {
		// What @nativescript/core does (a SystemJS shim).
		const load = (p) => p;
		globalThis.System = { import: load };
		expect(System.import).toBe(load);
		expect(System.IO.Path.Combine("a", "b")).toBe("a\\b");
	});

	it("throws a catchable JS error for a missing member", () => {
		const g = new TestFixtures.Basics.Greeter();
		expect(() => g.NoSuchMethod()).toThrow();
	});
});

describe("C# sources in plugins", () => {
	it("exposes plugin C# types by namespace", () => {
		expect(typeof TestPlugin).not.toBe("undefined");
		const g = new TestPlugin.Native.PluginGreeter();
		expect(g.Greet("app")).toBe("Hello from the plugin, app!");
		expect(TestPlugin.Native.PluginGreeter.PluginName).toBe("test-plugin");
	});
});
