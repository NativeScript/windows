using System;

// Base types for the JS class-extension specs (tests/extend-dotnet.js): every member that JS
// overrides is invoked from C# here, so a passing spec proves native -> JS virtual dispatch, the
// same guarantee the Android (Java) and iOS (Objective-C) extension tests make.
namespace TestFixtures.Inheritance
{
    public class Animal
    {
        public Animal() : this("animal") { }

        public Animal(string name)
        {
            Name = name;
        }

        public Animal(string name, int age)
        {
            Name = name;
            Age = age;
        }

        public string Name { get; }

        public int Age { get; }

        public virtual string Speak() => "...";

        public virtual int Legs => 4;

        public virtual string Greet(string other) => $"{Name} greets {other}";

        protected virtual string Secret() => "base-secret";

        // Non-virtual members that call the virtual ones from C#.
        public string Describe() => $"{Name} says {Speak()} on {Legs} legs";

        public string RevealSecret() => Secret();

        public string GreetFromNative(string other) => Greet(other);
    }

    public abstract class Shape
    {
        public abstract double Area();

        public virtual string Kind => "shape";

        public string Report() => $"{Kind}:{Area():0.##}";
    }

    public interface ICalculator
    {
        int Compute(int a, int b);

        string Name { get; }
    }

    public interface INamed
    {
        string GetName();
    }

    public static class Native
    {
        public static string Describe(Animal animal) => animal.Describe();

        public static string Speak(Animal animal) => animal.Speak();

        public static int Legs(Animal animal) => animal.Legs;

        public static string Report(Shape shape) => shape.Report();

        public static int Run(ICalculator calculator, int a, int b) => calculator.Compute(a, b);

        public static string NameOf(ICalculator calculator) => calculator.Name;

        public static string NameOf(INamed named) => named.GetName();

        public static bool IsAnimal(object value) => value is Animal;

        public static string TypeName(object value) => value?.GetType().BaseType?.FullName ?? "";

        // Keeps a reference on the native side, so identity can be checked on the way back.
        private static object s_kept;

        public static void Keep(object value) => s_kept = value;

        public static object Kept() => s_kept;
    }
}
