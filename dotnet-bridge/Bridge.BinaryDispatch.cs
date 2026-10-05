using System;
using System.Buffers;
using System.Collections.Generic;
using System.Linq;
using System.Reflection;
using System.Runtime.InteropServices;

namespace NativeScriptBridge;

public static partial class Bridge
{
    // Safe to reuse per thread: the response is copied to unmanaged memory
    // before InvokeBinary returns.
    [ThreadStatic]
    private static ArrayBufferWriter<byte>? t_responseWriter;

    [UnmanagedCallersOnly(EntryPoint = "InvokeBinary",
        CallConvs = [typeof(System.Runtime.CompilerServices.CallConvCdecl)])]
    public static unsafe int InvokeBinary(
        byte* requestPtr, int requestLen,
        byte** responsePtr, int* responseLenPtr)
    {
        try
        {
            var r   = new BinReader(new ReadOnlySpan<byte>(requestPtr, requestLen));
            var res = DispatchBin(ref r);
            var buf = t_responseWriter ??= new ArrayBufferWriter<byte>(256);
            buf.ResetWrittenCount();
            res.WriteAsBin(buf);
            WriteResponse(buf.WrittenSpan, responsePtr, responseLenPtr);
        }
        catch (Exception ex)
        {
            WriteBinError(Unwrap(ex).Message, responsePtr, responseLenPtr);
        }
        return 0;
    }

    // Per-thread unmanaged buffer InvokeBinary responses are written to, so a call allocates none.
    // The runtime reads a response and hands it to Free (which leaves this buffer alone) before it
    // makes another call on the thread, including a nested one from a JS callback, so reusing it
    // is safe. Large responses still get their own allocation, keeping the buffer small.
    [ThreadStatic]
    private static unsafe byte* t_responseBuffer;
    [ThreadStatic]
    private static int t_responseCapacity;
    private const int MaxReusedResponse = 64 * 1024;

    private static unsafe void WriteResponse(ReadOnlySpan<byte> bytes, byte** outPtr, int* outLen)
    {
        var needed = bytes.Length + 1;
        if (needed > MaxReusedResponse)
        {
            WriteUnmanaged(bytes, outPtr, outLen);
            return;
        }
        if (needed > t_responseCapacity)
        {
            var capacity = Math.Max(256, (int)System.Numerics.BitOperations.RoundUpToPowerOf2((uint)needed));
            t_responseBuffer = (byte*)NativeMemory.Realloc(t_responseBuffer, (nuint)capacity);
            t_responseCapacity = capacity;
        }
        var p = t_responseBuffer;
        bytes.CopyTo(new Span<byte>(p, bytes.Length));
        p[bytes.Length] = 0;
        *outPtr = p;
        *outLen = bytes.Length;
    }

    internal static unsafe bool IsResponseBuffer(byte* ptr) => ptr == t_responseBuffer;

