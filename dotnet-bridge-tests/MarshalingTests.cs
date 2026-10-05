using System;
using System.Buffers;
using System.Collections.Generic;
using System.Linq;
using NativeScriptBridge;
using Xunit;

namespace DotNetBridgeTests;

public static class MarshalingFixtures
{
    public static int Sum(params int[] values) => values.Sum();
    public static string Describe(string label, params object[] values) => label + ":" + values.Length;
    public static int Total(List<int> values) => values.Sum();
    public static int Count(IEnumerable<string> values) => values.Count();
    public static List<string> Names() => ["a", "b"];
    public static string[] NameArray() => ["x", "y"];
    public static DateTime Epoch() => new(2020, 1, 2, 3, 4, 5, DateTimeKind.Utc);
    public static int YearOf(DateTime value) => value.Year;
    public static double HoursOf(DateTimeOffset value) => value.UtcDateTime.Hour;
    public static int GuidVersion(Guid value) => value.Version;
}

[Collection("Bridge")]
/// Arrays and params arrays as arguments, collections and date/time values as results.
public sealed class MarshalingTests : IDisposable
{
    public MarshalingTests() => Bridge.ClearCaches();
    public void Dispose()     => Bridge.ClearCaches();

    private const string Fixtures = "DotNetBridgeTests.MarshalingFixtures";

    [Fact]
    public void ParamsArray_TakesTrailingArguments()
    {
        var result = Static("System.String", "Join", w =>
        {
            w.WriteByte(0x05); w.WriteString16(",");
            w.WriteByte(0x05); w.WriteString16("a");
            w.WriteByte(0x05); w.WriteString16("b");
            w.WriteByte(0x05); w.WriteString16("c");
        }, 4);
        Assert.Equal("a,b,c", result.PrimitiveValue());
    }

    [Fact]
    public void ParamsArray_ConvertsToElementType()
    {
        var result = Static(Fixtures, "Sum", w =>
        {
            w.WriteByte(0x03); w.WriteI32(1);
            w.WriteByte(0x03); w.WriteI32(2);
            w.WriteByte(0x03); w.WriteI32(3);
        }, 3);
        Assert.Equal(6, result.PrimitiveValue());
    }

    [Fact]
    public void ParamsArray_CanBeEmpty()
    {
        var result = Static(Fixtures, "Describe", w => { w.WriteByte(0x05); w.WriteString16("none"); });
        Assert.Equal("none:0", result.PrimitiveValue());
    }

    [Fact]
    public void ArrayArgument_ForArrayParameter()
    {
        var result = Static("System.String", "Join", w =>
        {
            w.WriteByte(0x05); w.WriteString16("-");
            WriteArray(w, "a", "b");
        }, 2);
        Assert.Equal("a-b", result.PrimitiveValue());
    }

    [Fact]
    public void ArrayArgument_ForListParameter()
    {
        var result = Static(Fixtures, "Total", w =>
        {
            w.WriteByte(0x07); w.WriteU32(3);
            w.WriteByte(0x03); w.WriteI32(4);
            w.WriteByte(0x03); w.WriteI32(5);
            w.WriteByte(0x04); w.WriteF64(6);
        });
        Assert.Equal(15, result.PrimitiveValue());
    }

    [Fact]
    public void ArrayArgument_ForEnumerableParameter()
    {
        var result = Static(Fixtures, "Count", w => WriteArray(w, "a", "b", "c"));
        Assert.Equal(3, result.PrimitiveValue());
    }

    [Fact]
    public void ListResult_IsAnObjectNotACopy()
    {
        var result = Static(Fixtures, "Names", null, 0);
        Assert.Equal(DispatchKind.Handle, result.Kind());
        Assert.True(Bridge.s_handles.TryGetValue(result.HandleId(), out var list));
        Assert.IsType<List<string>>(list);
    }

    [Fact]
    public void ArrayResult_IsStillACopy()
    {
        Assert.Equal(DispatchKind.Collection, Static(Fixtures, "NameArray", null, 0).Kind());
    }

    [Fact]
    public void CollectionHelpers_ReadAndWriteAList()
    {
        var list = new List<int> { 10, 20, 30 };
        Assert.Equal(7, Bridge.CollectionKind(list));
        Assert.Equal(3, Bridge.CollectionCount(list));
        Assert.Equal(20, Bridge.ItemAt(list, 1));
        Assert.Null(Bridge.ItemAt(list, 3));
        Bridge.SetItemAt(list, 1, 25.0);
        Assert.Equal(25, list[1]);
        Assert.Equal(new object?[] { 10, 25, 30 }, Bridge.CollectionItems(list));
    }

