using System;
using System.Buffers;
using System.Text.Json;
using BenchmarkDotNet.Attributes;
using BenchmarkDotNet.Columns;
using BenchmarkDotNet.Configs;
using BenchmarkDotNet.Running;
using NativeScriptBridge;

BenchmarkSwitcher.FromTypes([typeof(BridgeBenchmarks), typeof(AbiBenchmarks)]).Run(args);

/// <summary>
/// Measures the hot-path dispatch cost for the JSON and binary protocols.
///
/// Run in Release mode:
///   dotnet run -c Release
///
/// Categories:
///   StaticCall   — warm static method dispatch (Math.Abs)
///   PropertyGet  — warm instance property get (StringBuilder.Length)
///   Constructor  — constructor + release round-trip
///   ColdPath     — first-call cost: type resolution + expression compile
/// </summary>
[MemoryDiagnoser]
[HideColumns(Column.Error, Column.StdDev, Column.RatioSD)]
[GroupBenchmarksBy(BenchmarkLogicalGroupRule.ByCategory)]
[CategoriesColumn]
public class BridgeBenchmarks
{
    // JSON: {"assembly":"System","typeName":"System.Math","method":"Abs","args":[-42]}
    private static readonly byte[] s_jsonMathAbsBytes =
        System.Text.Encoding.UTF8.GetBytes(
            """{"assembly":"System","typeName":"System.Math","method":"Abs","args":[-42]}""");

    private static readonly JsonElement[] s_arg42 = [JsonSerializer.SerializeToElement(-42)];

    private static readonly byte[] s_binMathAbs = BuildPacket(w =>
    {
        w.WriteByte(0x02);                      // static call
        w.WriteString16("System.Math");
        w.WriteString16("System");
        w.WriteString16("Abs");
        w.WriteByte(1);                         // 1 arg
        w.WriteByte(0x03); w.WriteI32(-42);     // i32 arg
    });

    private static readonly byte[] s_binCtorSb = BuildPacket(w =>
    {
        w.WriteByte(0x03);                      // constructor
        w.WriteString16("System.Text.StringBuilder");
        w.WriteString16("System");
        w.WriteByte(0);                         // no args
    });

    // handle-dependent packets built in GlobalSetup
    private int    _sbHandle;
    private byte[] _binGetLength = null!;

    [GlobalSetup]
    public void GlobalSetup()
    {
        Bridge.ClearCaches();

        _sbHandle = Bridge.Dispatch(
            new InvokeRequest("System", "System.Text.StringBuilder", ".ctor", null, null))
            .HandleId();

        _binGetLength = BuildPacket(w =>
        {
            w.WriteByte(0x01);                  // instance call
            w.WriteI32(_sbHandle);
            w.WriteString16("get_Length");
            w.WriteByte(0);
        });

        // Prime all caches so warm benchmarks don't pay first-call cost.
        Bridge.Dispatch(new InvokeRequest("System", "System.Math", "Abs", null, s_arg42));
        Bridge.Dispatch(new InvokeRequest(null, null, "get_Length", _sbHandle, null));
        var r1 = new BinReader(s_binMathAbs.AsSpan());   Bridge.DispatchBin(ref r1);
        var r2 = new BinReader(_binGetLength.AsSpan());  Bridge.DispatchBin(ref r2);
    }

    [GlobalCleanup]
    public void GlobalCleanup() => Bridge.ClearCaches();

    [BenchmarkCategory("StaticCall"), Benchmark(Baseline = true)]
    public int Json_StaticCall()
        => Bridge.Dispatch(new InvokeRequest("System", "System.Math", "Abs", null, s_arg42)).GetHashCode();

    [BenchmarkCategory("StaticCall"), Benchmark]
    public int Bin_StaticCall()
    {
        var r = new BinReader(s_binMathAbs.AsSpan());
        return Bridge.DispatchBin(ref r).GetHashCode();
    }

    [BenchmarkCategory("PropertyGet"), Benchmark(Baseline = true)]
    public int Json_PropertyGet()
        => Bridge.Dispatch(new InvokeRequest(null, null, "get_Length", _sbHandle, null)).GetHashCode();

    [BenchmarkCategory("PropertyGet"), Benchmark]
    public int Bin_PropertyGet()
    {
        var r = new BinReader(_binGetLength.AsSpan());
        return Bridge.DispatchBin(ref r).GetHashCode();
    }

    [BenchmarkCategory("Constructor"), Benchmark(Baseline = true)]
    public void Json_Constructor()
    {
        var result = Bridge.Dispatch(
            new InvokeRequest("System", "System.Text.StringBuilder", ".ctor", null, null));
        Bridge.Dispatch(new InvokeRequest(null, null, "__release", result.HandleId(), null));
    }

    [BenchmarkCategory("Constructor"), Benchmark]
    public void Bin_Constructor()
    {
        var r      = new BinReader(s_binCtorSb.AsSpan());
        var result = Bridge.DispatchBin(ref r);
        Bridge.Dispatch(new InvokeRequest(null, null, "__release", result.HandleId(), null));
    }

    // Fair end-to-end comparison: JSON includes Deserialize + WriteTo(Utf8JsonWriter);
    // binary includes BinReader parsing + WriteAsBin. Both start from raw bytes.

    [BenchmarkCategory("Pipeline"), Benchmark(Baseline = true)]
    public int Json_Pipeline_StaticCall()
        => Bridge.PipelineJson(s_jsonMathAbsBytes).Length;

    [BenchmarkCategory("Pipeline"), Benchmark]
    public int Bin_Pipeline_StaticCall()
        => Bridge.PipelineBinary(s_binMathAbs).Length;

    // [InvocationCount(1)] ensures each iteration is exactly one call so the
    // [IterationSetup] cache clear takes effect for every measurement.

    [IterationSetup(Targets = [nameof(Json_StaticCall_Cold), nameof(Bin_StaticCall_Cold)])]
    public void ClearForCold() => Bridge.ClearCaches();

    [BenchmarkCategory("ColdPath"), Benchmark(Baseline = true)]
    [InvocationCount(1)]
    public int Json_StaticCall_Cold()
        => Bridge.Dispatch(new InvokeRequest("System", "System.Math", "Abs", null, s_arg42)).GetHashCode();

    [BenchmarkCategory("ColdPath"), Benchmark]
    [InvocationCount(1)]
    public int Bin_StaticCall_Cold()
    {
        var r = new BinReader(s_binMathAbs.AsSpan());
        return Bridge.DispatchBin(ref r).GetHashCode();
    }

    private static byte[] BuildPacket(Action<BinWriter> write)
    {
        var buf = new ArrayBufferWriter<byte>(64);
        var w   = new BinWriter(buf);
        write(w);
        return buf.WrittenSpan.ToArray();
    }
}
