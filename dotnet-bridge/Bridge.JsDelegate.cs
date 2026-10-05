using System;
using System.Buffers;
using System.Linq;
using System.Linq.Expressions;
using System.Reflection;
using System.Reflection.Emit;
using System.Runtime.InteropServices;
using System.Threading;

namespace NativeScriptBridge;

public static partial class Bridge
{
    // opcode 0x09: given a delegate type name (or "" for System.Action) and a
    // JS callback id, create a .NET delegate that calls back into V8.
    private static DispatchResult CreateJsDelegate(string typeName, int callbackId)
    {
        var delegateType = string.IsNullOrEmpty(typeName)
            ? typeof(Action)
            : ResolveType(null, typeName)
              ?? throw new TypeLoadException($"Delegate type not found: {typeName}");
        return Box(MakeJsDelegate(delegateType, callbackId));
    }

    // The target a JS-backed delegate is bound to. When the delegate is collected the finalizer
    // tells the runtime, which then drops its reference to the JS function.
    internal sealed class JsCallbackTarget(int callbackId)
    {
        public readonly int CallbackId = callbackId;

        public object? Invoke(object?[] args) => CallJsCallback(CallbackId, args, expectsResult: true);

        public void InvokeVoid(object?[] args) => CallJsCallback(CallbackId, args, expectsResult: false);

        ~JsCallbackTarget()
        {
            ReleaseJsCallback(CallbackId);
        }
    }

    // Function pointer registered by the runtime (RegisterJsCallbackRelease) to drop a JS function
    // whose delegate was collected. Called from the finalizer thread; the runtime queues the id.
    internal static unsafe delegate* unmanaged[Cdecl]<int, void> s_jsCallbackRelease;

    [UnmanagedCallersOnly(EntryPoint = "RegisterJsCallbackRelease",
        CallConvs = [typeof(System.Runtime.CompilerServices.CallConvCdecl)])]
    public static unsafe int RegisterJsCallbackRelease(delegate* unmanaged[Cdecl]<int, void> release)
    {
        s_jsCallbackRelease = release;
        return 0;
    }

    internal static unsafe void ReleaseJsCallback(int callbackId)
    {
        var release = s_jsCallbackRelease;
        if (release == null) return;
        try { release(callbackId); } catch { }
    }

    // One invoker per delegate type: (JsCallbackTarget target, <delegate params>) -> <delegate return>.
    // It packs the arguments into object[] and calls target.Invoke/InvokeVoid; a delegate instance is
    // that invoker closed over its target, so creating a delegate emits no code.
    private static readonly System.Collections.Concurrent.ConcurrentDictionary<Type, DynamicMethod> s_jsDelegateInvokers = new();

    internal static Delegate MakeJsDelegate(Type delegateType, int callbackId)
    {
        var invoker = s_jsDelegateInvokers.GetOrAdd(delegateType, BuildJsDelegateInvoker);
        return invoker.CreateDelegate(delegateType, new JsCallbackTarget(callbackId));
    }

    private static DynamicMethod BuildJsDelegateInvoker(Type delegateType)
    {
        var invokeMethod = delegateType.GetMethod("Invoke")
            ?? throw new MissingMethodException($"No Invoke method on {delegateType}");
        var paramTypes = invokeMethod.GetParameters().Select(p => p.ParameterType).ToArray();
        var returnType = invokeMethod.ReturnType;

        // Emitting IL (rather than compiling an expression tree) is reliable across delegate shapes.
        var dm = new DynamicMethod($"__ns_js_delegate_{delegateType.Name}", returnType,
            [typeof(JsCallbackTarget), .. paramTypes], typeof(Bridge).Module, skipVisibility: true);
        var il = dm.GetILGenerator();

        il.Emit(OpCodes.Ldarg_0);
        il.Emit(OpCodes.Ldc_I4, paramTypes.Length);
        il.Emit(OpCodes.Newarr, typeof(object));
        for (int i = 0; i < paramTypes.Length; i++)
        {
            var paramType = paramTypes[i].IsByRef ? paramTypes[i].GetElementType()! : paramTypes[i];
            il.Emit(OpCodes.Dup);
            il.Emit(OpCodes.Ldc_I4, i);
            il.Emit(OpCodes.Ldarg, i + 1);
            if (paramTypes[i].IsByRef) il.Emit(OpCodes.Ldobj, paramType);
            if (paramType.IsValueType) il.Emit(OpCodes.Box, paramType);
            il.Emit(OpCodes.Stelem_Ref);
        }

        if (returnType == typeof(void))
        {
            il.Emit(OpCodes.Call, typeof(JsCallbackTarget).GetMethod(nameof(JsCallbackTarget.InvokeVoid))!);
        }
        else
        {
            il.Emit(OpCodes.Call, typeof(JsCallbackTarget).GetMethod(nameof(JsCallbackTarget.Invoke))!);
            il.Emit(OpCodes.Ldtoken, returnType);
            il.Emit(OpCodes.Call, typeof(Type).GetMethod(nameof(Type.GetTypeFromHandle))!);
            il.Emit(OpCodes.Call, typeof(Bridge).GetMethod(nameof(ConvertJsResult), BindingFlags.Static | BindingFlags.NonPublic)!);
            il.Emit(OpCodes.Unbox_Any, returnType);
        }
        il.Emit(OpCodes.Ret);
        return dm;
    }

