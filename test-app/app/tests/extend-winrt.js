// Extending WinRT/WinUI classes from JavaScript. XAML's layout engine calls MeasureOverride /
// ArrangeOverride / OnApplyTemplate on our subclass, which must land in the JS overrides — the
// equivalent of overriding onMeasure/onLayout on an Android View or layoutSubviews on a UIView.

const { Controls, host, detach, waitFor } = require("./helpers");
const Size = (w, h) => ({ Width: w, Height: h });

describe("Extending WinRT classes", () => {
	let container;
	beforeEach(() => {
		container = host();
	});
	afterEach(() => {
		detach(container);
	});

	describe("with ES2015 class syntax", () => {
		class FixedPanel extends Controls.Panel {
			constructor() {
				super();
				this.measures = 0;
				this.arranges = 0;
			}

			MeasureOverride(available) {
				this.measures++;
				return Size(120, 40);
			}

			ArrangeOverride(finalSize) {
				this.arranges++;
				return finalSize;
			}
		}

		it("dispatches XAML layout to MeasureOverride/ArrangeOverride", async () => {
			const panel = new FixedPanel();
			container.Children.Append(panel);
			await waitFor(() => panel.arranges > 0);
			expect(panel.measures).toBeGreaterThan(0);
			expect(panel.DesiredSize.Width).toBe(120);
			expect(panel.DesiredSize.Height).toBe(40);
		});

		it("is instanceof the subclass and its WinRT bases", () => {
			const panel = new FixedPanel();
			expect(panel instanceof FixedPanel).toBe(true);
			expect(panel instanceof Controls.Panel).toBe(true);
			expect(panel instanceof Microsoft.UI.Xaml.UIElement).toBe(true);
		});

		it("returns the same JS object when XAML hands the instance back", () => {
			const panel = new FixedPanel();
			panel.marker = "mine";
			container.Children.Append(panel);
			const back = container.Children.GetAt(0);
			expect(back).toBe(panel);
			expect(back.marker).toBe("mine");
		});

		it("keeps native members of the base class", () => {
			const panel = new FixedPanel();
			panel.Width = 123;
			expect(panel.Width).toBe(123);
			panel.Children.Append(new Controls.TextBlock());
			expect(panel.Children.Size).toBe(1);
		});

		it("calls the base implementation with super", async () => {
			class Logged extends Controls.StackPanel {
				MeasureOverride(available) {
					this.called = true;
					return super.MeasureOverride(available);
				}
			}
			const panel = new Logged();
			const text = new Controls.TextBlock();
			text.Text = "hello";
			panel.Children.Append(text);
			container.Children.Append(panel);
			await waitFor(() => panel.called === true && panel.ActualHeight > 0);
			expect(panel.DesiredSize.Height).toBeGreaterThan(0);
		});

		it("dispatches OnApplyTemplate on a templated control", async () => {
			class TemplatedButton extends Controls.Button {
				OnApplyTemplate() {
					super.OnApplyTemplate();
					this.applied = (this.applied || 0) + 1;
				}
			}
			const button = new TemplatedButton();
			button.Content = "tap";
			container.Children.Append(button);
			await waitFor(() => button.applied > 0);
			expect(button.applied).toBe(1);
		});
	});

	describe("with TypeScript's ES5 output", () => {
		// What the @NativeClass transform emits for a class extending a WinRT class.
		const Es5Panel = (function (_super) {
			__extends(Es5Panel, _super);
			function Es5Panel() {
				var _this = _super.call(this) || this;
				_this.measured = 0;
				return _this;
			}
			Es5Panel.prototype.MeasureOverride = function (available) {
				this.measured++;
				return Size(48, 24);
			};
			return Es5Panel;
		})(Controls.Panel);

		it("dispatches XAML layout to the overrides", async () => {
			const panel = new Es5Panel();
			expect(panel instanceof Es5Panel).toBe(true);
			expect(panel instanceof Controls.Panel).toBe(true);
			container.Children.Append(panel);
			await waitFor(() => panel.measured > 0 && panel.DesiredSize.Width === 48);
			expect(panel.DesiredSize.Height).toBe(24);
		});

		it("calls the base implementation through _super.prototype", async () => {
			const Es5Stack = (function (_super) {
				__extends(Es5Stack, _super);
				function Es5Stack() {
					return _super.call(this) || this;
				}
				Es5Stack.prototype.MeasureOverride = function (available) {
					this.called = true;
					return _super.prototype.MeasureOverride.call(this, available);
				};
				return Es5Stack;
			})(Controls.StackPanel);
			const panel = new Es5Stack();
			const label = new Controls.TextBlock();
			label.Text = "hello";
			panel.Children.Append(label);
			container.Children.Append(panel);
			await waitFor(() => panel.called === true && panel.ActualHeight > 0);
			expect(panel.DesiredSize.Height).toBeGreaterThan(0);
		});
	});

	describe("with Class.extend()", () => {
		it("dispatches XAML layout to the overrides", async () => {
			const Sized = Controls.Panel.extend({
				MeasureOverride(available) {
					return Size(64, 32);
				},
				ArrangeOverride(finalSize) {
					this.arranged = true;
					return finalSize;
				},
			});
			const panel = new Sized();
			container.Children.Append(panel);
			await waitFor(() => panel.arranged === true);
			expect(panel.DesiredSize.Width).toBe(64);
			expect(panel instanceof Controls.Panel).toBe(true);
		});
	});
});
