// Callbacks invoked by native code on a background thread are delivered to JavaScript, as on iOS
// and Android: the calling thread waits while the callback runs on the JS (UI) thread, so return
// values flow back to native code. Fixtures: App_Resources/Windows/src/TestFixtures/Threading.cs.

function waitFor(predicate, timeoutMs = 4000) {
	return new Promise((resolve, reject) => {
		const started = Date.now();
		(function poll() {
			if (predicate()) return resolve();
			if (Date.now() - started > timeoutMs) return reject(new Error("timed out waiting for condition"));
			setTimeout(poll, 10);
		})();
	});
}

describe("Threading", () => {
	beforeAll(() => {
		TestFixtures.Threading.BackgroundInvoker.CaptureUiThread();
	});

	describe("WinRT delegates", () => {
		it("delivers a WorkItemHandler invoked on the thread pool", async () => {
			let ran = false;
			let onUiThread = null;
			const op = Windows.System.Threading.ThreadPool.RunAsync(() => {
				ran = true;
				onUiThread = TestFixtures.Threading.BackgroundInvoker.IsUiThread();
			});
			await NSWinRT.toPromise(op);
			expect(ran).toBe(true);
			expect(onUiThread).toBe(true);
		});

		it("delivers a ThreadPoolTimer callback", async () => {
			let fired = 0;
			Windows.System.Threading.ThreadPoolTimer.CreateTimer(() => {
				fired++;
			}, { Duration: 10 * 10000 });
			await waitFor(() => fired > 0);
			expect(fired).toBe(1);
		});

		it("delivers many background invocations in order", async () => {
			let count = 0;
			const ops = [];
			for (let i = 0; i < 20; i++) ops.push(NSWinRT.toPromise(Windows.System.Threading.ThreadPool.RunAsync(() => count++)));
			await Promise.all(ops);
			expect(count).toBe(20);
		});
	});

	describe(".NET delegates", () => {
		it("delivers a Func<> invoked on a background thread and returns its result", async () => {
			let result = null;
			let background = null;
			let calledOnUi = null;
			TestFixtures.Threading.BackgroundInvoker.ComputeInBackground(
				(v) => {
					calledOnUi = TestFixtures.Threading.BackgroundInvoker.IsUiThread();
					return v * 2;
				},
				21,
				(r, bg) => {
					result = r;
					background = bg;
				}
			);
			await waitFor(() => result !== null);
			expect(result).toBe(42);
			expect(background).toBe(true);
			expect(calledOnUi).toBe(true);
		});

		it("delivers calls from several threads at once", async () => {
			let done = null;
			TestFixtures.Threading.BackgroundInvoker.FanOut((n) => n + 1, 8, (count, sum) => (done = { count, sum }));
			await waitFor(() => done !== null);
			expect(done).toEqual({ count: 8, sum: 36 });
		});

		it("surfaces a JS exception thrown from a background call to the native caller", async () => {
			let outcome = null;
			TestFixtures.Threading.BackgroundInvoker.ThrowInBackground(
				() => {
					throw new Error("from js");
				},
				(r) => (outcome = r)
			);
			await waitFor(() => outcome !== null);
			expect(outcome).not.toBe("no-exception");
		});

		it("delivers C# events raised on a background thread", async () => {
			const ticker = new TestFixtures.Threading.Ticker();
			const ticks = [];
			ticker.add_Tick((sender, i) => ticks.push(i));
			ticker.StartInBackground(3);
			await waitFor(() => ticks.length === 3);
			expect(ticks).toEqual([1, 2, 3]);
		});

		it("resolves Task continuations that complete on the thread pool", async () => {
			const value = await TestFixtures.Basics.Callbacks.AddLaterAsync(1, 2, 20);
			expect(value).toBe(3);
		});
	});

	// A callback created in a worker belongs to that worker's isolate: invoked on a background
	// thread, it runs on the worker's thread (as on iOS and Android), not the UI thread. The
	// Node-API engines don't implement workers.
	(typeof Worker === "function" ? describe : xdescribe)("Workers", () => {
		const workerSource = `
			for (const name of ["setTimeout", "setInterval", "clearTimeout", "clearInterval"]) {
				if (typeof globalThis[name] !== "function") globalThis[name] = globalThis["__ns__" + name];
			}
			const BI = TestFixtures.Threading.BackgroundInvoker;
			const workerThread = BI.CurrentThreadId();
			const onWorker = () => BI.CurrentThreadId() === workerThread;
			const report = (test, run) =>
				Promise.resolve()
					.then(run)
					.then((result) => self.postMessage({ test, result }), (e) => self.postMessage({ test, error: String(e && e.message || e) }));
			self.onmessage = (e) => {
				if (e.data === "winrt") {
					report("winrt", async () => {
						let ranOnWorker = null;
						await NSWinRT.toPromise(Windows.System.Threading.ThreadPool.RunAsync(() => (ranOnWorker = onWorker())));
						return { ranOnWorker };
					});
				} else if (e.data === "dotnet") {
					report("dotnet", () => new Promise((resolve) => {
						let computedOnWorker = null;
						BI.ComputeInBackground(
							(v) => ((computedOnWorker = onWorker()), v * 2),
							21,
							(r, background) => resolve({ r, background, computedOnWorker, doneOnWorker: onWorker() })
						);
					}));
				} else if (e.data === "event") {
					report("event", () => new Promise((resolve) => {
						const ticker = new TestFixtures.Threading.Ticker();
						const ticks = [];
						ticker.add_Tick((sender, i) => {
							ticks.push(onWorker() ? i : -i);
							if (ticks.length === 3) resolve(ticks);
						});
						ticker.StartInBackground(3);
					}));
				}
			};
		`;
		let worker;
		const replies = {};

		function ask(test, timeoutMs = 6000) {
			worker.postMessage(test);
			return waitFor(() => test in replies, timeoutMs).then(() => {
				const reply = replies[test];
				if (reply.error) throw new Error(reply.error);
				return reply.result;
			});
		}

		beforeAll(() => {
			worker = new Worker(workerSource, { eval: true });
			worker.onmessage = (e) => (replies[e.data.test] = e.data);
		});

		afterAll(() => worker.terminate());

		it("delivers a WinRT delegate created in a worker on the worker's thread", async () => {
			expect(await ask("winrt")).toEqual({ ranOnWorker: true });
		});

		it("delivers a .NET delegate created in a worker on the worker's thread", async () => {
			expect(await ask("dotnet")).toEqual({ r: 42, background: true, computedOnWorker: true, doneOnWorker: true });
		});

		it("delivers C# events subscribed in a worker on the worker's thread", async () => {
			expect(await ask("event")).toEqual([1, 2, 3]);
		});

		it("delivers messages a worker posts on its own", async () => {
			const seen = [];
			const ticking = new Worker(
				`for (const name of ["setTimeout", "clearTimeout"]) if (typeof globalThis[name] !== "function") globalThis[name] = globalThis["__ns__" + name];
				setTimeout(() => self.postMessage("later"), 50);`,
				{ eval: true }
			);
			ticking.onmessage = (e) => seen.push(e.data);
			await waitFor(() => seen.length > 0);
			ticking.terminate();
			expect(seen).toEqual(["later"]);
		});

		it("refuses a callback whose worker has terminated without crashing", async () => {
			const BI = TestFixtures.Threading.BackgroundInvoker;
			let armed = false;
			const doomed = new Worker(
				`TestFixtures.Threading.BackgroundInvoker.CallLater(() => 1, 300); self.postMessage("armed");`,
				{ eval: true }
			);
			doomed.onmessage = () => (armed = true);
			await waitFor(() => armed);
			doomed.terminate();
			await waitFor(() => BI.LastLateCall != null);
			expect(BI.LastLateCall).toBe("JsException");
			expect(BI.CurrentThreadId()).toBeGreaterThan(0);
		});
	});
});
