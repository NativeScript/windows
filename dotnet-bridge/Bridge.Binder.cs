using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Linq;
using System.Reflection;
using System.Runtime.CompilerServices;
using System.Threading;

namespace NativeScriptBridge;

// Calls the same-arity overload cache can't make: optional, named and ref/out parameters, generic
// methods and extension methods. Also resolves closed generic type names.
public static partial class Bridge
{
    // A member name as JS sends it, `&Name<T1,T2>#a,b`: `&` returns ref/out values, `<...>` holds
    // type arguments, `#...` names the trailing arguments.
    internal readonly record struct CallName(string Name, bool WantsByRef, string[]? TypeArgs, string[]? Named)
    {
        public static CallName Parse(string raw)
        {
            if (raw.Length == 0 || (raw[0] != '&' && raw.IndexOf('#') < 0 && raw[^1] != '>'))
                return new CallName(raw, false, null, null);
            var byRef = raw[0] == '&';
            var name = byRef ? raw[1..] : raw;
            string[]? named = null;
            var hash = name.LastIndexOf('#');
            if (hash >= 0)
            {
                named = name[(hash + 1)..].Split(',', StringSplitOptions.RemoveEmptyEntries);
                name = name[..hash];
            }
            string[]? typeArgs = null;
            var lt = name.IndexOf('<');
            if (lt > 0 && name[^1] == '>')
            {
                typeArgs = SplitTypeList(name[(lt + 1)..^1]);
                name = name[..lt];
            }
            return new CallName(name, byRef, typeArgs, named);
        }
    }

    // Comma-separated type names, ignoring commas inside brackets.
    internal static string[] SplitTypeList(string list)
    {
        var parts = new List<string>();
        int depth = 0, start = 0;
        for (int i = 0; i < list.Length; i++)
        {
            var c = list[i];
            if (c is '[' or '<') depth++;
            else if (c is ']' or '>') depth--;
            else if (c == ',' && depth == 0)
            {
                parts.Add(list[start..i].Trim());
                start = i + 1;
            }
        }
        if (start < list.Length) parts.Add(list[start..].Trim());
        return parts.ToArray();
    }

    private static Type? ResolveTypeArg(string name)
    {
        name = name.Trim();
        if (name.Length > 1 && name[0] == '[' && name[^1] == ']') name = name[1..^1];
        return ResolveType(null, name);
    }

    // `List`1[System.String]` (or `List[System.String]`): the closed generic type.
    private static Type? ResolveGenericType(string? assemblyName, string typeName)
    {
        var open = typeName.IndexOf('[');
        if (open <= 0 || typeName[^1] != ']') return null;
        var argNames = SplitTypeList(typeName[(open + 1)..^1]);
        if (argNames.Length == 0) return null;
        var args = new Type[argNames.Length];
        for (int i = 0; i < args.Length; i++)
            if ((args[i] = ResolveTypeArg(argNames[i])!) is null) return null;
        var definitionName = typeName[..open];
        if (definitionName.IndexOf('`') < 0) definitionName += "`" + args.Length;
        var definition = ResolveType(assemblyName, definitionName);
        if (definition is null || !definition.IsGenericTypeDefinition) return null;
        try { return definition.MakeGenericType(args); }
        catch (ArgumentException e) { throw new ArgumentException($"{definition.FullName} can't take those type arguments: {e.Message}", e); }
    }

