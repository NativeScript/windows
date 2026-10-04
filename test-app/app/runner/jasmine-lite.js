// A small Jasmine-compatible test runner for the Windows runtime test app.
//
// The Android and iOS runtimes run their test-app suites on Jasmine; this keeps the same spec
// surface (describe/it/beforeEach/expect/done/...) without depending on a bundler or on the
// Node globals Jasmine's boot code expects, so the specs read the same across the three runtimes.

const DEFAULT_TIMEOUT_MS = 5000;

function Suite(description, parent) {
	this.description = description;
	this.parent = parent;
	this.children = [];
	this.beforeEach = [];
	this.afterEach = [];
	this.beforeAll = [];
	this.afterAll = [];
}

Suite.prototype.fullName = function () {
	const parts = [];
	for (let s = this; s && s.parent; s = s.parent) parts.unshift(s.description);
	return parts.join(" ");
};

const root = new Suite("", null);
let current = root;
let focused = false;

function addSpec(description, fn, opts) {
	current.children.push({ kind: "spec", description, fn, suite: current, pending: !!opts.pending, focused: !!opts.focused, timeout: opts.timeout });
	if (opts.focused) focused = true;
}

function addSuite(description, fn, opts) {
	const suite = new Suite(description, current);
	suite.pending = !!opts.pending || !!(current && current.pending);
	suite.focused = !!opts.focused || !!(current && current.focused);
	if (opts.focused) focused = true;
	current.children.push({ kind: "suite", suite });
	const prev = current;
	current = suite;
	try {
		fn();
	} finally {
		current = prev;
	}
}

// ── matchers ─────────────────────────────────────────────────────────────────

function fmt(v) {
	if (typeof v === "string") return JSON.stringify(v);
	if (typeof v === "function") return `[Function ${v.name || "anonymous"}]`;
	if (typeof v === "bigint") return `${v}n`;
	try {
		const s = JSON.stringify(v);
		if (s !== undefined) return s;
	} catch (_) {}
	try {
		return String(v);
	} catch (_) {
		return Object.prototype.toString.call(v);
	}
}

function deepEqual(a, b, seen) {
	if (Object.is(a, b)) return true;
	if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
	seen = seen || [];
	for (const [x, y] of seen) if (x === a && y === b) return true;
	seen.push([a, b]);
	if (Array.isArray(a) !== Array.isArray(b)) return false;
	const ka = Object.keys(a);
	const kb = Object.keys(b);
	if (ka.length !== kb.length) return false;
	for (const k of ka) {
		if (!Object.prototype.hasOwnProperty.call(b, k)) return false;
		if (!deepEqual(a[k], b[k], seen)) return false;
	}
	return true;
}

class ExpectationFailed extends Error {}

