using System;
using System.Buffers;
using System.Collections;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Linq.Expressions;
using System.Reflection;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Threading;
using System.Threading.Tasks;

namespace NativeScriptBridge;

[JsonSerializable(typeof(InvokeRequest))]
[JsonSourceGenerationOptions(PropertyNamingPolicy = JsonKnownNamingPolicy.CamelCase)]
internal partial class BridgeJsonContext : JsonSerializerContext { }

internal sealed record InvokeRequest(
    string?        Assembly,
    string?        TypeName,
    string?        Method,
    int?           Handle,
    JsonElement[]? Args
);

//
// Using Type as the key field (rather than Type.FullName) is correct and faster:
// Type instances are singletons within an AssemblyLoadContext, so reference
// equality and reference hash codes are sufficient and allocation-free.

internal readonly record struct MethodKey(Type Type, string Name, int ArgCount, BindingFlags Flags);
internal readonly record struct PropKey(Type Type, string Name, int PrefixLen, BindingFlags Flags);
internal readonly record struct CtorKey(Type Type, int ArgCount);

internal readonly struct DispatchEntry(Func<object?, object?[], object?>? invoke, ParameterInfo[] parameters)
{
    public readonly Func<object?, object?[], object?>? Invoke     = invoke;
    public readonly ParameterInfo[]                    Parameters = parameters;
    public static readonly DispatchEntry Empty = new(null, []);
}

internal readonly struct CtorEntry(ConstructorInfo? ctor, ParameterInfo[] parameters)
{
    public readonly ConstructorInfo? Ctor       = ctor;
    public readonly ParameterInfo[]  Parameters = parameters;
}

internal enum DispatchKind : byte { Void, Primitive, Handle, Collection, Members, TaskHandle }

