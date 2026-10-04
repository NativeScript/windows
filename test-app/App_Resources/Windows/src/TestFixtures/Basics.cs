using System;
using System.Collections.Generic;
using System.Threading.Tasks;

// C# sources placed under App_Resources/Windows are compiled into the app, the way Java/Kotlin
// under App_Resources/Android/src and Objective-C/Swift under App_Resources/iOS/src are on the
// other platforms. JavaScript reaches them by their namespace, with no registration step.
namespace TestFixtures.Basics
{
    public enum Mood
    {
        Happy,
        Sad,
        Sleepy = 10,
    }

    public class Greeter
    {
        public static int Instances;

        public Greeter() : this("World") { }

        public Greeter(string name)
        {
            Name = name;
            Instances++;
        }

        public string Name { get; set; }

        public Mood Mood { get; set; } = Mood.Happy;

        public string Greet() => $"Hello, {Name}!";

        public string Greet(string other) => $"Hello, {other}, from {Name}!";

        public static int Add(int a, int b) => a + b;

        public static double Scale(double value, double factor) => value * factor;

        public static string Version => "1.0";

        public int[] Numbers() => new[] { 1, 2, 3 };

        public List<string> Names() => new List<string> { "a", "b" };

        public event EventHandler<string> Greeted;

        public void RaiseGreeted(string message) => Greeted?.Invoke(this, message);
    }

    public static class Callbacks
    {
        public static int Apply(Func<int, int> f, int value) => f(value);

        public static string Join(Func<string, string, string> f, string a, string b) => f(a, b);

        public static int Repeat(Action<int> action, int times)
        {
            for (var i = 0; i < times; i++) action(i);
            return times;
        }

        public static async Task<int> AddLaterAsync(int a, int b, int delayMs)
        {
            await Task.Delay(delayMs).ConfigureAwait(false);
            return a + b;
        }

        public static async Task FailLaterAsync(string message, int delayMs)
        {
            await Task.Delay(delayMs).ConfigureAwait(false);
            throw new InvalidOperationException(message);
        }
    }

    // Internal types are reachable too (only public members are bound), as with Java
    // package-private classes from JS on Android.
    internal static class InternalHelper
    {
        public static string Ping() => "pong";
    }
}

namespace TestFixtures.Basics.Nested.Deeper
{
    public static class Deep
    {
        public static string Where() => "deep";
    }
}