    internal static DispatchResult DispatchBin(ref BinReader r)
    {
        var op = r.ReadByte();

            if (op == 0x04) // release
            {
                var handleToRemove = r.ReadI32();
                s_handles.TryRemove(handleToRemove, out _);
                if (s_nativePtrs.TryRemove(handleToRemove, out var nativePtr))
                {
                    RemoveNativePtr(handleToRemove, nativePtr);
                    try
                    {
                        Marshal.Release(nativePtr);
                    }
                    catch
                    {
                    }
                }
                return DispatchResult.Void;
            }

        if (op == 0x05) // members by handle
        {
            var h = r.ReadI32();
            if (!s_handles.TryGetValue(h, out var obj))
                throw new KeyNotFoundException($"Invalid handle {h}");
            return BuildMembersResult(
                obj?.GetType() ?? throw new InvalidOperationException("Handle is null"));
        }

        if (op == 0x06) // members by type
        {
            var typeName = r.ReadName16();
            var assembly = r.ReadName16();
            var type = ResolveType(NullIfEmpty(assembly), typeName)
                ?? throw new TypeLoadException($"Type not found: {typeName} (assembly: {assembly})");
            return BuildMembersResult(type);
        }

        if (op == 0x01) // instance call
        {
            var handle = r.ReadI32();
            if (!s_handles.TryGetValue(handle, out var target))
                throw new KeyNotFoundException($"Invalid handle {handle}");
            var type   = target?.GetType() ?? throw new InvalidOperationException("Handle is null");
            var method = r.ReadName16();
            var args   = r.ReadArgs();

            if (method == "__dotnet_await__" && args.Length == 2
                && args[0] is int resolveId && args[1] is int rejectId)
            {
                ScheduleTaskContinuation(handle, resolveId, rejectId);
                return DispatchResult.Void;
            }

            // A JS subclass instance whose JS object was collected: its dispatcher makes a new one
            // for this handle.
            if (method == JsSelfMember && target is not null && s_proxyCallbacks.TryGetValue(target, out var jsHolder))
            {
                CallJsCallback(jsHolder.CallbackId, [new HandleRef(handle), JsSelfMember], expectsResult: false);
                return DispatchResult.Void;
            }

            return DispatchCallBin(target, type, method, args, isStatic: false);
        }

        if (op == 0x09) // create JS delegate
        {
            var delTypeName  = r.ReadName16(); // "" → System.Action
            var callbackId   = r.ReadI32();
            return CreateJsDelegate(delTypeName, callbackId);
        }

        if (op == 0x0A) // create JS-backed subclass instance
        {
            var assembly = r.ReadName16();
            var typeName = r.ReadName16();

            var interfaceCount = r.ReadI32();
            var interfaceNames = interfaceCount > 0 ? new string[interfaceCount] : [];
            for (int i = 0; i < interfaceCount; i++) interfaceNames[i] = r.ReadName16();

            var memberCount = r.ReadI32();
            var memberNames = memberCount > 0 ? new string[memberCount] : [];
            for (int i = 0; i < memberCount; i++) memberNames[i] = r.ReadName16();

            var callbackId = r.ReadI32();
            var ctorArgs = r.HasMore ? r.ReadArgs() : [];
            return CreateJsSubclass(NullIfEmpty(assembly), typeName, callbackId, interfaceNames, memberNames, ctorArgs);
        }

        if (op == 0x0B) // get CLR-only property by raw IInspectable ptr (CLR reflection fallback)
        {
            var instancePtr = new IntPtr(r.ReadI64());
            var propName    = r.ReadName16();
            return ClrGetProperty(instancePtr, propName);
        }

        // Static ops: 0x02 = call, 0x03 = constructor
        var typeNameS = r.ReadName16();
        var assemblyS = r.ReadName16();
        var typeS = ResolveType(NullIfEmpty(assemblyS), typeNameS)
            ?? throw new TypeLoadException($"Type not found: {typeNameS} (assembly: {assemblyS})");

        if (op == 0x03) // constructor
        {
            var args  = r.ReadArgs();
            // Constructors with the same parameter count (StringBuilder(int) / StringBuilder(string)):
            // the one whose parameter types best fit the arguments.
            var ctors = GetCtorOverloads(typeS, args.Length);
            if (ctors.Ctors.Length > 1)
            {
                var best = SelectOverload(ctors.Parameters, args);
                object?[]? built = null;
                if (best >= 0)
                {
                    try { built = BuildArgsBinExact(args, ctors.Parameters[best]); }
                    catch { /* fall back to the first constructor by arity below */ }
                }
                if (built is not null && ArgsFit(built, ctors.Parameters[best]))
                    return Box(ctors.Ctors[best].Invoke(built));
            }
            var entry = GetCachedCtor(typeS, args.Length);
            if (entry.Ctor is null)
                throw new MissingMethodException(
                    $"No public ctor on {typeS.FullName} for {args.Length} args");
            // ConstructorInfo.Invoke requires exact arg count — build a precise array.
            var ctorArgs = BuildArgsBinExact(args, entry.Parameters);
            return Box(entry.Ctor.Invoke(ctorArgs));
        }

        // op == 0x02: static call
        var methodS = r.ReadName16();
        var argsS   = r.ReadArgs();
        return DispatchCallBin(null, typeS, methodS, argsS, isStatic: true);
    }

    private static DispatchResult DispatchCallBin(
        object? target, Type type, string method, object?[] args, bool isStatic)
    {
        var flags = (isStatic ? BindingFlags.Static : BindingFlags.Instance) | BindingFlags.Public;

        

