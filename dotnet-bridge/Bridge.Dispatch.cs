using System;
using System.Buffers;
using System.Collections;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Linq.Expressions;
using System.Reflection;
using System.Runtime.Loader;
using System.Runtime.InteropServices;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;

namespace NativeScriptBridge;

public static partial class Bridge
{
    internal static DispatchResult Dispatch(InvokeRequest req)
    {
        var method = req.Method ?? throw new ArgumentException("Method is required");

        if (method == "__release" && req.Handle.HasValue)
        {
            ReleaseHandle(req.Handle.Value, -1);
            return DispatchResult.Void;
        }

        object? target = null;
        Type type;

        if (req.Handle.HasValue)
        {
            if (!s_handles.TryGetValue(req.Handle.Value, out target))
                throw new KeyNotFoundException($"Invalid handle {req.Handle.Value}");
            type = target?.GetType() ?? throw new InvalidOperationException("Handle refers to null");
        }
        else
        {
            type = ResolveType(req.Assembly, req.TypeName)
                ?? throw new TypeLoadException($"Type not found: {req.TypeName} (assembly: {req.Assembly})");
        }

        if (method == "__members__")
            return BuildMembersResult(type);

        if (method == "__dotnet_await__" && req.Handle.HasValue)
        {
            var a = req.Args ?? [];
            if (a.Length >= 2)
            {
                var resolveId = (int)a[0].GetInt64();
                var rejectId  = (int)a[1].GetInt64();
                ScheduleTaskContinuation(req.Handle.Value, resolveId, rejectId);
            }
            return DispatchResult.Void;
        }

        if (method == ".ctor")
        {
            var argElems = req.Args ?? [];
            var entry    = GetCachedCtor(type, argElems.Length);
            if (entry.Ctor is null)
                throw new MissingMethodException(
                    $"No public constructor on {type.FullName} for {argElems.Length} args");
            // ConstructorInfo.Invoke validates args.Length == paramCount exactly — no pool.
            var ctorArgs = BuildArgsExact(argElems, entry.Parameters);
            return Box(entry.Ctor.Invoke(ctorArgs));
        }

        var isStatic = target is null;
        var flags    = (isStatic ? BindingFlags.Static : BindingFlags.Instance) | BindingFlags.Public;

        if (method.Length > 4)
        {
            if (method[0] == 'g' && method[1] == 'e' && method[2] == 't' && method[3] == '_')
            {
                var prop = GetCachedProp(type, method, 4, flags);
                if (prop is not null) return Box(prop.GetValue(target));
            }
            else if (method[0] == 's' && method[1] == 'e' && method[2] == 't' && method[3] == '_'
                     && req.Args?.Length == 1)
            {
                var prop = GetCachedProp(type, method, 4, flags);
                if (prop is not null)
                {
                    prop.SetValue(target, Coerce(req.Args[0], prop.PropertyType));
                    return DispatchResult.Void;
                }
            }
        }

        var args      = req.Args ?? [];
        var dispEntry = GetCachedMethod(type, method, args.Length, flags);
        if (dispEntry.Invoke is null)
        {
            // Fallback: try methods with the same name (different signatures).
            var candidates = type.GetMethods(flags).Where(m => m.Name == method && !m.IsSpecialName);
            foreach (var m in candidates)
            {
                var parameters = m.GetParameters();
                // MethodInfo.Invoke takes exactly as many arguments as the method has parameters.
                if (parameters.Length != args.Length) continue;
                object?[] built;
                try { built = BuildArgsExact(args, parameters); }
                catch { continue; }
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
                    catch { /* retry failed: try the next candidate */ }
                }
                catch (System.Runtime.InteropServices.COMException ce) when (IsMarshaledForDifferentThread(ce))
                {
                    if (Bridge.IsLogToConsole()) Console.Error.WriteLine($"[Bridge] Detected COMException wrong-thread; retrying {type.FullName}.{m.Name} on UI thread");
                    try
                    {
                        var res = InvokeOnUIThread(() => (m.Invoke(target, built)));
                        return Box(res);
                    }
                    catch { /* retry failed: try the next candidate */ }
                }
            }
            throw new MissingMethodException(
                $"Method '{method}' ({args.Length} args) not found on {type.FullName}");
        }

