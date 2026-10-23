using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Threading.Tasks;

// Fixtures for tests/csharp-interop.js.
namespace TestFixtures.Interop
{
    public enum Tint
    {
        Red = 1,
        Green = 2,
        Blue = 4,
    }

    [Flags]
    public enum Access
    {
        None = 0,
        Read = 1,
        Write = 2,
    }

    public class Fields
    {
        public const string Label = "fields";
        public static readonly int Answer = 42;
        public static int Counter;
        public int Value = 7;
        public readonly string Id = "id-1";
        public Tint Tint = Tint.Green;

        public int Twice() => Value * 2;
    }

    public class Outer
    {
        public class Inner
        {
            public static string Where() => "inner";

            public string Hello() => "hi";
        }

        public enum Kind
        {
            First,
            Second,
        }
    }

    public static class Numbers
    {
        public static long Big() => 9007199254740993L;
        public static long Small() => 42L;
        public static ulong Huge() => ulong.MaxValue;
        public static long Echo(long value) => value;
        public static ulong EchoUnsigned(ulong value) => value;
        public static string Describe(int value) => "int:" + value;
        public static string Describe(long value) => "long:" + value;
        public static string Text(long value) => value.ToString();

        public static byte[] Bytes() => new byte[] { 1, 2, 255 };
        public static int Sum(byte[] data) => data.Sum(b => (int)b);
        public static int SumInts(int[] data) => data.Sum();
        public static double SumDoubles(double[] data) => data.Sum();
        public static int Length(ReadOnlyMemory<byte> data) => data.Length;
        public static long StreamLength(Stream stream) => stream.Length;
        public static int[] Ints() => new[] { 1, 2, 3 };
    }

    public static class Params
    {
        public static string Optional(string a, string b = "B", int c = 3) => $"{a}{b}{c}";

        public static string Tinted(Tint tint = Tint.Blue) => tint.ToString();

        public static bool TryDouble(string text, out int result)
        {
            if (int.TryParse(text, out var value))
            {
                result = value * 2;
                return true;
            }
            result = 0;
            return false;
        }

        public static void Swap(ref int a, ref int b) => (a, b) = (b, a);

        public static void Split(string full, out string first, out string last)
        {
            var parts = full.Split(' ');
            first = parts[0];
            last = parts.Length > 1 ? parts[1] : "";
        }

        public static string Point(int x = 0, int y = 0, string label = "p") => $"{label}({x},{y})";
    }

    public class Sized
    {
        public Sized(string name, int size = 10)
        {
            Name = name;
            Size = size;
        }

        public string Name { get; }
        public int Size { get; }
    }

    public static class Generics
    {
        public static T Identity<T>(T value) => value;
        public static string NameOf<T>() => typeof(T).Name;
        public static int CountOf<T>(IEnumerable<T> items) => items.Count();
        public static List<T> Repeat<T>(T value, int times) => Enumerable.Repeat(value, times).ToList();
        public static string Describe<T>(List<T> list) => typeof(T).Name + ":" + list.Count;
    }

    public class Box<T>
    {
        public Box(T value) => Value = value;

        public T Value { get; set; }

        public string TypeArgument => typeof(T).Name;
    }

    public class Registry
    {
        public static Registry Shared { get; } = new Registry();

        public Registry Self() => this;

        public List<string> Names { get; } = new List<string> { "a" };

        public Dictionary<string, int> Scores { get; } = new Dictionary<string, int> { ["alice"] = 3 };

        public Dictionary<string, object> Bag() => new Dictionary<string, object> { ["n"] = 1, ["s"] = "two" };
    }

    public class Talker
    {
        public event EventHandler<string>? Said;
        public static event Action<int>? Ticked;

        public int Say(string message)
        {
            Said?.Invoke(this, message);
            return Handlers;
        }

        public int Handlers => Said?.GetInvocationList().Length ?? 0;

        public static int Tick(int n)
        {
            Ticked?.Invoke(n);
            return Ticked?.GetInvocationList().Length ?? 0;
        }
    }

    public static class Failing
    {
        public static void Throw(string message) => throw new InvalidOperationException(message);

        public static void ThrowWithInner() => throw new InvalidOperationException("outer", new ArgumentException("inner"));

        public static async Task FailAsync(string message)
        {
            await Task.Delay(5).ConfigureAwait(false);
            throw new FormatException(message);
        }

        // What a JS exception looks like to the C# code that called the callback.
        public static string Catch(Func<int> callback)
        {
            try
            {
                callback();
                return "none";
            }
            catch (Exception e)
            {
                return e.Message + "|" + (e.StackTrace ?? "");
            }
        }

        public static string CatchVoid(Action callback)
        {
            try
            {
                callback();
                return "none";
            }
            catch (Exception e)
            {
                return e.Message;
            }
        }
    }

    public static class Extensions
    {
        public static string Shout(this string text) => text.ToUpperInvariant() + "!";

        public static int Thrice(this Fields fields) => fields.Value * 3;
    }

    public abstract class Shape
    {
        protected virtual string Kind => "shape";

        protected int Sides { get; set; }

        protected string Describe(int n) => "n" + n;

        protected string Describe(string s) => "s" + s;

        public string Report() => Kind + ":" + Sides;

        public virtual T Convert<T>(object value) => (T)value;

        public string UseConvert() => Convert<string>("x");

        public abstract string Draw();
    }

    // .NET code creating instances of JS classes, by type or by name.
    public static class Factories
    {
        public static object Create(Type type) => Activator.CreateInstance(type)!;

        public static object CreateByName(string name) => Activator.CreateInstance(Type.GetType(name, throwOnError: true)!)!;

        public static string Draw(Shape shape) => shape.Draw();

        public static string TypeName(object value) => value.GetType().FullName!;
    }

    public static class Values
    {
        public static (int, string) Pair() => (1, "one");

        public static Dimensions Make(double width, double height) => new Dimensions { Width = width, Height = height };

        public static double Area(Dimensions d) => d.Width * d.Height;
    }

    public struct Dimensions
    {
        public double Width;
        public double Height;

        public double Area => Width * Height;
    }
}