        if (method.Length > 4 && args.Length == 0
            && method[0] == 'g' && method[1] == 'e' && method[2] == 't' && method[3] == '_')
        {
            var prop = GetCachedProp(type, method, 4, flags);
            if (prop is not null) return Box(prop.GetValue(target));
        }

        if (method.Length > 4
            && method[0] == 's' && method[1] == 'e' && method[2] == 't' && method[3] == '_'
            && args.Length == 1)
        {
            var prop = GetCachedProp(type, method, 4, flags);
            if (prop is not null)
            {
                prop.SetValue(target, CoerceBin(args[0], prop.PropertyType));
                return DispatchResult.Void;
            }
        }

        // Overloads with the same parameter count (Abs(int) / Abs(double), Describe(Animal) /
        // Describe(Shape)): the one whose parameter types best fit the arguments.
        var overloads = GetOverloads(type, method, args.Length, flags);
        var best = overloads.Methods.Length > 0 ? overloads.Select(args) : -1;
        // No method with this many parameters takes these arguments: a params method may
        // (String.Join(",", "a", "b", "c")).
        if (best < 0 && TryParamsCall(target, type, method, args, flags, out var paramsResult))
            return paramsResult;
        if (overloads.Methods.Length > 1)
        {
            if (best >= 0)
            {
                var chosen = overloads.Entry(best, type);
                object?[]? built = null;
                try { built = BuildArgsBin(args, chosen.Parameters); }
                catch { /* an argument didn't convert: try the overloads one by one below */ }
                if (built is not null)
                {
                    if (ArgsFit(built, chosen.Parameters))
                        return InvokeBuilt(target, type, method, chosen, built);
                    if (built.Length > 0) ReturnArgs(built);
                }
            }
            // Nothing fits by type: the first overload the arguments convert to.
            foreach (var overload in overloads.Methods)
            {
                var ps = overload.GetParameters();
                object?[] built;
                try { built = BuildArgsBinExact(args, ps); }
                catch { continue; }
                if (!ArgsFit(built, ps)) continue;
                return Box(overload.Invoke(target, built));
            }
        }

        var entry = GetCachedMethod(type, method, args.Length, flags);
        if (entry.Invoke is null && target is not null && IsJsBacked(target))
        {
            // A JS subclass may call the protected members it inherits.
            var protectedFlags = BindingFlags.Instance | BindingFlags.NonPublic;
            var inherited = type.GetMethods(protectedFlags)
                .FirstOrDefault(m => m.Name == method && (m.IsFamily || m.IsFamilyOrAssembly) && m.GetParameters().Length == args.Length);
            if (inherited is not null)
            {
                var built = BuildArgsBinExact(args, inherited.GetParameters());
                return Box(inherited.Invoke(target, built));
            }
        }
        if (entry.Invoke is null)
        {
            var candidates = type.GetMethods(flags).Where(m => m.Name == method && !m.IsSpecialName);
            foreach (var m in candidates)
            {
                var parameters = m.GetParameters();
                var built = BuildArgsBin(args, parameters);
                try
                {
                    var res = (m.Invoke(target, built));
                    return Box(res);
                }
                catch (TargetInvocationException tie) when (IsMarshaledForDifferentThread(tie.InnerException))
                {
                    if (Bridge.IsLogToConsole()) Console.Error.WriteLine($"[Bridge] Detected wrong-thread COM error; retrying {type.FullName}.{m.Name} on UI thread");
                    try
                    {
                        var res = InvokeOnUIThread(() => (m.Invoke(target, built)));
                        return Box(res);
                    }
                    catch { /* retry failed — try next candidate */ }
                }
                catch (System.Runtime.InteropServices.COMException ce) when (IsMarshaledForDifferentThread(ce))
                {
                    if (Bridge.IsLogToConsole()) Console.Error.WriteLine($"[Bridge] Detected COMException wrong-thread; retrying {type.FullName}.{m.Name} on UI thread");
                    try
                    {
                        var res = InvokeOnUIThread(() => (m.Invoke(target, built)));
                        return Box(res);
                    }
                    catch { /* retry failed — try next candidate */ }
                }
                finally { if (built.Length > 0) ReturnArgs(built); }
            }
            throw new MissingMethodException(
                $"Method '{method}' ({args.Length} args) not found on {type.FullName}");
        }

