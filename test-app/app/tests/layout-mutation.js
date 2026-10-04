// Changing the XAML tree synchronously from layout and rendering callbacks. In a C# WinUI app
// adding, removing or reparenting elements from SizeChanged, LayoutUpdated or
// CompositionTarget.Rendering is legal, and on Android/iOS changing views from onSizeChanged /
// layoutSubviews works too, so it must not take the app down (stowed exception 0xC000027B) when
// the handler is JavaScript.

const { Controls, host, detach, waitFor, delay } = require("./helpers");

function text(value) {
	const t = new Controls.TextBlock();
	t.Text = value;
	return t;
}

describe("Changing the UI during layout", () => {
	let container;
	beforeEach(() => {
		container = host();
	});
	afterEach(() => {
		detach(container);
	});

	it("adds children from SizeChanged", async () => {
		const panel = new Controls.StackPanel();
		let added = false;
		panel.SizeChanged = () => {
			if (added) return;
			added = true;
			panel.Children.Append(text("from SizeChanged"));
			panel.Children.Append(text("and another"));
		};
		container.Children.Append(panel);
		await waitFor(() => added && panel.Children.Size === 2);
		await delay(50);
		expect(panel.Children.Size).toBe(2);
	});

	it("removes and reparents children from SizeChanged", async () => {
		const a = new Controls.StackPanel();
		const b = new Controls.StackPanel();
		const child = text("moving");
		a.Children.Append(child);
		let moved = false;
		a.SizeChanged = () => {
			if (moved) return;
			moved = true;
			a.Children.Clear();
			b.Children.Append(child);
		};
		container.Children.Append(a);
		container.Children.Append(b);
		await waitFor(() => moved);
		await delay(50);
		expect(a.Children.Size).toBe(0);
		expect(b.Children.Size).toBe(1);
	});

	it("adds children from LayoutUpdated", async () => {
		const panel = new Controls.StackPanel();
		let added = false;
		panel.LayoutUpdated = () => {
			if (added) return;
			added = true;
			panel.Children.Append(text("from LayoutUpdated"));
		};
		container.Children.Append(panel);
		await waitFor(() => added && panel.Children.Size === 1);
		await delay(50);
		panel.LayoutUpdated = null;
		expect(panel.Children.Size).toBe(1);
	});

	it("adds and removes children from CompositionTarget.Rendering", async () => {
		const panel = new Controls.StackPanel();
		container.Children.Append(panel);
		let frames = 0;
		const handler = () => {
			frames++;
			if (frames === 1) panel.Children.Append(text("frame 1"));
			if (frames === 2) panel.Children.Append(text("frame 2"));
			if (frames === 3) panel.Children.RemoveAt(0);
		};
		Microsoft.UI.Xaml.Media.CompositionTarget.Rendering = handler;
		try {
			await waitFor(() => frames >= 4);
		} finally {
			Microsoft.UI.Xaml.Media.CompositionTarget.Rendering = null;
		}
		await delay(50);
		expect(panel.Children.Size).toBe(1);
	});

	it("reparents children from LayoutUpdated", async () => {
		const a = new Controls.StackPanel();
		const b = new Controls.StackPanel();
		const child = text("moving");
		a.Children.Append(child);
		let moved = false;
		a.LayoutUpdated = () => {
			if (moved) return;
			moved = true;
			a.Children.RemoveAt(0);
			b.Children.Append(child);
		};
		container.Children.Append(a);
		container.Children.Append(b);
		await waitFor(() => moved);
		await delay(50);
		a.LayoutUpdated = null;
		expect(b.Children.Size).toBe(1);
	});

	it("reparents a subtree from CompositionTarget.Rendering", async () => {
		const a = new Controls.StackPanel();
		const b = new Controls.StackPanel();
		const subtree = new Controls.StackPanel();
		subtree.Children.Append(text("one"));
		subtree.Children.Append(text("two"));
		a.Children.Append(subtree);
		container.Children.Append(a);
		container.Children.Append(b);
		let moved = false;
		Microsoft.UI.Xaml.Media.CompositionTarget.Rendering = () => {
			if (moved) return;
			moved = true;
			a.Children.Clear();
			b.Children.Append(subtree);
		};
		try {
			await waitFor(() => moved);
			await delay(50);
		} finally {
			Microsoft.UI.Xaml.Media.CompositionTarget.Rendering = null;
		}
		expect(a.Children.Size).toBe(0);
		expect(b.Children.Size).toBe(1);
	});

	it("removes the element whose SizeChanged is firing", async () => {
		const panel = new Controls.StackPanel();
		panel.Children.Append(text("about to go"));
		let removed = false;
		panel.SizeChanged = () => {
			if (removed) return;
			removed = true;
			container.Children.Clear();
		};
		container.Children.Append(panel);
		await waitFor(() => removed);
		await delay(50);
		expect(container.Children.Size).toBe(0);
	});

	it("mutates the tree from promise continuations scheduled in a layout callback", async () => {
		const panel = new Controls.StackPanel();
		let scheduled = false;
		let ran = false;
		panel.SizeChanged = () => {
			if (scheduled) return;
			scheduled = true;
			Promise.resolve().then(() => {
				panel.Children.Append(text("from a microtask"));
				ran = true;
			});
		};
		container.Children.Append(panel);
		await waitFor(() => ran);
		await delay(50);
		expect(panel.Children.Size).toBe(1);
	});

	it("replaces content from SizeChanged", async () => {
		const border = new Controls.Border();
		border.Child = text("before");
		let swapped = false;
		border.SizeChanged = () => {
			if (swapped) return;
			swapped = true;
			border.Child = text("after");
		};
		container.Children.Append(border);
		await waitFor(() => swapped);
		await delay(50);
		expect(border.Child.Text).toBe("after");
	});
});