    // A JS callback's result, converted to the type the managed caller expects (a delegate's return
    // type or an overridden member's).
    internal static object? ConvertJsResult(object? value, Type returnType)
    {
        if (value is null)
            return returnType.IsValueType && Nullable.GetUnderlyingType(returnType) is null
                ? Activator.CreateInstance(returnType)
                : null;
        var coerced = CoerceBin(value, returnType);
        if (coerced is null || returnType.IsInstanceOfType(coerced)) return coerced;
        var underlying = Nullable.GetUnderlyingType(returnType) ?? returnType;
        try { return Convert.ChangeType(coerced, underlying); }
        catch (Exception e)
        {
            throw new InvalidCastException(
                $"JavaScript returned {coerced.GetType().Name}, which can't be converted to {returnType.FullName}", e);
        }
    }

    internal static object? CallJsCallback(int id, object?[] args) => CallJsCallback(id, args, expectsResult: true);

    // Calls the JS function registered under `id`. When it throws, a caller that expects a result
    // gets a JsException (a delegate with a return value, an overridden member); a void delegate or
    // event just returns, the error having been reported as an uncaught JS error.
    internal static unsafe object? CallJsCallback(int id, object?[] args, bool expectsResult)
    {
        if (s_jsInvoker == null) return null;

        var buf = new ArrayBufferWriter<byte>(64);
        var w   = new BinWriter(buf);
        w.WriteByte((byte)Math.Min(args.Length, 255));
        foreach (var arg in args)
            WriteCallbackArg(ref w, arg);

        var bytes   = buf.WrittenSpan;
        byte* respPtr = null;
        int   respLen = 0;
        fixed (byte* p = bytes)
            s_jsInvoker(id, p, bytes.Length, &respPtr, &respLen);

        object? result = null;
        // Response is only set for non-void delegates; parse and free when present.
        if (respPtr != null && respLen > 0)
        {
            string? jsError = null;
            try
            {
                var span = new ReadOnlySpan<byte>(respPtr, respLen);
                if (span[0] == 0x0F)
                    jsError = span.Length > 5 ? new BinReader(span[1..]).ReadString32() : "the JavaScript callback could not run";
                else
                    result = ParseCallbackResponse(span);
            }
            catch { result = null; }
            finally
            {
                Marshal.FreeHGlobal((IntPtr)respPtr);
            }
            if (jsError != null && expectsResult) throw new JsException(jsError);
        }

        return result;
    }