        return InvokeBuilt(target, type, method, entry, BuildArgsBin(args, entry.Parameters));
    }

    // Runs a compiled invoker on arguments from BuildArgsBin (returned to the pool afterwards),
    // retrying on the UI thread when a COM object rejects the calling thread.
    private static DispatchResult InvokeBuilt(object? target, Type type, string method, DispatchEntry entry, object?[] builtArgs)
    {
        try {
            try
            {
                var res = (entry.Invoke!(target, builtArgs));
                return Box(res);
            }
            catch (TargetInvocationException tie) when (IsMarshaledForDifferentThread(tie.InnerException))
            {
                if (Bridge.IsLogToConsole()) Console.Error.WriteLine($"[Bridge] Detected wrong-thread COM error; retrying {type.FullName}.{method} on UI thread");
                var res = InvokeOnUIThread(() => (entry.Invoke!(target, builtArgs)));
                return Box(res);
            }
            catch (System.Runtime.InteropServices.COMException ce) when (IsMarshaledForDifferentThread(ce))
            {
                if (Bridge.IsLogToConsole()) Console.Error.WriteLine($"[Bridge] Detected COMException wrong-thread; retrying {type.FullName}.{method} on UI thread");
                var res = InvokeOnUIThread(() => (entry.Invoke!(target, builtArgs)));
                return Box(res);
            }
        }
        finally { if (builtArgs.Length > 0) ReturnArgs(builtArgs); }
    }

    private static readonly System.Collections.Concurrent.ConcurrentDictionary<MethodKey, OverloadSet> s_overloadCache = new();

    private static OverloadSet GetOverloads(Type type, string name, int argCount, BindingFlags flags)
        => s_overloadCache.GetOrAdd(new MethodKey(type, name, argCount, flags), static k =>
            new OverloadSet(k.Type.GetMethods(k.Flags)
                .Where(m => m.Name == k.Name && !m.IsGenericMethodDefinition && m.GetParameters().Length == k.ArgCount)
                .ToArray()));

    // The public methods of a type sharing a name and parameter count, with their parameters and
    // (built on first use) compiled invokers.
    private sealed class OverloadSet(MethodInfo[] methods)
    {
        public readonly MethodInfo[] Methods = methods;
        private readonly ParameterInfo[][] _parameters = Array.ConvertAll(methods, m => m.GetParameters());
        private readonly DispatchEntry?[] _entries = new DispatchEntry?[methods.Length];

        public DispatchEntry Entry(int index, Type type) => _entries[index] ??= BuildDispatchEntry(type, Methods[index]);

        public int Select(object?[] args) => SelectOverload(_parameters, args);
    }

    // The public constructors of a type with a given parameter count.
    private sealed class CtorSet(ConstructorInfo[] ctors)
    {
        public readonly ConstructorInfo[] Ctors = ctors;
        public readonly ParameterInfo[][] Parameters = Array.ConvertAll(ctors, c => c.GetParameters());
    }

    private static readonly System.Collections.Concurrent.ConcurrentDictionary<CtorKey, CtorSet> s_ctorOverloadCache = new();

    private static CtorSet GetCtorOverloads(Type type, int argCount)
        => s_ctorOverloadCache.GetOrAdd(new CtorKey(type, argCount), static k =>
            new CtorSet(k.Type.GetConstructors(BindingFlags.Public | BindingFlags.Instance)
                .Where(c => c.GetParameters().Length == k.ArgCount)
                .ToArray()));

    // The candidate (by its parameter list) the arguments fit best, or -1 when none can take them.
    // Ties go to the first.
    private static int SelectOverload(ParameterInfo[][] candidates, object?[] args)
    {
        int best = -1, bestScore = -1;
        for (int m = 0; m < candidates.Length; m++)
        {
            var ps = candidates[m];
            int total = 0;
            for (int i = 0; i < ps.Length; i++)
            {
                var score = MatchScore(i < args.Length ? args[i] : null, ps[i].ParameterType);
                if (score < 0) { total = -1; break; }
                total += score;
            }
            if (total > bestScore) { bestScore = total; best = m; }
        }
        return best;
    }