    // Picks the best-fitting candidate and builds its argument array; false when none fits.
    private static bool TryBindCall(IEnumerable<MethodBase> candidates, object? target, object?[] args, CallName call, out MethodBase? method, out object?[]? built, out BindInfo info)
    {
        method = null;
        built = null;
        info = default;
        int bestScore = int.MinValue;
        var namedCount = call.Named?.Length ?? 0;
        var positional = args.Length - namedCount;
        if (positional < 0) return false;

        foreach (var candidate in candidates)
        {
            var m = candidate;
            if (m is MethodInfo { IsGenericMethodDefinition: true } generic)
            {
                m = call.TypeArgs is { } names ? CloseGenericMethod(generic, names) : InferGenericMethod(generic, args, positional);
                if (m is null) continue;
            }
            else if (call.TypeArgs is not null)
            {
                continue;
            }

            var ps = m.GetParameters();
            if (positional > ps.Length) continue;
            var slots = new int[ps.Length];
            Array.Fill(slots, -1);
            for (int i = 0; i < positional; i++) slots[i] = i;
            var ok = true;
            for (int n = 0; n < namedCount && ok; n++)
            {
                var index = Array.FindIndex(ps, p => string.Equals(p.Name, call.Named![n], StringComparison.OrdinalIgnoreCase));
                if (index < 0 || slots[index] >= 0) ok = false;
                else slots[index] = positional + n;
            }
            if (!ok) continue;

            int score = 0;
            var omittedOuts = new List<int>();
            for (int i = 0; i < ps.Length && score >= 0; i++)
            {
                var p = ps[i];
                if (slots[i] < 0)
                {
                    if (p.IsOut) { omittedOuts.Add(i); continue; }
                    if (p.HasDefaultValue || p.IsOptional || IsParamArray(p)) continue;
                    score = -1;
                    break;
                }
                var arg = args[slots[i]];
                var type = p.ParameterType.IsByRef ? p.ParameterType.GetElementType()! : p.ParameterType;
                // JS passes null for an out parameter it only reads back.
                var s = p.IsOut && arg is null ? 5 : MatchScore(arg, type);
                score = s < 0 ? -1 : score + s;
            }
            if (score < 0) continue;
            // Prefer methods that need fewer filled-in parameters.
            score = score * 4 - (ps.Length - args.Length);
            if (score <= bestScore) continue;

            object?[] values;
            try { values = BuildBoundArgs(ps, slots, args); }
            catch { continue; }
            bestScore = score;
            method = m;
            built = values;
            info = new BindInfo(omittedOuts.ToArray(), slots);
        }
        return method is not null;
    }

    internal readonly record struct BindInfo(int[] OmittedOuts, int[] Slots);

    private static bool IsParamArray(ParameterInfo p) =>
        p.ParameterType.IsArray && p.IsDefined(typeof(ParamArrayAttribute), false);

    private static object?[] BuildBoundArgs(ParameterInfo[] ps, int[] slots, object?[] args)
    {
        var values = new object?[ps.Length];
        for (int i = 0; i < ps.Length; i++)
        {
            var p = ps[i];
            var type = p.ParameterType.IsByRef ? p.ParameterType.GetElementType()! : p.ParameterType;
            if (slots[i] >= 0)
            {
                var v = CoerceBin(args[slots[i]], type);
                values[i] = v is null && type.IsValueType && Nullable.GetUnderlyingType(type) is null ? DefaultOf(type) : v;
            }
            else if (IsParamArray(p))
                values[i] = Array.CreateInstance(type.GetElementType()!, 0);
            else if (p.HasDefaultValue && p.DefaultValue is not DBNull && p.DefaultValue is not Missing)
                values[i] = p.DefaultValue is null ? DefaultOf(type) : CoerceDefault(p.DefaultValue, type);
            else
                values[i] = DefaultOf(type);
        }
        return values;
    }

    private static object? DefaultOf(Type t) => t.IsValueType ? RuntimeHelpers.GetUninitializedObject(t) : null;

    // Enum defaults are stored as their underlying number.
    private static object? CoerceDefault(object value, Type type)
    {
        var underlying = Nullable.GetUnderlyingType(type) ?? type;
        if (underlying.IsEnum) return Enum.ToObject(underlying, value);
        return value;
    }

