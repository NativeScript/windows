// Extending C# classes and implementing C# interfaces from JavaScript, with the semantics the
// Android and iOS runtimes give Java/Objective-C classes: native code calling a virtual member
// lands in the JS override, constructors forward their arguments, `super` reaches the base
// implementation, `this` is the JS object (fields survive native round-trips) and instanceof
// works. Fixtures: App_Resources/Windows/src/TestFixtures/Inheritance.cs.

const { Animal, Shape, ICalculator, INamed, Native } = TestFixtures.Inheritance;

describe("Extending C# classes", () => {
	describe("with Class.extend()", () => {
		it("dispatches native virtual calls to the JS override", () => {
			const Dog = Animal.extend({
				Speak() {
					return "Woof";
				},
			});
			const dog = new Dog();
			expect(dog.Speak()).toBe("Woof");
			expect(Native.Speak(dog)).toBe("Woof");
			expect(dog.Describe()).toBe("animal says Woof on 4 legs");
		});

		it("supports a type name as the first argument", () => {
			const Cow = Animal.extend("TestApp.Cow", {
				Speak() {
					return "Moo";
				},
			});
			expect(Native.Speak(new Cow())).toBe("Moo");
		});

		it("forwards constructor arguments to the base constructor", () => {
			const Dog = Animal.extend({
				Speak() {
					return "Woof";
				},
			});
			const rex = new Dog("rex");
			expect(rex.Name).toBe("rex");
			expect(rex.Describe()).toBe("rex says Woof on 4 legs");
			const old = new Dog("fido", 12);
			expect(old.Age).toBe(12);
		});

		it("calls the base implementation through this.super", () => {
			const Loud = Animal.extend({
				Greet(other) {
					return this.super.Greet(other).toUpperCase();
				},
			});
			expect(new Loud("max").GreetFromNative("bob")).toBe("MAX GREETS BOB");
		});

		it("keeps members that are not overridden native", () => {
			const Dog = Animal.extend({
				Speak() {
					return "Woof";
				},
			});
			expect(new Dog("rex").Greet("bob")).toBe("rex greets bob");
			expect(Native.Legs(new Dog())).toBe(4);
		});

		it("overrides protected members", () => {
			const Spy = Animal.extend({
				Secret() {
					return "js-secret";
				},
			});
			expect(new Spy().RevealSecret()).toBe("js-secret");
		});

		it("overrides virtual properties with accessors", () => {
			const Bird = Animal.extend({
				get Legs() {
					return 2;
				},
				Speak() {
					return "Tweet";
				},
			});
			expect(Native.Legs(new Bird())).toBe(2);
			expect(new Bird("tweety").Describe()).toBe("tweety says Tweet on 2 legs");
		});

		it("is instanceof the extended and the base class", () => {
			const Dog = Animal.extend({
				Speak() {
					return "Woof";
				},
			});
			const dog = new Dog();
			expect(dog instanceof Dog).toBe(true);
			expect(dog instanceof Animal).toBe(true);
			expect(Native.IsAnimal(dog)).toBe(true);
		});

		it("can be extended again", () => {
			const Dog = Animal.extend({
				Speak() {
					return "Woof";
				},
			});
			const Puppy = Dog.extend({
				Speak() {
					return "Yip";
				},
			});
			const p = new Puppy("bit");
			expect(Native.Speak(p)).toBe("Yip");
			expect(p instanceof Dog).toBe(true);
			expect(p instanceof Animal).toBe(true);
		});
	});

	describe("with ES2015 class syntax", () => {
		class Cat extends Animal {
			constructor(name) {
				super(name, 3);
				this.lives = 9;
			}

			Speak() {
				return "Meow x" + this.lives;
			}

			Greet(other) {
				return super.Greet(other) + "!";
			}
		}

		it("dispatches native virtual calls to the JS override", () => {
			const cat = new Cat("tom");
			expect(Native.Speak(cat)).toBe("Meow x9");
			expect(cat.Describe()).toBe("tom says Meow x9 on 4 legs");
		});

		it("forwards super(...) arguments to the base constructor", () => {
			const cat = new Cat("tom");
			expect(cat.Name).toBe("tom");
			expect(cat.Age).toBe(3);
		});

		it("keeps JS fields visible to overrides called from native code", () => {
			const cat = new Cat("tom");
			cat.lives = 3;
			expect(Native.Speak(cat)).toBe("Meow x3");
		});

		it("calls the base implementation with super", () => {
			expect(new Cat("tom").GreetFromNative("jerry")).toBe("tom greets jerry!");
		});

		it("is instanceof the subclass and the base class", () => {
			const cat = new Cat("tom");
			expect(cat instanceof Cat).toBe(true);
			expect(cat instanceof Animal).toBe(true);
			expect(Native.IsAnimal(cat)).toBe(true);
		});

		it("returns the same JS object when native code hands the instance back", () => {
			const cat = new Cat("tom");
			Native.Keep(cat);
			expect(Native.Kept()).toBe(cat);
		});

		it("supports subclasses of subclasses", () => {
			class Kitten extends Cat {
				Speak() {
					return "mew (" + super.Speak() + ")";
				}
			}
			const k = new Kitten("kit");
			expect(Native.Speak(k)).toBe("mew (Meow x9)");
			expect(k instanceof Cat).toBe(true);
		});

		it("implements abstract members", () => {
			class Square extends Shape {
				constructor(side) {
					super();
					this.side = side;
				}

				Area() {
					return this.side * this.side;
				}

				get Kind() {
					return "square";
				}
			}
			expect(Native.Report(new Square(2))).toBe("square:4");
		});

		it("accepts `return global.__native(this)` in constructors, as on Android/iOS", () => {
			class Shared extends Animal {
				constructor() {
					super("shared");
					return global.__native(this);
				}

				Speak() {
					return "shared";
				}
			}
			const s = new Shared();
			expect(s instanceof Shared).toBe(true);
			expect(Native.Speak(s)).toBe("shared");
		});

		it("accepts the @NativeClass decorator", () => {
			expect(typeof NativeClass).toBe("function");
			// What TypeScript emits for `@NativeClass() class Decorated extends Animal {}`.
			let Decorated = class Decorated extends Animal {
				Speak() {
					return "decorated";
				}
			};
			Decorated = NativeClass()(Decorated) || Decorated;
			expect(Native.Speak(new Decorated())).toBe("decorated");
		});
	});
});