    // How well a bridged argument fits a parameter type: higher is better, -1 when it can't be
    // passed. An exact type beats a widening conversion, which beats a narrowing one the value
    // fits, which beats object. So Abs(-0.5) is Abs(double) and Abs(-128) is Abs(int), never
    // Abs(sbyte).
    private static int MatchScore(object? arg, Type p)
    {
        if (p.IsByRef || p.IsByRefLike || p.IsPointer) return -1;
        var underlying = Nullable.GetUnderlyingType(p);
        if (arg is null) return !p.IsValueType || underlying is not null ? 1 : -1;
        if (underlying is not null) p = underlying;
        switch (arg)
        {
            case int i:
                if (p == typeof(int)) return 10;
                if (p == typeof(long)) return 9;
                if (p == typeof(double)) return 8;
                if (p == typeof(float) || p == typeof(decimal)) return 7;
                if (p.IsEnum) return 6;
                if (IsIntegerType(p)) return FitsInteger(i, p) ? 4 : -1;
                return p.IsInstanceOfType(arg) ? 2 : -1;
            case double d:
                if (p == typeof(double)) return 10;
                if (p == typeof(float)) return 8;
                if (p == typeof(decimal)) return 7;
                // Integral numbers that fit an int arrive as int; this is a larger one (3e9).
                if (IsIntegerType(p)) return Math.Floor(d) == d && FitsInteger(d, p) ? 4 : -1;
                return p.IsInstanceOfType(arg) ? 2 : -1;
            case bool:
                return p == typeof(bool) ? 10 : p.IsInstanceOfType(arg) ? 2 : -1;
            case string s:
                if (p == typeof(string)) return 10;
                if (p == typeof(char)) return s.Length == 1 ? 6 : -1;
                if (IsParsedFromString(p)) return 5;
                return p.IsInstanceOfType(arg) ? 2 : -1;
            case JsArrayValue:
                if (p.IsArray) return 8;
                if (p == typeof(string)) return -1;
                if (typeof(System.Collections.IEnumerable).IsAssignableFrom(p)) return 6;
                return p == typeof(object) ? 1 : -1;
            case HandleRef h:
                s_handles.TryGetValue(h.Id, out var obj);
                if (obj is null) return p.IsValueType ? -1 : 1;
                if (obj.GetType() == p) return 10;
                return p.IsInstanceOfType(obj) ? (p == typeof(object) ? 2 : 8) : -1;
            case JsFunctionRef:
                if (typeof(Delegate).IsAssignableFrom(p))
                    return p == typeof(Delegate) || p == typeof(MulticastDelegate) ? 4 : 8;
                return p == typeof(object) ? 1 : -1;
            case WinRtRef:
                if (p == typeof(object)) return 2;
                return p.IsInterface || (p.IsClass && p != typeof(string)) ? 4 : -1;
            case JsJsonValue:
                if (p == typeof(object)) return 1;
                if (p == typeof(string)) return 2;
                return p.IsPrimitive || p.IsEnum || typeof(Delegate).IsAssignableFrom(p) ? -1 : 5;
            default:
                return p.IsInstanceOfType(arg) ? 2 : -1;
        }
    }

    // Methods whose last parameter is a params array, by type, name and binding flags.
    private static readonly System.Collections.Concurrent.ConcurrentDictionary<(Type, string, BindingFlags), MethodInfo[]> s_paramsCache = new();