    // Serialises a single delegate argument in response-binary tag format so
    // the Rust side can reuse its existing bin_read_value parser.
    private static void WriteCallbackArg(ref BinWriter w, object? arg)
    {
        // Allow existing handle references to be forwarded without re-boxing.
        if (arg is HandleRef hr)
        {
            w.WriteByte(0x06);
            w.WriteI32(hr.Id);
            try
            {
                if (s_handles.TryGetValue(hr.Id, out var obj) && obj != null)
                {
                    var objTypeName = obj.GetType().FullName ?? obj.GetType().Name;
                    w.WriteString16(objTypeName);
                }
                else
                {
                    w.WriteString16("");
                }
            }
            catch
            {
                w.WriteString16("");
            }
            if (Bridge.s_nativePtrs.TryGetValue(hr.Id, out var nativePtr))
            {
                w.WriteByte(1);
                w.WriteI64(nativePtr.ToInt64());
            }
            else
            {
                w.WriteByte(0);
            }
            return;
        }

        if (arg is null)    { w.WriteByte(0x00); return; }
        if (arg is bool b)  { w.WriteByte(b ? (byte)0x02 : (byte)0x01); return; }
        if (arg is int  i)  { w.WriteByte(0x03); w.WriteI32(i); return; }
        if (arg is uint u)  { w.WriteByte(0x03); w.WriteI32((int)u); return; }
        if (arg is long l)  { w.WriteByte(0x04); w.WriteF64((double)l); return; }
        if (arg is float f) { w.WriteByte(0x04); w.WriteF64((double)f); return; }
        if (arg is double d){ w.WriteByte(0x04); w.WriteF64(d); return; }
        if (arg is string s){ w.WriteByte(0x05); w.WriteString32(s); return; }
        if (arg is Enum e)  { w.WriteByte(0x04); w.WriteF64(Convert.ToDouble(e)); return; }
        if (arg is decimal m){ w.WriteByte(0x04); w.WriteF64((double)m); return; }
        // A struct (Windows.Foundation.Size, a C# record struct, ...) arrives as a plain JS object.
        if (arg.GetType().IsValueType && !arg.GetType().IsPrimitive
            && arg is not DateTime && arg is not DateTimeOffset && arg is not TimeSpan)
        {
            string? json = null;
            try { json = System.Text.Json.JsonSerializer.Serialize(arg, arg.GetType(), s_jsJsonOptions); } catch { }
            if (json != null) { w.WriteByte(0x0D); w.WriteString32(json); return; }
        }

        // Object: box in the handle map and send as a handle reference.
        // The JS side receives {__handle, __type} which can be turned into a
        // proxy via NSWinRT.dotnet.fromHandle(...).
        var handleId = Interlocked.Increment(ref s_nextHandle);
        s_handles[handleId] = arg;
        var typeName = arg.GetType().FullName ?? arg.GetType().Name;
        w.WriteByte(0x06);
        w.WriteI32(handleId);
        w.WriteString16(typeName);
        if (Bridge.s_nativePtrs.TryGetValue(handleId, out var nativePtr2)) {
            w.WriteByte(1);
            w.WriteI64(nativePtr2.ToInt64());
        } else {
            w.WriteByte(0);
        }
    }

    private static object? ParseCallbackResponse(ReadOnlySpan<byte> span)
    {
        if (span.IsEmpty) return null;
        var r = new BinReader(span);
        var tag = r.ReadByte();
        switch (tag)
        {
            case 0x00: return null;
            case 0x01: return (object)false;
            case 0x02: return (object)true;
            case 0x03: return (object)r.ReadI32();
            case 0x04: return (object)r.ReadF64();
            case 0x05: return (object)r.ReadString32();
            case 0x06: return (object)new HandleRef(r.ReadI32());
            case 0x0A: return (object)new WinRtRef(r.ReadI64());
            case 0x0D: return new JsJsonValue(r.ReadString32());
            case 0x07: // array: u32 count + N tagged items
            {
                var count = (int)r.ReadU32();
                var arr = new object?[count];
                for (int i = 0; i < count; i++)
                {
                    var t = r.ReadByte();
                    switch (t)
                    {
                        case 0x00: arr[i] = null; break;
                        case 0x01: arr[i] = false; break;
                        case 0x02: arr[i] = true; break;
                        case 0x03: arr[i] = r.ReadI32(); break;
                        case 0x04: arr[i] = r.ReadF64(); break;
                        case 0x05: arr[i] = r.ReadString32(); break;
                        case 0x06: arr[i] = new HandleRef(r.ReadI32()); break;
                        case 0x0A: arr[i] = new WinRtRef(r.ReadI64()); break;
                        default: arr[i] = null; break;
                    }
                }
                return arr;
            }
            default: return null;
        }
    }

    internal static unsafe void CallJsCallbackVoid(int id, object?[] args)
    {
        CallJsCallback(id, args, expectsResult: false);
    }

    
}

/// Thrown into managed code when a JavaScript callback that was expected to return a value threw.
public sealed class JsException(string message) : Exception(message);