describe("Extending C# classes with TypeScript's ES5 output", () => {
	// What the @NativeClass transform (and TypeScript targeting ES5) emits for
	// `class Es5Dog extends Animal { constructor(name) { super(name); this.tricks = 2; } Speak() {...} }`.
	const Es5Dog = (function (_super) {
		__extends(Es5Dog, _super);
		function Es5Dog(name) {
			var _this = _super.call(this, name) || this;
			_this.tricks = 2;
			return _this;
		}
		Es5Dog.prototype.Speak = function () {
			return "woof x" + this.tricks;
		};
		Es5Dog.prototype.Greet = function (other) {
			return _super.prototype.Greet.call(this, other) + "?";
		};
		return Es5Dog;
	})(Animal);

	it("dispatches native virtual calls to the JS override", () => {
		const dog = new Es5Dog("rex");
		expect(Native.Speak(dog)).toBe("woof x2");
		expect(dog.Describe()).toBe("rex says woof x2 on 4 legs");
	});

	it("calls the base implementation through _super.prototype", () => {
		expect(new Es5Dog("rex").GreetFromNative("bob")).toBe("rex greets bob?");
	});

	it("is instanceof the subclass and the base class", () => {
		const dog = new Es5Dog("rex");
		expect(dog instanceof Es5Dog).toBe(true);
		expect(dog instanceof Animal).toBe(true);
	});
});

describe("Implementing C# interfaces", () => {
	it("implements an interface with new Interface({...})", () => {
		const calc = new ICalculator({
			Compute(a, b) {
				return a * b;
			},
			get Name() {
				return "multiply";
			},
		});
		expect(Native.Run(calc, 6, 7)).toBe(42);
		expect(Native.NameOf(calc)).toBe("multiply");
	});

	it("implements interfaces with Object.extend({ interfaces })", () => {
		const Named = Object.extend({
			interfaces: [INamed],
			GetName() {
				return "object-extend";
			},
		});
		expect(Native.NameOf(new Named())).toBe("object-extend");
	});

	it("implements interfaces on a class with @Interfaces", () => {
		class Named extends System.Object {
			GetName() {
				return "named";
			}
		}
		Interfaces([INamed])(Named);
		expect(Native.NameOf(new Named())).toBe("named");
	});
});