        var builtArgs = BuildArgs(args, dispEntry.Parameters);
        try {
            try
            {
                var res = (dispEntry.Invoke(target, builtArgs));
                return Box(res);
            }
            catch (TargetInvocationException tie) when (IsMarshaledForDifferentThread(tie.InnerException))
            {
                if (Bridge.IsLogToConsole()) Console.Error.WriteLine($"[Bridge] Detected wrong-thread COM error; retrying {type.FullName}.{method} on UI thread");
                var res = InvokeOnUIThread(() => (dispEntry.Invoke(target, builtArgs)));
                return Box(res);
            }
            catch (System.Runtime.InteropServices.COMException ce) when (IsMarshaledForDifferentThread(ce))
            {
                if (Bridge.IsLogToConsole()) Console.Error.WriteLine($"[Bridge] Detected COMException wrong-thread; retrying {type.FullName}.{method} on UI thread");
                var res = InvokeOnUIThread(() => (dispEntry.Invoke(target, builtArgs)));
                return Box(res);
            }
        }
        finally { if (builtArgs.Length > 0) ReturnArgs(builtArgs); }
    }

    private static DispatchEntry GetCachedMethod(Type type, string name, int argCount, BindingFlags flags)
        => s_methodCache.GetOrAdd(
            new MethodKey(type, name, argCount, flags),
            static k => BuildDispatchEntry(k.Type, k.Name, k.ArgCount, k.Flags));

    private static DispatchEntry BuildDispatchEntry(Type type, string name, int argCount, BindingFlags flags)
    {
        var mi = FindMethodCore(type, name, argCount, flags);
        return mi is null ? DispatchEntry.Empty : BuildDispatchEntry(type, mi);
    }

    internal static DispatchEntry BuildDispatchEntry(Type type, MethodInfo mi)
    {
        var parameters = mi.GetParameters();
        if (parameters.Any(p => p.ParameterType.IsByRef || p.ParameterType.IsByRefLike) || mi.ReturnType.IsByRefLike)
            return new DispatchEntry(CompileInvoker(type, mi, parameters), parameters);
        return new DispatchEntry(new TieredInvoker(type, mi, parameters).Invoke, parameters);
    }

    // MethodInvoker (cheap to create) until the method is called often enough to pay for a compiled
    // delegate (~80 us to build, a few ns per call).
    private sealed class TieredInvoker(Type type, MethodInfo mi, ParameterInfo[] parameters)
    {
        private const int CompileAfter = 32;
        private readonly MethodInvoker _invoker = MethodInvoker.Create(mi);
        private Func<object?, object?[], object?>? _compiled;
        private int _calls;

        public object? Invoke(object? target, object?[] args)
        {
            var compiled = _compiled;
            if (compiled is not null) return compiled(target, args);
            if (Interlocked.Increment(ref _calls) == CompileAfter)
                _compiled = CompileInvoker(type, mi, parameters);
            try
            {
                return _invoker.Invoke(target, new Span<object?>(args, 0, parameters.Length));
            }
            catch (TargetInvocationException tie) when (tie.InnerException is not null)
            {
                // Throw what the method threw, as the compiled delegate does.
                System.Runtime.ExceptionServices.ExceptionDispatchInfo.Capture(tie.InnerException).Throw();
                throw;
            }
        }
    }

    private static Func<object?, object?[], object?> CompileInvoker(Type type, MethodInfo mi, ParameterInfo[] parameters)
    {
        try
        {
            var targetParam = Expression.Parameter(typeof(object), "t");
            var argsParam   = Expression.Parameter(typeof(object?[]), "a");

            var typedArgs = parameters.Select((p, i) =>
                (Expression)Expression.Convert(
                    Expression.ArrayIndex(argsParam, Expression.Constant(i)),
                    p.ParameterType)).ToArray();

            Expression body = mi.IsStatic
                ? Expression.Call(mi, typedArgs)
                : Expression.Call(Expression.Convert(targetParam, type), mi, typedArgs);

            if (body.Type == typeof(void))
                body = Expression.Block(typeof(object), body, Expression.Constant(null, typeof(object)));
            else if (body.Type != typeof(object))
                body = Expression.Convert(body, typeof(object));

            return Expression.Lambda<Func<object?, object?[], object?>>(
                body, targetParam, argsParam).Compile();
        }
        catch
        {
            // mi.Invoke validates args.Length == paramCount; slice the rented buffer if needed.
            int pc = parameters.Length;
            return (t, a) => mi.Invoke(t, a.Length == pc ? a : a[..pc]);
        }
    }

    private static CtorEntry GetCachedCtor(Type type, int argCount)
        => s_ctorCache.GetOrAdd(new CtorKey(type, argCount), static k =>
        {
            var ctors = k.Type.GetConstructors(BindingFlags.Public | BindingFlags.Instance);
            var ctor  = Array.Find(ctors, c => c.GetParameters().Length == k.ArgCount)
                     ?? ctors.FirstOrDefault();
            return new CtorEntry(ctor, ctor?.GetParameters() ?? []);
        });

    // Keyed by the accessor name as called ("get_Length") plus the prefix length, so a warm lookup
    // allocates no property-name substring.
    private static DataMember? GetCachedProp(Type type, string method, int prefixLen, BindingFlags flags)
        => s_propCache.GetOrAdd(
            new PropKey(type, method, prefixLen, flags),
            static k => DataMember.Find(k.Type, k.Name[k.PrefixLen..], k.Flags));

    // Pooled: rented array passed to the compiled delegate (which accesses by index,
    // not by Length). Caller must return via ReturnArgs immediately after invoke.
    private static object?[] BuildArgs(JsonElement[] elements, ParameterInfo[] parameters)
    {
        if (parameters.Length == 0) return [];
        var args = ArrayPool<object?>.Shared.Rent(parameters.Length);
        for (int i = 0; i < parameters.Length && i < elements.Length; i++)
            args[i] = Coerce(elements[i], parameters[i].ParameterType);
        return args;
    }

    // Exact: required by ConstructorInfo.Invoke / MethodInfo.Invoke which validate
    // args.Length == paramCount and would throw on an oversized rented buffer.
    private static object?[] BuildArgsExact(JsonElement[] elements, ParameterInfo[] parameters)
    {
        if (parameters.Length == 0) return [];
        var args = new object?[parameters.Length];
        for (int i = 0; i < parameters.Length && i < elements.Length; i++)
            args[i] = Coerce(elements[i], parameters[i].ParameterType);
        return args;
    }

    private static void ReturnArgs(object?[] args) =>
        ArrayPool<object?>.Shared.Return(args, clearArray: true);

    private static object? Coerce(JsonElement el, Type targetType)
    {
        if (el.ValueKind == JsonValueKind.Null) return null;
        if (el.ValueKind == JsonValueKind.Object && el.TryGetProperty("__handle", out var h))
        {
            s_handles.TryGetValue(h.GetInt32(), out var obj);
            return obj;
        }
        return el.Deserialize(targetType, s_coerceOpts);
    }

    private static DispatchResult Box(object? value)
    {
        if (value is null) return DispatchResult.Void;
        // Treat pointer-sized IntPtr/UIntPtr as primitive numeric returns so
        // the runtime receives a numeric pointer value instead of a handle.
        if (value is IntPtr ip)
            return DispatchResult.Primitive(ip.ToInt64(), typeof(long));
        if (value is UIntPtr up)
            return DispatchResult.Primitive(unchecked((long)up.ToUInt64()), typeof(long));

        if (TryGetBytes(value, out _, out _)) return DispatchResult.Bytes(value);

        var t = value.GetType();
        var info = GetBoxInfo(t);

        if (info.Primitive)
            return DispatchResult.Primitive(value, t);

        // Arrays are marshalled as Collections (0x07) so the runtime receives the items
        // directly. Other collections (List<T>, Dictionary, ObservableCollection) stay .NET
        // objects, so their methods work and native code sees the same instance.
        if (info.Collection)
            return DispatchResult.Collection((System.Collections.IEnumerable)value);

        if (info.EnumUnderlying is { } ut)
            return DispatchResult.Primitive(Convert.ChangeType(value, ut), ut);

        // An instance of a JS subclass goes back to JS as the handle its JS object holds, so the
        // runtime hands out that same JS object. If that object was collected (its handle released),
        // the instance gets a new handle below, recorded for the JS object the runtime revives.
        var jsHolder = JsBackedHolder(value, out var jsHandleLive);
        if (jsHolder is not null && jsHandleLive)
            return DispatchResult.Handle(jsHolder.HandleId, t.FullName ?? t.Name);



        var id = HandleFor(value);
        if (jsHolder is not null) jsHolder.HandleId = id;

            // Try to obtain a canonical IInspectable/IUnknown pointer for COM/WinRT objects
            // so the runtime can call native vtable methods directly.
            // ObtainNativePtr prefers IInspectable for WinRT-compatible types and uses
            // the C#/WinRT inner object for managed WinRT subclasses (e.g. FlexboxLayout
            // subclasses), ensuring QI succeeds for all inherited WinRT interfaces.
            // The pointer is addref'd here and released on __release.
            // C#/WinRT objects and JS subclass instances get it here, since the runtime wraps them
            // as WinRT objects right away. Other objects get one when first passed to a WinRT API
            // (GetNativePtrForHandle): making a COM wrapper costs more than the rest of the call.
            try
            {
                if ((info.WinRTNativeObject is not null || jsHolder is not null) && !s_nativePtrs.ContainsKey(id))
                {
                    var p = ObtainNativePtr(value);
                    if (p != IntPtr.Zero)
                        StoreNativePtr(id, p);
                }
            }
            catch
            {
                // Not a COM object or failed to obtain native pointer; ignore.
            }
        return info.Awaitable
            ? DispatchResult.TaskHandle(id, info.TypeName)
            : DispatchResult.Handle(id, info.TypeName);
    }

    // What Box needs to know about a returned value's type, worked out once per type rather than
    // by reflection on every call.
    internal sealed class BoxInfo
    {
        public string TypeName = "";
        public bool Primitive;
        public bool Collection;
        public Type? EnumUnderlying;
        public bool Awaitable;
        // WinRT.IWinRTObject.NativeObject when the type is a C#/WinRT projection (see ObtainNativePtr).
        public PropertyInfo? WinRTNativeObject;
    }

    private static readonly System.Collections.Concurrent.ConcurrentDictionary<Type, BoxInfo> s_boxInfo = new();

    internal static BoxInfo GetBoxInfo(Type t) => s_boxInfo.GetOrAdd(t, static t =>
    {
        var info = new BoxInfo { TypeName = t.FullName ?? t.Name };
        if (t.IsPrimitive || t == typeof(string) || t == typeof(decimal) || t == typeof(Guid))
        {
            info.Primitive = true;
            return info;
        }
        if (t.IsArray)
        {
            info.Collection = true;
            return info;
        }
        if (t.IsEnum)
        {
            info.EnumUnderlying = Enum.GetUnderlyingType(t);
            return info;
        }
        info.Awaitable = IsAwaitable(t);
        try { info.WinRTNativeObject = t.GetInterface("WinRT.IWinRTObject")?.GetProperty("NativeObject"); } catch { }
        return info;
    });

    private static bool IsAwaitable(Type t)
    {
        if (typeof(Task).IsAssignableFrom(t) || t == typeof(ValueTask)) return true;
        if (t.IsGenericType && t.GetGenericTypeDefinition() == typeof(ValueTask<>)) return true;
        if ((t.FullName ?? "").StartsWith("Windows.Foundation.IAsync", StringComparison.Ordinal)) return true;
        foreach (var iface in t.GetInterfaces())
            if ((iface.FullName ?? "").StartsWith("Windows.Foundation.IAsync", StringComparison.Ordinal)) return true;
        return t.GetMethod("GetAwaiter",
                   BindingFlags.Public | BindingFlags.Instance,
                   null, Type.EmptyTypes, null) is not null;
    }

    /// "interface", "abstract", "static", "sealed", "class", "struct", "enum", "delegate", or ""
    /// when the type isn't found.
    /// Lets JS decide between `new Interface({...})`, subclassing and plain construction.
    public static string TypeKind(string typeName)
    {
        var t = ResolveType(null, typeName);
        if (t is null) return "";
        if (t.IsInterface) return "interface";
        if (t.IsEnum) return "enum";
        if (typeof(Delegate).IsAssignableFrom(t)) return "delegate";
        if (t.IsValueType) return "struct";
        if (t.IsAbstract && t.IsSealed) return "static";
        if (t.IsAbstract) return "abstract";
        if (t.IsSealed) return "sealed";
        return "class";
    }

    // What a .NET collection supports, for the JS side to make it array-like: 1 = enumerable
    // (for...of, map, ...), 2 = has a count (length), 4 = indexed by position (list[0]),
    // 8 = keyed by string (dict.key / dict['key']).
    public static int CollectionKind(object? value)
    {
        if (value is null or string || value is not System.Collections.IEnumerable) return 0;
        var info = GetCollectionInfo(value.GetType());
        return 1 | (info.Count is not null ? 2 : 0) | (info.Indexer is not null ? 4 : 0) | (info.KeyIndexer is not null ? 8 : 0);
    }

    // [found, value] for `key` of a string-keyed dictionary.
    public static object?[] DictionaryTryGet(object? value, string key)
    {
        var info = value is null ? null : GetCollectionInfo(value.GetType());
        if (info?.KeyIndexer is null || info.ContainsKey is null) return [false, null];
        if (info.ContainsKey.Invoke(value, [key]) is not true) return [false, null];
        return [true, info.KeyIndexer.GetValue(value, [key])];
    }

    public static void DictionarySet(object? value, string key, object? item)
    {
        var info = value is null ? null : GetCollectionInfo(value.GetType());
        if (info?.KeyIndexer is not { CanWrite: true } indexer)
            throw new NotSupportedException($"{value?.GetType().FullName} has no settable string keys");
        indexer.SetValue(value, CoerceBin(item, indexer.PropertyType), [key]);
    }

    // A collection's items, as an array the runtime copies into a JS array.
    public static object?[] CollectionItems(object? value)
    {
        if (value is not System.Collections.IEnumerable items) return [];
        var list = new List<object?>();
        foreach (var item in items) list.Add(item);
        return list.ToArray();
    }

    public static int CollectionCount(object? value)
    {
        if (value is System.Collections.ICollection c) return c.Count;
        var count = value is null ? null : GetCollectionInfo(value.GetType()).Count;
        return count?.GetValue(value) is int n ? n : CollectionItems(value).Length;
    }

    // The item at `index`, or null past either end (as a JS array gives undefined).
    public static object? ItemAt(object? value, int index)
    {
        if (index < 0 || index >= CollectionCount(value)) return null;
        if (value is System.Collections.IList list) return list[index];
        var indexer = value is null ? null : GetCollectionInfo(value.GetType()).Indexer;
        if (indexer is null) throw new NotSupportedException($"{value?.GetType().FullName} is not indexed by position");
        return indexer.GetValue(value, [index]);
    }

    public static void SetItemAt(object? value, int index, object? item)
    {
        var info = value is null ? null : GetCollectionInfo(value.GetType());
        if (info?.Indexer is not { CanWrite: true } indexer)
            throw new NotSupportedException($"{value?.GetType().FullName} has no settable items");
        indexer.SetValue(value, CoerceBin(item, indexer.PropertyType), [index]);
    }

    // The count property and positional indexer of a collection type, from IList<T> /
    // IReadOnlyList<T> / ICollection<T> / IReadOnlyCollection<T> (or their non-generic forms).
    internal sealed class CollectionInfo
    {
        public PropertyInfo? Count;
        public PropertyInfo? Indexer;
        // The `this[string]` of an IDictionary<string, T> / IReadOnlyDictionary<string, T>.
        public PropertyInfo? KeyIndexer;
        public MethodInfo? ContainsKey;
    }

    private static readonly System.Collections.Concurrent.ConcurrentDictionary<Type, CollectionInfo> s_collectionInfo = new();

    private static CollectionInfo GetCollectionInfo(Type t) => s_collectionInfo.GetOrAdd(t, static t =>
    {
        var info = new CollectionInfo();
        Type[] interfaces = t.IsInterface ? [t, .. t.GetInterfaces()] : t.GetInterfaces();
        foreach (var i in interfaces)
        {
            var definition = i.IsGenericType ? i.GetGenericTypeDefinition() : i;
            if (info.Indexer is null && (definition == typeof(IList<>) || definition == typeof(IReadOnlyList<>) || definition == typeof(System.Collections.IList)))
                info.Indexer = i.GetProperty("Item");
            if (info.Count is null && (definition == typeof(ICollection<>) || definition == typeof(IReadOnlyCollection<>) || definition == typeof(System.Collections.ICollection)))
                info.Count = i.GetProperty("Count");
            if ((info.KeyIndexer is null || !info.KeyIndexer.CanWrite)
                && (definition == typeof(IDictionary<,>) || definition == typeof(IReadOnlyDictionary<,>))
                && i.GetGenericArguments()[0] == typeof(string))
            {
                info.KeyIndexer = i.GetProperty("Item");
                info.ContainsKey = i.GetMethod("ContainsKey");
            }
        }
        // A writable IList<T> indexer beats a read-only IReadOnlyList<T> one.
        if (info.Indexer is { CanWrite: false })
            foreach (var i in interfaces)
                if (i.IsGenericType && i.GetGenericTypeDefinition() == typeof(IList<>))
                    info.Indexer = i.GetProperty("Item");
        return info;
    });

    /// `value instanceof SomeType` for a .NET object that was not constructed from JS.
    public static bool IsInstanceOf(object? value, string typeName)
    {
        if (value is null) return false;
        var t = ResolveType(null, typeName);
        return t is not null && t.IsInstanceOfType(value);
    }

    internal static Type? ResolveType(string? assemblyName, string? typeName)
    {
        if (string.IsNullOrEmpty(typeName)) return null;
        var key = (assemblyName ?? "", typeName);
        if (s_typeCache.TryGetValue(key, out var cached)) return cached;
        var generation = Volatile.Read(ref s_assemblyGeneration);
        if (s_missingTypes.TryGetValue(key, out var missedAt) && missedAt == generation) return null;
        var t = ResolveTypeCore(key.Item1.Length == 0 ? null : key.Item1, typeName)
            ?? ResolveNestedType(key.Item1.Length == 0 ? null : key.Item1, typeName)
            ?? ResolveGenericType(key.Item1.Length == 0 ? null : key.Item1, typeName);
        if (t is not null) s_typeCache[key] = t;
        else s_missingTypes[key] = generation;
        return t;
    }

    // Type lookups that failed, with the assembly-load generation they failed in.
    private static readonly System.Collections.Concurrent.ConcurrentDictionary<(string, string), int> s_missingTypes = new();
    private static int s_assemblyGeneration;

    private static void OnAssemblyLoad(object? sender, AssemblyLoadEventArgs args) =>
        Interlocked.Increment(ref s_assemblyGeneration);

    // `Outer.Inner` as JS spells a nested type: CLR names it `Outer+Inner`.
    private static Type? ResolveNestedType(string? assemblyName, string typeName)
    {
        if (typeName.IndexOf('[') >= 0) return null;
        for (var dot = typeName.LastIndexOf('.'); dot > 0; dot = typeName.LastIndexOf('.', dot - 1))
        {
            var outer = ResolveType(assemblyName, typeName[..dot]);
            if (outer is null) continue;
            Type? t = outer;
            foreach (var part in typeName[(dot + 1)..].Split('.'))
            {
                t = t.GetNestedType(part, BindingFlags.Public | BindingFlags.NonPublic);
                if (t is null) break;
            }
            return t;
        }
        return null;
    }

    private static Type? ResolveTypeInAssembly(Assembly asm, string typeName, int lastDot, string shortName)
    {
        var t = asm.GetType(typeName);
        if (t is not null) return t;

        foreach (var ty in asm.GetTypes())
        {
            if (string.Equals(ty.FullName, typeName, StringComparison.Ordinal))
                return ty;
            if (lastDot < 0 && string.Equals(ty.Name, shortName, StringComparison.Ordinal))
                return ty;
        }

        return null;
    }

    private static Type? ResolveTypeFromKnownAssemblies(string typeName, int lastDot, string shortName, string? skipAssemblyName = null)
    {
        foreach (var dir in s_assemblySearchDirs.Where(Directory.Exists))
        {
            IEnumerable<string> candidates;
            try
            {
                candidates = Directory.EnumerateFiles(dir, "*.dll", SearchOption.TopDirectoryOnly);
            }
            catch
            {
                continue;
            }

            foreach (var file in candidates)
            {
                try
                {
                    var simple = Path.GetFileNameWithoutExtension(file);
                    if (string.IsNullOrEmpty(simple) || string.Equals(simple, skipAssemblyName, StringComparison.OrdinalIgnoreCase))
                        continue;

                    var asm = AppDomain.CurrentDomain.GetAssemblies()
                        .FirstOrDefault(a => string.Equals(a.GetName().Name, simple, StringComparison.OrdinalIgnoreCase));
                    if (asm is null)
                    {
                        try { asm = AssemblyLoadContext.Default.LoadFromAssemblyPath(file); }
                        catch { continue; }
                    }

                    var t = ResolveTypeInAssembly(asm, typeName, lastDot, shortName);
                    if (t is not null) return t;
                }
                catch (ReflectionTypeLoadException) { }
                catch { }
            }
        }

        return null;
    }

    private static Type? ResolveTypeCore(string? assemblyName, string? typeName)
    {
        if (string.IsNullOrEmpty(typeName)) return null;

        // Fast/normal lookup first (assembly-qualified or plain type name).
        var fqn = string.IsNullOrEmpty(assemblyName) ? typeName! : $"{typeName}, {assemblyName}";
        var t = Type.GetType(fqn);
        if (t is not null) return t;

        t = Type.GetType(typeName);
        if (t is not null) return t;

        // Last segment (type name without namespace) used for loose matching below.
        var lastDot = typeName.LastIndexOf('.');
        var shortName = lastDot >= 0 ? typeName[(lastDot + 1)..] : typeName;

        // Search loaded assemblies quickly; prefer asm.GetType which is inexpensive.
        foreach (var asm in AppDomain.CurrentDomain.GetAssemblies())
        {
            try
            {
                t = ResolveTypeInAssembly(asm, typeName, lastDot, shortName);
                if (t is not null) return t;
            }
            catch (ReflectionTypeLoadException) { }
            catch { }
        }

        // If an assembly name was provided try loading it explicitly and repeat
        // the above search inside that assembly only (less work than scanning all).
        if (!string.IsNullOrEmpty(assemblyName))
        {
            try
            {
                var asm = Assembly.Load(assemblyName);
                if (asm is not null)
                {
                    t = ResolveTypeInAssembly(asm, typeName!, lastDot, shortName);
                    if (t is not null) return t;
                }
            }
            catch { }
        }

        return ResolveTypeFromKnownAssemblies(typeName!, lastDot, shortName, assemblyName);
    }

    private static MethodInfo? FindMethodCore(Type type, string name, int argCount, BindingFlags flags)
    {
        // Exact match first: same name and parameter count.
        var match = Array.Find(type.GetMethods(flags),
            m => m.Name == name && !m.IsGenericMethod && m.GetParameters().Length == argCount);
        if (match is not null) return match;

        // Fallback to GetMethod which may handle binding differently.
        var gm = type.GetMethod(name, flags);
        if (gm is not null && !gm.IsGenericMethod && gm.GetParameters().Length == argCount) return gm;

        // WinRT method names sometimes project to different CLR names
        // (e.g. WinRT's `Append` → CLR `Add`). Try a small alias map
        // before giving up so common collection methods work through
        // the dotnet bridge without extra bridge-side shims.
        var aliases = new Dictionary<string, string[]>()
        {
            ["Append"] = new[] { "Add" },
            ["InsertAt"] = new[] { "Insert" },
            ["GetAt"] = new[] { "get_Item" , "ElementAt" },
            ["SetAt"] = new[] { "set_Item" },
            ["RemoveAt"] = new[] { "RemoveAt" },
            ["ReplaceAll"] = new[] { "Clear" },
        };

        if (aliases.TryGetValue(name, out var candidates))
        {
            foreach (var cand in candidates)
            {
                var candMatch = Array.Find(type.GetMethods(flags),
                    m => m.Name == cand && !m.IsGenericMethod && m.GetParameters().Length == argCount);
                if (candMatch is not null) return candMatch;

                var gm2 = type.GetMethod(cand, flags);
                if (gm2 is not null && !gm2.IsGenericMethod && gm2.GetParameters().Length == argCount) return gm2;
            }
        }

        return null;
    }

    internal static Exception Unwrap(Exception ex) =>
        ex is TargetInvocationException { InnerException: { } inner } ? Unwrap(inner) : ex;

    // Used by benchmarks to give a fair end-to-end comparison of protocols.

    internal static byte[] PipelineJson(ReadOnlySpan<byte> jsonRequest)
    {
        var req    = JsonSerializer.Deserialize(jsonRequest, BridgeJsonContext.Default.InvokeRequest)!;
        var result = Dispatch(req);
        var buf    = new ArrayBufferWriter<byte>(128);
        using var w = new Utf8JsonWriter(buf);
        w.WriteStartObject();
        w.WritePropertyName("result"u8);
        result.WriteTo(w, s_coerceOpts);
        w.WriteEndObject();
        w.Flush();
        return buf.WrittenSpan.ToArray();
    }

    internal static byte[] PipelineBinary(ReadOnlySpan<byte> binRequest)
    {
        var r      = new BinReader(binRequest);
        var result = DispatchBin(ref r);
        var buf    = new ArrayBufferWriter<byte>(128);
        result.WriteAsBin(buf);
        return buf.WrittenSpan.ToArray();
    }

    private static unsafe void WriteResult(DispatchResult res, byte** outPtr, int* outLen)
    {
        var buf = new ArrayBufferWriter<byte>(256);
        using var w = new Utf8JsonWriter(buf);
        w.WriteStartObject();
        w.WritePropertyName("result"u8);
        res.WriteTo(w, s_coerceOpts);
        w.WriteEndObject();
        w.Flush();
        WriteUnmanaged(buf.WrittenSpan, outPtr, outLen);
    }

    private static unsafe void WriteError(string msg, byte** outPtr, int* outLen)
    {
        var buf = new ArrayBufferWriter<byte>(128);
        using var w = new Utf8JsonWriter(buf);
        w.WriteStartObject();
        w.WriteString("error"u8, msg);
        w.WriteEndObject();
        w.Flush();
        WriteUnmanaged(buf.WrittenSpan, outPtr, outLen);
    }

    internal static unsafe void WriteUnmanaged(ReadOnlySpan<byte> bytes, byte** outPtr, int* outLen)
    {
        var p = (byte*)Marshal.AllocHGlobal(bytes.Length + 1);
        bytes.CopyTo(new Span<byte>(p, bytes.Length));
        p[bytes.Length] = 0;
        *outPtr  = p;
        *outLen  = bytes.Length;
    }
}