    // Calls the params method (`Join(string, params string[])`) the arguments fit best, the
    // trailing ones packed into its params array. False when there is none they fit.
    private static bool TryParamsCall(object? target, Type type, string method, object?[] args, BindingFlags flags, out DispatchResult result)
    {
        result = default;
        var candidates = s_paramsCache.GetOrAdd((type, method, flags), static k =>
            k.Item1.GetMethods(k.Item3)
                .Where(m => m.Name == k.Item2 && !m.IsGenericMethodDefinition
                    && m.GetParameters() is { Length: > 0 } ps
                    && ps[^1].ParameterType.IsArray
                    && ps[^1].IsDefined(typeof(ParamArrayAttribute), false))
                .ToArray());
        if (candidates.Length == 0) return false;

        MethodInfo? best = null;
        int bestScore = -1;
        foreach (var m in candidates)
        {
            var ps = m.GetParameters();
            var fixedCount = ps.Length - 1;
            if (args.Length < fixedCount) continue;
            var elementType = ps[^1].ParameterType.GetElementType()!;
            int total = 0;
            for (int i = 0; i < args.Length && total >= 0; i++)
            {
                var score = MatchScore(args[i], i < fixedCount ? ps[i].ParameterType : elementType);
                total = score < 0 ? -1 : total + score;
            }
            if (total > bestScore) { bestScore = total; best = m; }
        }
        if (best is null) return false;

        var parameters = best.GetParameters();
        var fixedParams = parameters.Length - 1;
        var element = parameters[^1].ParameterType.GetElementType()!;
        var built = new object?[parameters.Length];
        for (int i = 0; i < fixedParams; i++)
            built[i] = CoerceBin(args[i], parameters[i].ParameterType);
        var rest = Array.CreateInstance(element, args.Length - fixedParams);
        for (int i = 0; i < rest.Length; i++)
            rest.SetValue(CoerceBin(args[fixedParams + i], element), i);
        built[^1] = rest;
        result = Box(best.Invoke(target, built));
        return true;
    }

    private static bool IsIntegerType(Type t) => Type.GetTypeCode(t) is
        TypeCode.SByte or TypeCode.Byte or TypeCode.Int16 or TypeCode.UInt16 or
        TypeCode.Int32 or TypeCode.UInt32 or TypeCode.Int64 or TypeCode.UInt64;

    private static bool FitsInteger(double v, Type t) => Type.GetTypeCode(t) switch
    {
        TypeCode.SByte  => v >= sbyte.MinValue && v <= sbyte.MaxValue,
        TypeCode.Byte   => v >= 0 && v <= byte.MaxValue,
        TypeCode.Int16  => v >= short.MinValue && v <= short.MaxValue,
        TypeCode.UInt16 => v >= 0 && v <= ushort.MaxValue,
        TypeCode.Int32  => v >= int.MinValue && v <= int.MaxValue,
        TypeCode.UInt32 => v >= 0 && v <= uint.MaxValue,
        TypeCode.Int64  => v >= long.MinValue && v < 9223372036854775808.0,
        TypeCode.UInt64 => v >= 0 && v < 18446744073709551616.0,
        _ => false,
    };

    private static object?[] BuildArgsBin(object?[] binArgs, ParameterInfo[] parameters)
    {
        if (parameters.Length == 0) return [];
        var result = ArrayPool<object?>.Shared.Rent(parameters.Length);
        for (int i = 0; i < parameters.Length && i < binArgs.Length; i++)
            result[i] = CoerceBin(binArgs[i], parameters[i].ParameterType);
        return result;
    }

    private static object?[] BuildArgsBinExact(object?[] binArgs, ParameterInfo[] parameters)
    {
        if (parameters.Length == 0) return [];
        var result = new object?[parameters.Length];
        for (int i = 0; i < parameters.Length && i < binArgs.Length; i++)
            result[i] = CoerceBin(binArgs[i], parameters[i].ParameterType);
        return result;
    }

    // Plain JS objects <-> .NET structs/records: public fields count (Windows.Foundation.Size and most
    // interop structs are fields), member names match case-insensitively.
    internal static readonly System.Text.Json.JsonSerializerOptions s_jsJsonOptions = new()
    {
        IncludeFields = true,
        PropertyNameCaseInsensitive = true,
        NumberHandling = System.Text.Json.Serialization.JsonNumberHandling.AllowReadingFromString
            | System.Text.Json.Serialization.JsonNumberHandling.AllowNamedFloatingPointLiterals,
    };