function makeExpect(onFailure) {
	return function expect(actual) {
		function build(negate) {
			function check(pass, message) {
				if (pass === negate) onFailure(new ExpectationFailed(negate ? message.replace(/^Expected (.*?) to /, "Expected $1 not to ") : message));
			}
			const m = {
				toBe: (e) => check(Object.is(actual, e), `Expected ${fmt(actual)} to be ${fmt(e)}.`),
				toEqual: (e) => check(deepEqual(actual, e), `Expected ${fmt(actual)} to equal ${fmt(e)}.`),
				toBeTruthy: () => check(!!actual, `Expected ${fmt(actual)} to be truthy.`),
				toBeFalsy: () => check(!actual, `Expected ${fmt(actual)} to be falsy.`),
				toBeTrue: () => check(actual === true, `Expected ${fmt(actual)} to be true.`),
				toBeFalse: () => check(actual === false, `Expected ${fmt(actual)} to be false.`),
				toBeDefined: () => check(actual !== undefined, `Expected ${fmt(actual)} to be defined.`),
				toBeUndefined: () => check(actual === undefined, `Expected ${fmt(actual)} to be undefined.`),
				toBeNull: () => check(actual === null, `Expected ${fmt(actual)} to be null.`),
				toBeNaN: () => check(Number.isNaN(actual), `Expected ${fmt(actual)} to be NaN.`),
				toBeGreaterThan: (e) => check(actual > e, `Expected ${fmt(actual)} to be greater than ${fmt(e)}.`),
				toBeGreaterThanOrEqual: (e) => check(actual >= e, `Expected ${fmt(actual)} to be greater than or equal to ${fmt(e)}.`),
				toBeLessThan: (e) => check(actual < e, `Expected ${fmt(actual)} to be less than ${fmt(e)}.`),
				toBeLessThanOrEqual: (e) => check(actual <= e, `Expected ${fmt(actual)} to be less than or equal to ${fmt(e)}.`),
				toBeCloseTo: (e, precision = 2) => check(Math.abs(e - actual) < Math.pow(10, -precision) / 2, `Expected ${fmt(actual)} to be close to ${fmt(e)}.`),
				toBeInstanceOf: (ctor) => check(actual instanceof ctor, `Expected ${fmt(actual)} to be an instance of ${(ctor && ctor.name) || fmt(ctor)}.`),
				toContain: (e) => check(actual != null && (typeof actual === "string" ? actual.indexOf(e) !== -1 : Array.prototype.some.call(actual, (x) => deepEqual(x, e))), `Expected ${fmt(actual)} to contain ${fmt(e)}.`),
				toMatch: (re) => check(new RegExp(re).test(String(actual)), `Expected ${fmt(actual)} to match ${String(re)}.`),
				toHaveBeenCalled: () => check(!!(actual && actual.calls && actual.calls.count() > 0), `Expected spy to have been called.`),
				toHaveBeenCalledTimes: (n) => check(!!(actual && actual.calls && actual.calls.count() === n), `Expected spy to have been called ${n} times but was called ${actual && actual.calls ? actual.calls.count() : 0} times.`),
				toHaveBeenCalledWith: (...args) => check(!!(actual && actual.calls && actual.calls.all().some((c) => deepEqual(c.args, args))), `Expected spy to have been called with ${fmt(args)}.`),
				toThrow: (e) => {
					let threw = false;
					let err;
					try {
						actual();
					} catch (x) {
						threw = true;
						err = x;
					}
					check(threw && (arguments.length === 0 || e === undefined || deepEqual(err, e)), `Expected function to throw${e !== undefined ? " " + fmt(e) : ""}${threw ? `, but it threw ${fmt(err && err.message)}` : ""}.`);
				},
				toThrowError: (typeOrMessage, maybeMessage) => {
					let threw = false;
					let err;
					try {
						actual();
					} catch (x) {
						threw = true;
						err = x;
					}
					let ok = threw;
					let type = typeof typeOrMessage === "function" ? typeOrMessage : null;
					let message = type ? maybeMessage : typeOrMessage;
					if (ok && type) ok = err instanceof type;
					if (ok && message !== undefined) {
						const text = err && err.message !== undefined ? String(err.message) : String(err);
						ok = message instanceof RegExp ? message.test(text) : text === message;
					}
					check(ok, `Expected function to throw an Error${message !== undefined ? " matching " + String(message) : ""}${threw ? `, but it threw ${fmt(err && err.message !== undefined ? err.message : err)}` : ", but it did not throw"}.`);
				},
			};
			return m;
		}
		const matchers = build(false);
		matchers.not = build(true);
		return matchers;
	};
}

// ── spies ────────────────────────────────────────────────────────────────────

function createSpy(name, impl) {
	const calls = [];
	let strategy = impl || null;
	const spy = function (...args) {
		calls.push({ object: this, args });
		return strategy ? strategy.apply(this, args) : undefined;
	};
	spy.and = {
		returnValue(v) {
			strategy = () => v;
			return spy;
		},
		callFake(fn) {
			strategy = fn;
			return spy;
		},
		throwError(e) {
			strategy = () => {
				throw typeof e === "string" ? new Error(e) : e;
			};
			return spy;
		},
	};
	spy.calls = {
		count: () => calls.length,
		argsFor: (i) => (calls[i] ? calls[i].args : []),
		all: () => calls.slice(),
		mostRecent: () => calls[calls.length - 1],
		reset: () => {
			calls.length = 0;
		},
	};
	spy.and.identity = name || "unknown";
	return spy;
}

