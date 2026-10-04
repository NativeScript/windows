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
});