    private static object? CoerceBin(object? value, Type targetType)
    {
        if (value is null) return null;
        if (value is HandleRef hr)
        {
            s_handles.TryGetValue(hr.Id, out var obj);
            return obj;
        }
        if (value is WinRtRef wr)
        {
            if (wr.Ptr == 0) return null;
            var nativePtr = new IntPtr((long)wr.Ptr);
            // If this native pointer is one that we previously exported (stored
            // in s_nativePtrs), prefer returning the original managed object
            // to preserve identity and avoid creating a new RCW which can
            // fail for CCW pointers. This addresses cases where the runtime
            // round-trips a managed object's canonical IUnknown pointer.
            if (s_nativePtrsReverse.TryGetValue(nativePtr, out var ownerHandle)
                && s_handles.TryGetValue(ownerHandle, out var original))
            {
                return original;
            }
            // 1. Typed QI first: works for COM/CsWinRT interface types that carry a
            //    [Guid] attribute.  More precise than a generic RCW for strongly-typed
            //    parameters such as Windows.UI.Xaml.UIElement.
            if (targetType != typeof(object) && targetType.GUID != Guid.Empty)
            {
                try
                {
                    return Marshal.GetTypedObjectForIUnknown(nativePtr, targetType);
                }
                catch (Exception ex)
                {
                    if (Bridge.IsLogToConsole())
                    {
                        try { Console.Error.WriteLine($"[Bridge] GetTypedObjectForIUnknown failed ptr=0x{nativePtr.ToInt64():x} targetType={targetType.FullName}: {ex}"); } catch { }
                    }
                }
            }

            try
            {
                return Marshal.GetObjectForIUnknown(nativePtr);
            }
            catch (Exception ex)
            {
                if (Bridge.IsLogToConsole())
                {
                    try
                    {
                        Console.Error.WriteLine($"[Bridge] GetObjectForIUnknown failed ptr=0x{nativePtr.ToInt64():x} targetType={targetType.FullName} ({targetType.GUID}): {ex}");
                        try
                        {
                            var found = s_nativePtrs.FirstOrDefault(kvp => kvp.Value == nativePtr);
                            Console.Error.WriteLine($"[Bridge] s_nativePtrs match: key={found.Key} ptr=0x{found.Value.ToInt64():x}");
                        }
                        catch { }
                    }
                    catch { }
                }
                throw;
            }
        }
        if (value is JsJsonValue json)
        {
            if (targetType == typeof(string)) return json.Json;
            var target = targetType == typeof(object) ? typeof(System.Text.Json.JsonElement) : targetType;
            return System.Text.Json.JsonSerializer.Deserialize(json.Json, target, s_jsJsonOptions);
        }
        if (value is JsArrayValue array)
            return CoerceArray(array.Items, targetType);
        if (value is string text && IsParsedFromString(Nullable.GetUnderlyingType(targetType) ?? targetType))
            return ParseString(text, Nullable.GetUnderlyingType(targetType) ?? targetType);
        if (value is JsFunctionRef fn)
        {
            var delegateType = typeof(Delegate).IsAssignableFrom(targetType)
                && targetType != typeof(Delegate) && targetType != typeof(MulticastDelegate)
                    ? targetType
                    : typeof(Action);
            return MakeJsDelegate(delegateType, fn.Id);
        }
        if (value.GetType() == targetType || targetType.IsInstanceOfType(value)) return value;
        var underlying = Nullable.GetUnderlyingType(targetType) ?? targetType;
        if (underlying.IsEnum)
        {
            try { return Enum.ToObject(underlying, Convert.ToInt64(value)); }
            catch { return value; }
        }
        try { return Convert.ChangeType(value, underlying); }
        catch { return value; }
    }

    private static bool IsParsedFromString(Type t) =>
        t == typeof(DateTime) || t == typeof(DateTimeOffset) || t == typeof(TimeSpan) || t == typeof(Guid);

    // A string for a DateTime, DateTimeOffset, TimeSpan or Guid parameter, parsed culture-invariantly
    // (a JS Date is sent as its ISO 8601 string). Left as is when it doesn't parse.
    private static object ParseString(string text, Type t)
    {
        var invariant = System.Globalization.CultureInfo.InvariantCulture;
        var roundtrip = System.Globalization.DateTimeStyles.RoundtripKind;
        if (t == typeof(DateTime) && DateTime.TryParse(text, invariant, roundtrip, out var dt)) return dt;
        if (t == typeof(DateTimeOffset) && DateTimeOffset.TryParse(text, invariant, roundtrip, out var dto)) return dto;
        if (t == typeof(TimeSpan) && TimeSpan.TryParse(text, invariant, out var ts)) return ts;
        if (t == typeof(Guid) && Guid.TryParse(text, out var g)) return g;
        return text;
    }