internal readonly struct DispatchResult
{
    public static readonly DispatchResult Void =
        new(DispatchKind.Void, null, null, 0, null, null, null, null, null);

    private readonly DispatchKind _kind;
    private readonly object?      _value;
    private readonly Type?        _type;
    private readonly int          _handle;
    private readonly string?      _typeName;
    private readonly string[]?    _methods;
    private readonly string[]?    _properties;
    private readonly string[]?    _staticMethods;
    private readonly string[]?    _staticProperties;
    private readonly string[]?    _readonlyProperties;
    private readonly string[]?    _readonlyStaticProperties;
    private readonly string[]?    _writeonlyProperties;
    private readonly string[]?    _writeonlyStaticProperties;

    private DispatchResult(DispatchKind kind, object? value, Type? type, int handle,
        string? typeName, string[]? methods, string[]? props,
        string[]? staticMethods, string[]? staticProps,
        string[]? readonlyProps = null, string[]? readonlyStaticProps = null,
        string[]? writeonlyProps = null, string[]? writeonlyStaticProps = null)
    {
        _kind = kind; _value = value; _type = type; _handle = handle;
        _typeName = typeName; _methods = methods; _properties = props;
        _staticMethods = staticMethods; _staticProperties = staticProps;
        _readonlyProperties = readonlyProps; _readonlyStaticProperties = readonlyStaticProps;
        _writeonlyProperties = writeonlyProps; _writeonlyStaticProperties = writeonlyStaticProps;
    }

    public static DispatchResult Primitive(object value, Type type)
        => new(DispatchKind.Primitive, value, type, 0, null, null, null, null, null);

    public static DispatchResult Handle(int id, string typeName)
        => new(DispatchKind.Handle, null, null, id, typeName, null, null, null, null);

    public static DispatchResult TaskHandle(int id, string typeName)
        => new(DispatchKind.TaskHandle, null, null, id, typeName, null, null, null, null);

    public static DispatchResult Collection(IEnumerable items)
        => new(DispatchKind.Collection, items, null, 0, null, null, null, null, null);

    public static DispatchResult Members(
        string[] methods, string[] props, string[] staticMethods, string[] staticProps,
        string[] readonlyProps, string[] readonlyStaticProps,
        string[] writeonlyProps, string[] writeonlyStaticProps)
        => new(DispatchKind.Members, null, null, 0, null,
               methods, props, staticMethods, staticProps,
               readonlyProps, readonlyStaticProps, writeonlyProps, writeonlyStaticProps);

    internal int HandleId() => _handle;

    public void WriteTo(Utf8JsonWriter w, JsonSerializerOptions opts)
    {
        switch (_kind)
        {
            case DispatchKind.Void:
                w.WriteNullValue();
                break;

            case DispatchKind.Primitive:
                JsonSerializer.Serialize(w, _value, _type!, opts);
                break;

            case DispatchKind.Handle:
                    w.WriteStartObject();
                    w.WriteNumber("__handle"u8, _handle);
                    w.WriteString("__type"u8,   _typeName);
                    // If the bridge has a canonical native pointer for this handle,
                    // expose it as a hex string so the runtime can detect and use it.
                    if (Bridge.s_nativePtrs.TryGetValue(_handle, out var nativePtr))
                    {
                        try
                        {
                            w.WriteString("__native_ptr"u8, nativePtr.ToInt64().ToString("x"));
                        }
                        catch
                        {
                            // ignore formatting errors
                        }
                    }
                    w.WriteEndObject();
                break;

            case DispatchKind.TaskHandle:
                w.WriteStartObject();
                w.WriteNumber("__handle"u8, _handle);
                w.WriteString("__type"u8,   _typeName);
                w.WriteBoolean("__isTask"u8, true);
                w.WriteEndObject();
                break;

            case DispatchKind.Collection:
                w.WriteStartArray();
                foreach (var item in (IEnumerable)_value!)
                    JsonSerializer.Serialize(w, item, item?.GetType() ?? typeof(object), opts);
                w.WriteEndArray();
                break;

            case DispatchKind.Members:
                w.WriteStartObject();
                WriteStringArray(w, "methods"u8,                  _methods!);
                WriteStringArray(w, "properties"u8,               _properties!);
                WriteStringArray(w, "staticMethods"u8,            _staticMethods!);
                WriteStringArray(w, "staticProperties"u8,         _staticProperties!);
                WriteStringArray(w, "readonlyProperties"u8,       _readonlyProperties!);
                WriteStringArray(w, "readonlyStaticProperties"u8, _readonlyStaticProperties!);
                WriteStringArray(w, "writeonlyProperties"u8,      _writeonlyProperties!);
                WriteStringArray(w, "writeonlyStaticProperties"u8,_writeonlyStaticProperties!);
                w.WriteEndObject();
                break;
        }
    }

    private static void WriteStringArray(Utf8JsonWriter w, ReadOnlySpan<byte> name, string[] arr)
    {
        w.WriteStartArray(name);
        foreach (var s in arr) w.WriteStringValue(s);
        w.WriteEndArray();
    }

    //
    // Response tags:
    //   0x00 = null   0x01 = false   0x02 = true
    //   0x03 = i32[4]   0x04 = f64[8]   0x05 = string[u32+utf8]
    //   0x06 = handle[i32 + u16+utf8 type]
    //   0x07 = array[u32 count + N tagged items]
    //   0x08 = members{ methods, props, staticMethods, staticProps }
    //   0xFF = error[u32+utf8]

    public void WriteAsBin(ArrayBufferWriter<byte> outBuf)
    {
        var w = new BinWriter(outBuf);
        switch (_kind)
        {
            case DispatchKind.Void:
                w.WriteByte(0x00);
                break;

            case DispatchKind.Primitive:
                WritePrimitiveBin(ref w, _value!);
                break;

            case DispatchKind.Handle:
                w.WriteByte(0x06);
                w.WriteI32(_handle);
                w.WriteString16(_typeName ?? "");
                // Optionally include a canonical native pointer for this handle
                // so binary clients (Rust runtime) can avoid an extra bridge
                // round-trip when a COM pointer is available.
                if (Bridge.s_nativePtrs.TryGetValue(_handle, out var nativePtr)) {
                    w.WriteByte(1);
                    w.WriteI64(nativePtr.ToInt64());
                } else {
                    w.WriteByte(0);
                }
                break;

            case DispatchKind.TaskHandle:
                w.WriteByte(0x0C);
                w.WriteI32(_handle);
                w.WriteString16(_typeName ?? "");
                if (Bridge.s_nativePtrs.TryGetValue(_handle, out var nativePtr2)) {
                    w.WriteByte(1);
                    w.WriteI64(nativePtr2.ToInt64());
                } else {
                    w.WriteByte(0);
                }
                break;

            case DispatchKind.Collection:
                var items = new List<object?>();
                foreach (var item in (IEnumerable)_value!) items.Add(item);
                w.WriteByte(0x07);
                w.WriteU32((uint)items.Count);
                foreach (var item in items) WriteValueBin(ref w, item);
                break;

            case DispatchKind.Members:
                w.WriteByte(0x08);
                WriteStringArrayBin(ref w, _methods!);
                WriteStringArrayBin(ref w, _properties!);
                WriteStringArrayBin(ref w, _staticMethods!);
                WriteStringArrayBin(ref w, _staticProperties!);
                WriteStringArrayBin(ref w, _readonlyProperties!);
                WriteStringArrayBin(ref w, _readonlyStaticProperties!);
                WriteStringArrayBin(ref w, _writeonlyProperties!);
                WriteStringArrayBin(ref w, _writeonlyStaticProperties!);
                break;
        }
    }

    private static void WritePrimitiveBin(ref BinWriter w, object value)
    {
        switch (value)
        {
            case bool    b: w.WriteByte(b ? (byte)0x02 : (byte)0x01); break;
            case sbyte   v: w.WriteByte(0x03); w.WriteI32(v); break;
            case byte    v: w.WriteByte(0x03); w.WriteI32(v); break;
            case short   v: w.WriteByte(0x03); w.WriteI32(v); break;
            case ushort  v: w.WriteByte(0x03); w.WriteI32(v); break;
            case int     v: w.WriteByte(0x03); w.WriteI32(v); break;
            case uint    v: w.WriteByte(0x04); w.WriteF64(v); break;
            case long    v: w.WriteByte(0x04); w.WriteF64(v); break;
            case ulong   v: w.WriteByte(0x04); w.WriteF64(v); break;
            case float   v: w.WriteByte(0x04); w.WriteF64(v); break;
            case double  v: w.WriteByte(0x04); w.WriteF64(v); break;
            case decimal v: w.WriteByte(0x04); w.WriteF64((double)v); break;
            case string  s: w.WriteByte(0x05); w.WriteString32(s); break;
            default:        w.WriteByte(0x05); w.WriteString32(value.ToString() ?? ""); break;
        }
    }

    private static void WriteValueBin(ref BinWriter w, object? value)
    {
        if (value is null) { w.WriteByte(0x00); return; }
        var t = value.GetType();
        if (t.IsPrimitive || t == typeof(string) || t == typeof(decimal))
        {
            WritePrimitiveBin(ref w, value);
            return;
        }
        if (value is Array enumerable)
        {
            var items = new List<object?>();
            foreach (var item in enumerable) items.Add(item);
            w.WriteByte(0x07);
            w.WriteU32((uint)items.Count);
            foreach (var item in items) WriteValueBin(ref w, item);
            return;
        }
        var id = Interlocked.Increment(ref Bridge.s_nextHandle);
        Bridge.s_handles[id] = value;
        w.WriteByte(0x06);
        w.WriteI32(id);
        w.WriteString16(t.FullName ?? t.Name);
        w.WriteByte(0); // no native ptr
    }

    private static void WriteStringArrayBin(ref BinWriter w, string[] arr)
    {
        w.WriteU16((ushort)arr.Length);
        foreach (var s in arr) w.WriteString16(s);
    }
}

internal static class TaskResultCache
{
    private static readonly ConcurrentDictionary<Type, Func<Task, object?>> s_cache = new();

    public static object? GetResult(Task task)
    {
        var taskType = task.GetType();
        if (!taskType.IsGenericType) return null;
        var fn = s_cache.GetOrAdd(taskType, static t =>
        {
            var param = Expression.Parameter(typeof(Task));
            Expression body = Expression.Property(Expression.Convert(param, t), "Result");
            if (body.Type.IsValueType) body = Expression.Convert(body, typeof(object));
            return Expression.Lambda<Func<Task, object?>>(body, param).Compile();
        });
        return fn(task);
    }
}

internal static class ValueTaskAsTaskCache
{
    private static readonly ConcurrentDictionary<Type, Func<object, Task>> s_cache = new();

    public static Task AsTask(Type valueTaskType, object vt)
    {
        var fn = s_cache.GetOrAdd(valueTaskType, static t =>
        {
            var param = Expression.Parameter(typeof(object));
            var cast  = Expression.Convert(param, t);
            var call  = Expression.Call(cast, t.GetMethod("AsTask")!);
            return Expression.Lambda<Func<object, Task>>(call, param).Compile();
        });
        return fn(vt);
    }
}