// ── execution ────────────────────────────────────────────────────────────────

function runFn(fn, ctx, timeoutMs, label) {
	return new Promise((resolve, reject) => {
		let settled = false;
		const finish = (err) => {
			if (settled) return;
			settled = true;
			clearTimeout(timer);
			err ? reject(err) : resolve();
		};
		const timer = setTimeout(() => finish(new Error(`Timeout - ${label} did not complete within ${timeoutMs}ms`)), timeoutMs);
		try {
			if (fn.length > 0) {
				const done = () => finish();
				done.fail = (e) => finish(e instanceof Error ? e : new Error(e === undefined ? "Failed" : String(e)));
				fn.call(ctx, done);
			} else {
				const r = fn.call(ctx);
				if (r && typeof r.then === "function") r.then(() => finish(), (e) => finish(e || new Error("Promise rejected")));
				else finish();
			}
		} catch (e) {
			finish(e);
		}
	});
}

function chain(suite, key) {
	const out = [];
	for (let s = suite; s; s = s.parent) out.unshift(...s[key]);
	return key === "afterEach" ? out.reverse() : out;
}

function isFocused(node) {
	return node.kind === "spec" ? node.focused || node.suite.focused : node.suite.focused || node.suite.children.some(isFocused);
}

async function execute(options) {
	const results = [];
	const log = options.log || ((s) => console.log(s));
	const started = Date.now();

	async function visitSuite(suite) {
		const runnable = suite.children.filter((c) => !focused || isFocused(c));
		if (runnable.length === 0) return;
		for (const fn of suite.beforeAll) {
			try {
				await runFn(fn, {}, DEFAULT_TIMEOUT_MS, `beforeAll in "${suite.fullName()}"`);
			} catch (e) {
				log(`[ERROR] beforeAll failed in "${suite.fullName()}": ${e && e.message}`);
			}
		}
		for (const child of runnable) {
			if (child.kind === "suite") await visitSuite(child.suite);
			else await visitSpec(child);
		}
		for (const fn of suite.afterAll) {
			try {
				await runFn(fn, {}, DEFAULT_TIMEOUT_MS, `afterAll in "${suite.fullName()}"`);
			} catch (e) {
				log(`[ERROR] afterAll failed in "${suite.fullName()}": ${e && e.message}`);
			}
		}
	}

	async function visitSpec(spec) {
		const fullName = `${spec.suite.fullName()} ${spec.description}`.trim();
		if (spec.pending || spec.suite.pending || (options.filter && !options.filter.test(fullName))) {
			results.push({ suite: spec.suite.fullName(), name: spec.description, fullName, status: "pending", failures: [], time: 0 });
			return;
		}
		const failures = [];
		const t0 = Date.now();
		currentFailures = failures;
		const ctx = {};
		const timeout = spec.timeout || DEFAULT_TIMEOUT_MS;
		try {
			for (const fn of chain(spec.suite, "beforeEach")) await runFn(fn, ctx, timeout, "beforeEach");
			await runFn(spec.fn, ctx, timeout, `"${fullName}"`);
		} catch (e) {
			failures.push(e);
		}
		try {
			for (const fn of chain(spec.suite, "afterEach")) await runFn(fn, ctx, timeout, "afterEach");
		} catch (e) {
			failures.push(e);
		}
		currentFailures = null;
		const status = failures.length ? "failed" : "passed";
		results.push({ suite: spec.suite.fullName(), name: spec.description, fullName, status, failures: failures.map(describeError), time: (Date.now() - t0) / 1000 });
		if (status === "passed") log(`[PASS] ${fullName}`);
		else for (const f of failures) log(`[FAIL] ${fullName} - ${describeError(f).message}`);
	}

	await visitSuite(root);
	const passed = results.filter((r) => r.status === "passed").length;
	const failed = results.filter((r) => r.status === "failed").length;
	const pending = results.filter((r) => r.status === "pending").length;
	log(`[TEST SUMMARY] passed=${passed}, failed=${failed}, pending=${pending}`);
	return { results, passed, failed, pending, time: (Date.now() - started) / 1000 };
}