    // A JS array's items as the array or collection type a parameter expects: T[], a collection
    // interface (IEnumerable<T>, IList<T>, IReadOnlyList<T>) or a concrete collection with Add
    // (List<T>, ObservableCollection<T>). Anything else gets object[].
    private static object? CoerceArray(object?[] items, Type targetType)
    {
        var target = Nullable.GetUnderlyingType(targetType) ?? targetType;
        var element = target.IsArray ? target.GetElementType()! : CollectionElementType(target);
        if (target.IsArray || (target.IsInterface && element is not null))
        {
            var array = Array.CreateInstance(element!, items.Length);
            for (int i = 0; i < items.Length; i++) array.SetValue(CoerceBin(items[i], element!), i);
            return array;
        }
        if (element is not null && !target.IsAbstract && target.GetConstructor(Type.EmptyTypes) is not null)
        {
            var collection = Activator.CreateInstance(target)!;
            var add = target.GetMethod("Add", [element]);
            if (add is not null)
            {
                foreach (var item in items) add.Invoke(collection, [CoerceBin(item, element)]);
                return collection;
            }
        }
        var objects = new object?[items.Length];
        for (int i = 0; i < items.Length; i++) objects[i] = CoerceBin(items[i], typeof(object));
        return objects;
    }

    // T of the IEnumerable<T> a type is or implements, or null.
    internal static Type? CollectionElementType(Type t)
    {
        if (t.IsGenericType && t.GetGenericTypeDefinition() == typeof(IEnumerable<>))
            return t.GetGenericArguments()[0];
        foreach (var i in t.GetInterfaces())
            if (i.IsGenericType && i.GetGenericTypeDefinition() == typeof(IEnumerable<>))
                return i.GetGenericArguments()[0];
        return null;
    }

    // CLR reflection fallback for properties that exist only in managed code and are
    // therefore invisible to the WinRT metadata layer (e.g. App.MainWindow on a class
    // that derives from Microsoft.UI.Xaml.Application but adds CLR-only members).
    // Called by binary opcode 0x0B from the Rust runtime's property interceptor.
    // Throws MissingMemberException when the property doesn't exist so the Rust side
    // can distinguish "not found" (0xFF error → kNo) from "found but null" (0x00 → JS null).
    private static DispatchResult ClrGetProperty(IntPtr instancePtr, string propName)
    {
        if (instancePtr == IntPtr.Zero)
            throw new ArgumentException("Null instance pointer");

        object? target;
        // Prefer the existing managed wrapper (CCW/RCW identity) over creating a new RCW.
        if (s_nativePtrsReverse.TryGetValue(instancePtr, out var ownerHandle)
            && s_handles.TryGetValue(ownerHandle, out target))
        { /* use cached */ }
        else
        {
            try   { target = Marshal.GetObjectForIUnknown(instancePtr); }
            catch (Exception e) { throw new InvalidOperationException($"GetObjectForIUnknown failed: {e.Message}"); }
        }

        if (target is null)
            throw new InvalidOperationException("Instance resolved to null");

        var type  = target.GetType();
        var flags = BindingFlags.Public | BindingFlags.Instance;
        var prop  = GetCachedProp(type, propName, 0, flags);

        if (prop is null)
            throw new MissingMemberException($"CLR property '{propName}' not found on {type.FullName}");

        return Box(prop.GetValue(target));
    }

    private static string? NullIfEmpty(string s) => s.Length == 0 ? null : s;

    private static unsafe void WriteBinError(string msg, byte** outPtr, int* outLen)
    {
        var msgBytes = System.Text.Encoding.UTF8.GetByteCount(msg);
        var buf = new ArrayBufferWriter<byte>(5 + msgBytes);
        var w   = new BinWriter(buf);
        w.WriteByte(0xFF);
        w.WriteString32(msg);
        WriteUnmanaged(buf.WrittenSpan, outPtr, outLen);
    }
}
