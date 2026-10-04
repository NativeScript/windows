// Entry point of the Windows runtime test app. Mirrors the Android/iOS test-app layout: a list
// of spec files run by a Jasmine-compatible runner, with native fixtures compiled into the app
// (App_Resources/Windows/src, the counterpart of Android's src/main/java and iOS's TestFixtures)
// and a local plugin (plugins/test-plugin) that ships C# sources.

// The runtime exposes timers as __ns__* host functions; @nativescript/core installs the standard
// globals on top of them (as on iOS), and this app runs without core.
for (const name of ["setTimeout", "setInterval", "clearTimeout", "clearInterval"]) {
	if (typeof globalThis[name] !== "function") globalThis[name] = globalThis["__ns__" + name];
}

const { api, execute, toJUnitXml } = require("./runner/jasmine-lite");
Object.assign(globalThis, api);

const specs = [
	"./tests/csharp-sources.js",
	"./tests/extend-dotnet.js",
	"./tests/extend-winrt.js",
	"./tests/threading.js",
	"./tests/boxing.js",
	"./tests/layout-mutation.js",
	"./tests/lifetime.js",
];

function env(name) {
	try {
		return System.Environment.GetEnvironmentVariable(name) || "";
	} catch (_) {
		return "";
	}
}

// Each line is appended as it happens, so a crash still leaves the log of everything before it.
const logPath = env("NS_TEST_LOG");
function log(line) {
	console.log(line);
	if (!logPath) return;
	try {
		System.IO.File.AppendAllText(logPath, line + "\r\n");
	} catch (_) {}
}

// The window activates only once it has content, and the layout specs need a live window.
try {
	require("./tests/helpers").root();
} catch (e) {
	log("[ERROR] could not create the window content: " + (e && e.message));
}

for (const spec of specs) {
	try {
		require(spec);
	} catch (e) {
		// A spec file that can't even load is reported as a failing spec, not a silent skip.
		describe(spec, () => {
			it("loads", () => {
				throw e;
			});
		});
	}
}

function finish(run) {
	const resultsPath = env("NS_TEST_RESULTS");
	try {
		if (resultsPath) System.IO.File.WriteAllText(resultsPath, toJUnitXml(run));
	} catch (e) {
		log("[ERROR] could not write test results: " + (e && e.message));
	}
	if (resultsPath) System.Environment.Exit(run.failed);
}

// Start after the host has shown the window and started pumping, so specs that need layout,
// rendering or timers run inside a live app.
setTimeout(() => {
	const filterText = env("NS_TEST_FILTER");
	execute({ log, filter: filterText ? new RegExp(filterText, "i") : null }).then(finish, (e) => {
		log("[ERROR] runner crashed: " + (e && e.stack ? e.stack : e));
		finish({ results: [], passed: 0, failed: 1, pending: 0, time: 0 });
	});
}, 0);
