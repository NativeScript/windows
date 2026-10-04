// Shared helpers for specs that need the live XAML tree.

const Controls = Microsoft.UI.Xaml.Controls;

let rootPanel = null;
let window = null;

function mainWindow() {
	if (!window) {
		window = new Microsoft.UI.Xaml.Window();
		window.Title = "NativeScript Windows TestRunner";
		window.Activate();
	}
	return window;
}

/** The window's root panel (a StackPanel), created on first use. */
function root() {
	if (rootPanel) return rootPanel;
	const win = mainWindow();
	rootPanel = new Controls.StackPanel();
	win.Content = rootPanel;
	return rootPanel;
}

/** A fresh, empty Grid attached to the root for one spec; removed by `detach`. */
function host(width = 300, height = 200) {
	const grid = new Controls.Grid();
	grid.Width = width;
	grid.Height = height;
	root().Children.Append(grid);
	return grid;
}

function detach(element) {
	const children = root().Children;
	for (let i = 0; i < children.Size; i++) {
		if (children.GetAt(i).Equals ? children.GetAt(i).Equals(element) : children.GetAt(i) === element) {
			children.RemoveAt(i);
			return;
		}
	}
}

function waitFor(predicate, timeoutMs = 4000) {
	return new Promise((resolve, reject) => {
		const started = Date.now();
		(function poll() {
			let ok = false;
			try {
				ok = predicate();
			} catch (e) {
				return reject(e);
			}
			if (ok) return resolve();
			if (Date.now() - started > timeoutMs) return reject(new Error("timed out waiting for condition"));
			setTimeout(poll, 10);
		})();
	});
}

function delay(ms) {
	return new Promise((resolve) => setTimeout(resolve, ms));
}

module.exports = { Controls, mainWindow, root, host, detach, waitFor, delay };
