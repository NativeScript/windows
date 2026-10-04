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
            WriteUnmanaged(buf.WrittenSpan, responsePtr, responseLenPtr);
        }
        catch (Exception ex)
        {
            WriteBinError(Unwrap(ex).Message, responsePtr, responseLenPtr);
        }
        return 0;
    }

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
            var typeName = r.ReadString16();
            var assembly = r.ReadString16();
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
            var method = r.ReadString16();
            var args   = r.ReadArgs();

            if (method == "__dotnet_await__" && args.Length == 2
                && args[0] is int resolveId && args[1] is int rejectId)
            {
                ScheduleTaskContinuation(handle, resolveId, rejectId);
                return DispatchResult.Void;
            }

            return DispatchCallBin(target, type, method, args, isStatic: false);
        }

        if (op == 0x09) // create JS delegate
        {
            var delTypeName  = r.ReadString16(); // "" → System.Action
            var callbackId   = r.ReadI32();
            return CreateJsDelegate(delTypeName, callbackId);
        }

        if (op == 0x0A) // create JS-backed subclass instance
        {
            var assembly = r.ReadString16();
            var typeName = r.ReadString16();

            var interfaceCount = r.ReadI32();
            var interfaceNames = interfaceCount > 0 ? new string[interfaceCount] : [];
            for (int i = 0; i < interfaceCount; i++) interfaceNames[i] = r.ReadString16();

            var memberCount = r.ReadI32();
            var memberNames = memberCount > 0 ? new string[memberCount] : [];
            for (int i = 0; i < memberCount; i++) memberNames[i] = r.ReadString16();

            var callbackId = r.ReadI32();
            var ctorArgs = r.HasMore ? r.ReadArgs() : [];
            return CreateJsSubclass(NullIfEmpty(assembly), typeName, callbackId, interfaceNames, memberNames, ctorArgs);
        }

        if (op == 0x0B) // get CLR-only property by raw IInspectable ptr (CLR reflection fallback)
        {
            var instancePtr = new IntPtr(r.ReadI64());
            var propName    = r.ReadString16();
            return ClrGetProperty(instancePtr, propName);
        }

        // Static ops: 0x02 = call, 0x03 = constructor
        var typeNameS = r.ReadString16();
        var assemblyS = r.ReadString16();
        var typeS = ResolveType(NullIfEmpty(assemblyS), typeNameS)
            ?? throw new TypeLoadException($"Type not found: {typeNameS} (assembly: {assemblyS})");

        if (op == 0x03) // constructor
        {
            var args  = r.ReadArgs();
            var entry = GetCachedCtor(typeS, args.Length);
            if (entry.Ctor is null)
                throw new MissingMethodException(
                    $"No public ctor on {typeS.FullName} for {args.Length} args");
            // ConstructorInfo.Invoke requires exact arg count — build a precise array.
            var ctorArgs = BuildArgsBinExact(args, entry.Parameters);
            return Box(entry.Ctor.Invoke(ctorArgs));
        }

        // op == 0x02: static call
        var methodS = r.ReadString16();
        var argsS   = r.ReadArgs();
        return DispatchCallBin(null, typeS, methodS, argsS, isStatic: true);
    }

    private static DispatchResult DispatchCallBin(
        object? target, Type type, string method, object?[] args, bool isStatic)
    {
        var flags = (isStatic ? BindingFlags.Static : BindingFlags.Instance) | BindingFlags.Public;

        

        if (method.Length > 4
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

        // Overloads with the same parameter count (Describe(Animal) / Describe(Shape)): pick the
        // first whose parameter types the arguments convert to.
        var overloads = GetOverloads(type, method, args.Length, flags);
        if (overloads.Length > 1)
        {
            foreach (var overload in overloads)
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

        var builtArgs = BuildArgsBin(args, entry.Parameters);
        try { 
            try
            {
                var res = (entry.Invoke(target, builtArgs));
                return Box(res);
            }
            catch (TargetInvocationException tie) when (IsMarshaledForDifferentThread(tie.InnerException))
            {
                if (Bridge.IsLogToConsole()) Console.Error.WriteLine($"[Bridge] Detected wrong-thread COM error; retrying {type.FullName}.{method} on UI thread");
                var res = InvokeOnUIThread(() => (entry.Invoke(target, builtArgs)));
                return Box(res);
            }
            catch (System.Runtime.InteropServices.COMException ce) when (IsMarshaledForDifferentThread(ce))
            {
                if (Bridge.IsLogToConsole()) Console.Error.WriteLine($"[Bridge] Detected COMException wrong-thread; retrying {type.FullName}.{method} on UI thread");
                var res = InvokeOnUIThread(() => (entry.Invoke(target, builtArgs)));
                return Box(res);
            }
        }
        finally { if (builtArgs.Length > 0) ReturnArgs(builtArgs); }
    }

    private static readonly System.Collections.Concurrent.ConcurrentDictionary<MethodKey, MethodInfo[]> s_overloadCache = new();

    private static MethodInfo[] GetOverloads(Type type, string name, int argCount, BindingFlags flags)
        => s_overloadCache.GetOrAdd(new MethodKey(type, name, argCount, flags), static k =>
            k.Type.GetMethods(k.Flags)
                .Where(m => m.Name == k.Name && !m.IsGenericMethodDefinition && m.GetParameters().Length == k.ArgCount)
                .ToArray());

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
        if (value is JsFunctionRef fn)
        {
            var delegateType = typeof(Delegate).IsAssignableFrom(targetType)
                && targetType != typeof(Delegate) && targetType != typeof(MulticastDelegate)
                    ? targetType
                    : typeof(Action);
            return MakeJsDelegate(delegateType, fn.Id);
        }
        if (value.GetType() == targetType) return value;
        var underlying = Nullable.GetUnderlyingType(targetType) ?? targetType;
        if (underlying.IsEnum)
        {
            try { return Enum.ToObject(underlying, Convert.ToInt64(value)); }
            catch { return value; }
        }
        try { return Convert.ChangeType(value, underlying); }
        catch { return value; }
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