    // `&` calls return [result, ...arguments after the call]; calls that leave out parameters off
    // return [result, ...outs] (outs only for void), like WinRT methods.
    private static DispatchResult InvokeBound(MethodBase method, object? target, object?[] built, BindInfo info, CallName call)
    {
        object? result;
        try
        {
            result = method is ConstructorInfo ctor ? ctor.Invoke(built) : method.Invoke(target, built);
        }
        catch (TargetInvocationException tie) when (tie.InnerException is not null && IsMarshaledForDifferentThread(tie.InnerException))
        {
            result = InvokeOnUIThread(() => method is ConstructorInfo c ? c.Invoke(built) : method.Invoke(target, built));
        }
        var isVoid = method is MethodInfo mi && mi.ReturnType == typeof(void);
        if (call.WantsByRef)
        {
            var byPosition = new object?[info.Slots.Length > 0 ? info.Slots.Max() + 1 : 0];
            for (int i = 0; i < info.Slots.Length; i++)
                if (info.Slots[i] >= 0) byPosition[info.Slots[i]] = built[i];
            return DispatchResult.Collection(new object?[] { result }.Concat(byPosition).ToArray());
        }
        if (info.OmittedOuts.Length > 0)
        {
            var outs = info.OmittedOuts.Select(i => built[i]);
            return DispatchResult.Collection((isVoid ? outs : new[] { result }.Concat(outs)).ToArray());
        }
        return Box(result);
    }

    private static MethodInfo? CloseGenericMethod(MethodInfo definition, string[] typeArgNames)
    {
        if (definition.GetGenericArguments().Length != typeArgNames.Length) return null;
        var types = new Type[typeArgNames.Length];
        for (int i = 0; i < types.Length; i++)
            if ((types[i] = ResolveTypeArg(typeArgNames[i])!) is null) return null;
        try { return definition.MakeGenericMethod(types); }
        catch (ArgumentException) { return null; }
    }

    // Infers type arguments from the arguments as C# does (ToList(list) of a List<string> is
    // ToList<string>). Ones the arguments leave open, such as a JS callback's TResult, are object.
    private static MethodInfo? InferGenericMethod(MethodInfo definition, object?[] args, int positional)
    {
        var genericArgs = definition.GetGenericArguments();
        var bound = new Type?[genericArgs.Length];
        var ps = definition.GetParameters();
        for (int i = 0; i < Math.Min(positional, ps.Length); i++)
        {
            var argType = BridgedType(args[i]);
            if (argType is null) continue;
            var p = ps[i].ParameterType;
            Unify(p.IsByRef ? p.GetElementType()! : p, argType, genericArgs, bound);
        }
        var types = new Type[bound.Length];
        for (int i = 0; i < types.Length; i++) types[i] = bound[i] ?? typeof(object);
        try { return definition.MakeGenericMethod(types); }
        catch (ArgumentException) { return null; }
    }

    // The .NET type a bridged argument stands for, when there is one to infer from.
    private static Type? BridgedType(object? arg) => arg switch
    {
        null => null,
        HandleRef h => s_handles.TryGetValue(h.Id, out var o) ? o?.GetType() : null,
        JsArrayValue a => (CommonType(a.Items) ?? typeof(object)).MakeArrayType(),
        JsTypedArray t => t.ElementType.MakeArrayType(),
        JsFunctionRef or JsJsonValue or WinRtRef => null,
        _ => arg.GetType(),
    };

    // The element type every item of a JS array shares (int, double, string, bool, a .NET type).
    private static Type? CommonType(object?[] items)
    {
        Type? common = null;
        foreach (var item in items)
        {
            var t = BridgedType(item);
            if (t is null) continue;
            if (common is null) common = t;
            else if (common != t)
            {
                if ((common == typeof(int) && t == typeof(double)) || (common == typeof(double) && t == typeof(int))) common = typeof(double);
                else return typeof(object);
            }
        }
        return common;
    }

    private static void Unify(Type parameter, Type argument, Type[] genericArgs, Type?[] bound)
    {
        if (parameter.IsGenericParameter)
        {
            var index = Array.IndexOf(genericArgs, parameter);
            if (index >= 0 && bound[index] is null) bound[index] = argument;
            return;
        }
        if (!parameter.ContainsGenericParameters) return;
        if (parameter.IsArray)
        {
            if (argument.IsArray) Unify(parameter.GetElementType()!, argument.GetElementType()!, genericArgs, bound);
            else if (CollectionElementType(argument) is { } element) Unify(parameter.GetElementType()!, element, genericArgs, bound);
            return;
        }
        if (!parameter.IsGenericType) return;
        var definition = parameter.GetGenericTypeDefinition();
        var match = FindGenericBase(argument, definition);
        if (match is null && argument.IsArray && definition == typeof(IEnumerable<>))
            match = typeof(IEnumerable<>).MakeGenericType(argument.GetElementType()!);
        if (match is null) return;
        var pArgs = parameter.GetGenericArguments();
        var aArgs = match.GetGenericArguments();
        for (int i = 0; i < pArgs.Length; i++) Unify(pArgs[i], aArgs[i], genericArgs, bound);
    }

