using System;
using System.Reflection;
using BenchmarkDotNet.Attributes;
using NativeScriptBridge;

/// <summary>
/// What a method costs the first time JS calls it (building its invoker) and after that.
///
///   dotnet run -c Release -- --filter *EntryBenchmarks* --job short
/// </summary>
[MemoryDiagnoser]
public class EntryBenchmarks
{
    private static readonly MethodInfo s_max = typeof(Math).GetMethod(nameof(Math.Max), [typeof(int), typeof(int)])!;
    private static readonly MethodInfo s_append = typeof(System.Text.StringBuilder).GetMethod(nameof(System.Text.StringBuilder.Append), [typeof(string)])!;
    private readonly object?[] _maxArgs = [3, 7];
    private readonly System.Text.StringBuilder _sb = new();
    private readonly object?[] _appendArgs = [""];
    private DispatchEntry _warmMax;
    private DispatchEntry _warmAppend;

    [GlobalSetup]
    public void Setup()
    {
        _warmMax = Bridge.BuildDispatchEntry(typeof(Math), s_max);
        _warmAppend = Bridge.BuildDispatchEntry(typeof(System.Text.StringBuilder), s_append);
    }

    [Benchmark] public object? FirstCallStatic() => Bridge.BuildDispatchEntry(typeof(Math), s_max).Invoke!(null, _maxArgs);

    [Benchmark] public object? FirstCallInstance() => Bridge.BuildDispatchEntry(typeof(System.Text.StringBuilder), s_append).Invoke!(_sb, _appendArgs);

    [Benchmark] public object? WarmStatic() => _warmMax.Invoke!(null, _maxArgs);

    [Benchmark] public object? WarmInstance() => _warmAppend.Invoke!(_sb, _appendArgs);
}