    [Fact]
    public void CollectionHelpers_ReadOnlyAndEnumerables()
    {
        IReadOnlyList<string> readOnly = new[] { "a" }.AsReadOnly();
        Assert.Equal(7, Bridge.CollectionKind(readOnly));
        Assert.Equal("a", Bridge.ItemAt(readOnly, 0));
        Assert.Equal(1, Bridge.CollectionKind(Enumerable.Range(0, 3).Select(i => i)));
        Assert.Equal(3, Bridge.CollectionCount(Enumerable.Range(0, 3).Select(i => i)));
        Assert.Equal(0, Bridge.CollectionKind("text"));
        Assert.Equal(0, Bridge.CollectionKind(new object()));
    }

    [Fact]
    public void DateTimeResult_IsAnObject()
    {
        var result = Static(Fixtures, "Epoch", null, 0);
        Assert.Equal(DispatchKind.Handle, result.Kind());
        Assert.True(Bridge.s_handles.TryGetValue(result.HandleId(), out var value));
        Assert.Equal(2020, ((DateTime)value!).Year);
    }

    [Fact]
    public void IsoString_ForDateTimeParameter()
    {
        var result = Static(Fixtures, "YearOf", w => { w.WriteByte(0x05); w.WriteString16("2031-05-06T07:08:09.000Z"); });
        Assert.Equal(2031, result.PrimitiveValue());
    }

    [Fact]
    public void IsoString_ForDateTimeOffsetParameter()
    {
        var result = Static(Fixtures, "HoursOf", w => { w.WriteByte(0x05); w.WriteString16("2031-05-06T07:08:09.000Z"); });
        Assert.Equal(7.0, result.PrimitiveValue());
    }

    [Fact]
    public void String_ForGuidParameter()
    {
        var result = Static(Fixtures, "GuidVersion", w => { w.WriteByte(0x05); w.WriteString16(Guid.NewGuid().ToString()); });
        Assert.Equal(4, result.PrimitiveValue());
    }

    [Fact]
    public void IndexerAccessor_WithArgument_IsAMethodCall()
    {
        var list = new List<string> { "first" };
        var id = 900001;
        Bridge.s_handles[id] = list;
        var buf = new ArrayBufferWriter<byte>(64);
        var w = new BinWriter(buf);
        w.WriteByte(0x01);
        w.WriteI32(id);
        w.WriteString16("get_Item");
        w.WriteByte(1);
        w.WriteByte(0x03); w.WriteI32(0);
        var r = new BinReader(buf.WrittenSpan);
        Assert.Equal("first", Bridge.DispatchBin(ref r).PrimitiveValue());
    }

    [Fact]
    public void Indexers_AreNotListedAsProperties()
    {
        var buf = new ArrayBufferWriter<byte>(64);
        Bridge.BuildMembersResult(typeof(List<int>)).WriteAsBin(buf);
        Assert.Equal(0x08, buf.WrittenSpan[0]);
        var methods = ReadStringArray(buf.WrittenSpan[1..], out var consumed);
        var properties = ReadStringArray(buf.WrittenSpan[(1 + consumed)..], out _);
        Assert.Contains("Count", properties);
        Assert.DoesNotContain("Item", properties);
        Assert.Contains("Add", methods);
    }

    private static string[] ReadStringArray(ReadOnlySpan<byte> span, out int consumed)
    {
        var count = BitConverter.ToUInt16(span);
        var pos = 2;
        var names = new string[count];
        for (int i = 0; i < count; i++)
        {
            var len = BitConverter.ToUInt16(span[pos..]);
            names[i] = System.Text.Encoding.UTF8.GetString(span.Slice(pos + 2, len));
            pos += 2 + len;
        }
        consumed = pos;
        return names;
    }

    private static void WriteArray(BinWriter w, params string[] items)
    {
        w.WriteByte(0x07);
        w.WriteU32((uint)items.Length);
        foreach (var item in items) { w.WriteByte(0x05); w.WriteString16(item); }
    }

    private static DispatchResult Static(string type, string method, Action<BinWriter>? args, int argCount = 1)
    {
        var buf = new ArrayBufferWriter<byte>(64);
        var w = new BinWriter(buf);
        w.WriteByte(0x02);
        w.WriteString16(type);
        w.WriteString16("");
        w.WriteString16(method);
        w.WriteByte((byte)argCount);
        args?.Invoke(w);
        var r = new BinReader(buf.WrittenSpan);
        return Bridge.DispatchBin(ref r);
    }
}
