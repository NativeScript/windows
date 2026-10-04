// Values stored in Object-typed WinRT members (FrameworkElement.Tag, PropertySet entries) are
// boxed as IPropertyValue; reading them back yields the JS primitive, as on iOS and Android.

describe("Object values", () => {
	const Controls = Microsoft.UI.Xaml.Controls;

	it("round-trips a string through Tag", () => {
		const panel = new Controls.StackPanel();
		panel.Tag = "tagged";
		expect(panel.Tag).toBe("tagged");
	});

	it("round-trips numbers and booleans through Tag", () => {
		const panel = new Controls.StackPanel();
		panel.Tag = 42.5;
		expect(panel.Tag).toBe(42.5);
		panel.Tag = true;
		expect(panel.Tag).toBe(true);
	});

	it("returns objects stored in Tag as themselves", () => {
		const panel = new Controls.StackPanel();
		const text = new Controls.TextBlock();
		text.Text = "inner";
		panel.Tag = text;
		expect(panel.Tag.Text).toBe("inner");
	});

	it("unboxes PropertySet values", () => {
		const set = new Windows.Foundation.Collections.PropertySet();
		set.Insert("name", "value");
		set.Insert("count", 3);
		expect(set.Lookup("name")).toBe("value");
		expect(set.Lookup("count")).toBe(3);
	});
});
