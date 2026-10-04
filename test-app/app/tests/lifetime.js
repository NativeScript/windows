// Lifetime of JS subclass instances. An instance is a JS object plus a managed object; neither
// keeps the other alive on its own, so an instance nothing references is collected, while one
// native code holds keeps working (its JS object is made anew if it was collected, as on Android).
// Subclasses of WinRT UI classes stay alive, fields included, while they are in the visual tree.
// Fixtures: App_Resources/Windows/src/TestFixtures/Lifetime.cs.

const { Controls, host, detach, delay } = require("./helpers");
const { Tracked, Gc } = TestFixtures.Lifetime;
const { Animal, Native } = TestFixtures.Inheritance;

// Collects garbage on both sides until `done()` holds: JS first (FinalizationRegistry callbacks run
// as tasks, so wait between rounds), then .NET.
async function collect(done, rounds = 10) {
	for (let i = 0; i < rounds && !(done && done()); i++) {
		gc();
		await delay(20);
		Gc.Collect();
		await delay(20);
	}
}

const describeIfGc = typeof gc === "function" ? describe : xdescribe;

describeIfGc("Lifetime of JS subclass instances", () => {
	it("releases instances neither JS nor native code references", async () => {
		class Dropped extends Tracked {
			Name() {
				return "dropped";
			}
		}
		const before = Tracked.Finalized;
		(function () {
			for (let i = 0; i < 20; i++) new Dropped();
		})();
		await collect(() => Tracked.Finalized - before >= 20);
		expect(Tracked.Finalized - before).toBeGreaterThanOrEqual(15);
	}, 15000);

	it("keeps working when only native code holds the instance", async () => {
		class Parrot extends Animal {
			Speak() {
				return "polly";
			}
		}
		Native.Keep(new Parrot("p"));
		await collect(null, 4);
		const kept = Native.Kept();
		expect(Native.Speak(kept)).toBe("polly");
		expect(kept instanceof Parrot).toBe(true);
		expect(kept.Name).toBe("p");
		expect(Native.Kept()).toBe(kept);
	}, 15000);

	it("keeps a WinRT UI subclass in the visual tree alive, fields included", async () => {
		class Sized extends Controls.Panel {
			constructor() {
				super();
				this.width = 77;
			}

			MeasureOverride() {
				return { Width: this.width, Height: 10 };
			}
		}
		const container = host();
		try {
			container.Children.Append(new Sized());
			// Let it get into the tree and past the next attachment check.
			await delay(2100);
			container.InvalidateMeasure();
			container.UpdateLayout();
			await collect(null, 4);
			const child = container.Children.GetAt(0);
			expect(child instanceof Sized).toBe(true);
			expect(child.width).toBe(77);
			expect(child.DesiredSize.Width).toBe(77);
		} finally {
			detach(container);
		}
	}, 15000);
});