    // `t` itself, a base class or an interface of it that is an instance of `definition`.
    private static Type? FindGenericBase(Type t, Type definition)
    {
        for (var b = t; b is not null; b = b.BaseType)
            if (b.IsGenericType && b.GetGenericTypeDefinition() == definition) return b;
        if (definition.IsInterface)
            foreach (var i in t.GetInterfaces())
                if (i.IsGenericType && i.GetGenericTypeDefinition() == definition) return i;
        return null;
    }

    // Every method (or constructor) with the call's name: any arity, generic definitions included.
    private static readonly ConcurrentDictionary<(Type, string, BindingFlags), MethodBase[]> s_slowCandidates = new();

    private static MethodBase[] SlowCandidates(Type type, string name, BindingFlags flags) =>
        s_slowCandidates.GetOrAdd((type, name, flags), static k =>
            k.Item2 == ".ctor"
                ? k.Item1.GetConstructors(BindingFlags.Public | BindingFlags.Instance)
                : k.Item1.GetMethods(k.Item3).Where(m => m.Name == k.Item2).ToArray<MethodBase>());

    // Extension methods of every loaded assembly, by name; rebuilt after an assembly loads.
    private static Dictionary<string, MethodInfo[]>? s_extensionIndex;
    private static int s_extensionIndexGeneration = -1;

    private static MethodInfo[] ExtensionMethods(string name)
    {
        var generation = Volatile.Read(ref s_assemblyGeneration);
        var index = s_extensionIndex;
        if (index is null || s_extensionIndexGeneration != generation)
        {
            var built = new Dictionary<string, List<MethodInfo>>(StringComparer.Ordinal);
            foreach (var asm in AppDomain.CurrentDomain.GetAssemblies())
            {
                if (asm.IsDynamic) continue;
                Type[] types;
                try { types = asm.GetExportedTypes(); }
                catch { continue; }
                foreach (var t in types)
                {
                    if (!t.IsSealed || !t.IsAbstract || t.IsGenericType || !t.IsDefined(typeof(ExtensionAttribute), false)) continue;
                    foreach (var m in t.GetMethods(BindingFlags.Public | BindingFlags.Static))
                    {
                        if (!m.IsDefined(typeof(ExtensionAttribute), false)) continue;
                        if (!built.TryGetValue(m.Name, out var list)) built[m.Name] = list = new List<MethodInfo>();
                        list.Add(m);
                    }
                }
            }
            index = built.ToDictionary(kv => kv.Key, kv => kv.Value.ToArray(), StringComparer.Ordinal);
            s_extensionIndex = index;
            s_extensionIndexGeneration = generation;
        }
        return index.TryGetValue(name, out var methods) ? methods : [];
    }

    // `obj.Where(fn)` for an extension method of the object's type: Enumerable.Where(obj, fn).
    private static bool TryExtensionCall(object target, object?[] args, CallName call, out DispatchResult result)
    {
        result = default;
        var candidates = ExtensionMethods(call.Name);
        if (candidates.Length == 0) return false;
        var targetType = target.GetType();
        var applicable = candidates.Where(m =>
        {
            var first = m.GetParameters()[0].ParameterType;
            if (!first.ContainsGenericParameters) return first.IsInstanceOfType(target);
            if (first.IsGenericParameter) return true;
            return first.IsGenericType && FindGenericBase(targetType, first.GetGenericTypeDefinition()) is not null;
        }).ToArray();
        if (applicable.Length == 0) return false;
        var withTarget = new object?[args.Length + 1];
        withTarget[0] = target;
        Array.Copy(args, 0, withTarget, 1, args.Length);
        // The target is the extension method's first argument.
        if (!TryBindCall(applicable, null, withTarget, call, out var method, out var built, out var info)) return false;
        result = InvokeBound(method!, null, built!, info, call with { WantsByRef = false });
        return true;
    }
}