function describeError(e) {
	if (e && typeof e === "object" && "message" in e && "stack" in e) return e;
	if (e && typeof e === "object") return { message: e.message !== undefined ? String(e.message) : fmt(e), stack: e.stack || "" };
	return { message: String(e), stack: "" };
}

let currentFailures = null;
const expect = makeExpect((err) => {
	if (currentFailures) currentFailures.push(err);
	else throw err;
});

function escapeXml(s) {
	return String(s).replace(/[<>&"']/g, (c) => ({ "<": "&lt;", ">": "&gt;", "&": "&amp;", '"': "&quot;", "'": "&apos;" })[c]);
}

/** JUnit XML in the shape the Android test-app reporter produces, so CI can consume either. */
function toJUnitXml(run) {
	const bySuite = new Map();
	for (const r of run.results) {
		if (!bySuite.has(r.suite)) bySuite.set(r.suite, []);
		bySuite.get(r.suite).push(r);
	}
	const lines = ['<?xml version="1.0" encoding="UTF-8" ?>', `<testsuites tests="${run.results.length}" failures="${run.failed}" skipped="${run.pending}" time="${run.time}">`];
	for (const [suite, specs] of bySuite) {
		const failures = specs.filter((s) => s.status === "failed").length;
		const skipped = specs.filter((s) => s.status === "pending").length;
		lines.push(`  <testsuite name="${escapeXml(suite)}" tests="${specs.length}" failures="${failures}" skipped="${skipped}">`);
		for (const s of specs) {
			lines.push(`    <testcase classname="${escapeXml(suite)}" name="${escapeXml(s.name)}" time="${s.time}">`);
			if (s.status === "pending") lines.push("      <skipped />");
			for (const f of s.failures) lines.push(`      <failure message="${escapeXml(f.message)}">${escapeXml(f.stack || f.message)}</failure>`);
			lines.push("    </testcase>");
		}
		lines.push("  </testsuite>");
	}
	lines.push("</testsuites>");
	return lines.join("\n");
}

const api = {
	describe: (d, fn) => addSuite(d, fn, {}),
	xdescribe: (d, fn) => addSuite(d, fn, { pending: true }),
	fdescribe: (d, fn) => addSuite(d, fn, { focused: true }),
	it: (d, fn, timeout) => addSpec(d, fn || (() => {}), { pending: !fn, timeout }),
	xit: (d, fn) => addSpec(d, fn || (() => {}), { pending: true }),
	fit: (d, fn, timeout) => addSpec(d, fn, { focused: true, timeout }),
	beforeEach: (fn) => current.beforeEach.push(fn),
	afterEach: (fn) => current.afterEach.push(fn),
	beforeAll: (fn) => current.beforeAll.push(fn),
	afterAll: (fn) => current.afterAll.push(fn),
	expect,
	fail: (message) => {
		const err = new ExpectationFailed(message === undefined ? "Failed" : String(message));
		if (currentFailures) currentFailures.push(err);
		else throw err;
	},
	pending: () => {},
	jasmine: {
		createSpy,
		DEFAULT_TIMEOUT_INTERVAL: DEFAULT_TIMEOUT_MS,
	},
	spyOn(obj, method) {
		const original = obj[method];
		const spy = createSpy(method, function (...args) {
			return original.apply(this, args);
		});
		spy.and.callThrough = () => spy.and.callFake(original);
		obj[method] = spy;
		return spy;
	},
};

module.exports = { api, execute, toJUnitXml };
