using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Linq;
using System.Linq.Expressions;
using System.Reflection;
using System.Threading.Tasks;

namespace NativeScriptBridge;

public static partial class Bridge
{
    private static readonly ConcurrentDictionary<Type, Func<object, Task>?> s_awaitableCache = new();

    internal static void ScheduleTaskContinuation(int handleId, int resolveId, int rejectId)
    {
        if (!s_handles.TryGetValue(handleId, out var obj) || obj is null)
            throw new KeyNotFoundException($"Invalid handle {handleId}");

        var task = GetTaskForAwaitable(obj)
            ?? throw new InvalidOperationException(
                $"Object of type {obj.GetType().FullName} is not awaitable. " +
                $"Expected Task<T>, ValueTask<T>, IAsyncOperation<T>, or any type implementing GetAwaiter().");

        // The handle stays valid after completion (the JS proxy releases it when collected), so
        // the same Task can be awaited again.
        task.ContinueWith(completed =>
        {
            try
            {
                if (completed.IsFaulted)
                {
                    var ex = completed.Exception?.InnerException ?? completed.Exception;
                    CallJsCallback(rejectId, [ex?.Message ?? "Task faulted"]);
                }
                else if (completed.IsCanceled)
                {
                    CallJsCallback(rejectId, ["Task cancelled"]);
                }
                else
                {
                    CallJsCallback(resolveId, [TaskResultCache.GetResult(completed)]);
                }
            }
            catch (Exception ex)
            {
                CallJsCallback(rejectId, [ex.Message]);
            }
        }, TaskScheduler.Default);
    }

    private static Task? GetTaskForAwaitable(object obj)
    {
        if (obj is Task t) return t;
        if (obj is ValueTask vt) return vt.AsTask();

        var type = obj.GetType();
        if (type.IsGenericType && type.GetGenericTypeDefinition() == typeof(ValueTask<>))
            return ValueTaskAsTaskCache.AsTask(type, obj);

        var winrtTask = WinRtAsyncCache.TryGetTask(type, obj);
        if (winrtTask != null) return winrtTask;

        return s_awaitableCache.GetOrAdd(type, BuildAwaitableFactory)?.Invoke(obj);
    }

    private static Func<object, Task>? BuildAwaitableFactory(Type type)
    {
        var getAwaiter = type.GetMethod("GetAwaiter",
            BindingFlags.Instance | BindingFlags.Public, null, Type.EmptyTypes, null);
        if (getAwaiter is null) return null;

        var awaiterType = getAwaiter.ReturnType;
        var isCompleted = awaiterType.GetProperty("IsCompleted");
        var getResult   = awaiterType.GetMethod("GetResult",
            BindingFlags.Instance | BindingFlags.Public, null, Type.EmptyTypes, null);
        var onCompleted = awaiterType.GetMethod("UnsafeOnCompleted",
                              BindingFlags.Instance | BindingFlags.Public)
                       ?? awaiterType.GetMethod("OnCompleted",
                              BindingFlags.Instance | BindingFlags.Public);

        if (isCompleted is null || onCompleted is null) return null;

        return obj =>
        {
            var tcs = new TaskCompletionSource<object?>(
                TaskCreationOptions.RunContinuationsAsynchronously);
            try
            {
                var awaiter = getAwaiter.Invoke(obj, null);
                if (awaiter is null) { tcs.SetResult(null); return tcs.Task; }

                void Complete()
                {
                    try
                    {
                        tcs.SetResult(getResult?.Invoke(awaiter, null));
                    }
                    catch (TargetInvocationException ex)
                    {
                        tcs.SetException(ex.InnerException ?? ex);
                    }
                    catch (Exception ex) { tcs.SetException(ex); }
                }

                if ((bool)(isCompleted.GetValue(awaiter) ?? false))
                    Complete();
                else
                    onCompleted.Invoke(awaiter, [(Action)Complete]);
            }
            catch (Exception ex) { tcs.SetException(ex); }

            return tcs.Task;
        };
    }
}

internal static class WinRtAsyncCache
{
    private static readonly ConcurrentDictionary<Type, Func<object, Task>?> s_cache = new();

    public static Task? TryGetTask(Type type, object obj)
        => s_cache.GetOrAdd(type, BuildFactory)?.Invoke(obj);

    private static Func<object, Task>? BuildFactory(Type type)
    {
        foreach (var iface in type.GetInterfaces())
        {
            var ifn = iface.FullName ?? "";
            if (!ifn.StartsWith("Windows.Foundation.IAsync", StringComparison.Ordinal)) continue;

            var completedProp = iface.GetProperty("Completed");
            if (completedProp?.GetSetMethod() is not { } setter) continue;

            var delegateType   = completedProp.PropertyType;
            var delegateInvoke = delegateType.GetMethod("Invoke");
            if (delegateInvoke == null) continue;

            var dp = delegateInvoke.GetParameters()
                .Select(p => Expression.Parameter(p.ParameterType))
                .ToArray();

            if (iface.IsGenericType)
            {
                var resultType = iface.GetGenericArguments()[0];
                var getResults = iface.GetMethod("GetResults");
                if (getResults == null) continue;

                var tcsType  = typeof(TaskCompletionSource<>).MakeGenericType(resultType);
                var taskProp = tcsType.GetProperty("Task")!;
                var completeM = typeof(WinRtAsyncCache)
                    .GetMethod(nameof(CompleteResult), BindingFlags.NonPublic | BindingFlags.Static)!
                    .MakeGenericMethod(resultType);

                return obj =>
                {
                    var tcs = Activator.CreateInstance(tcsType, TaskCreationOptions.RunContinuationsAsynchronously)!;
                    var body = Expression.Call(completeM,
                        Expression.Constant(tcs),
                        Expression.Convert(dp[0], typeof(object)),
                        Expression.Convert(dp[1], typeof(int)),
                        Expression.Constant(getResults));
                    setter.Invoke(obj, [Expression.Lambda(delegateType, body, dp).Compile()]);
                    return (Task)taskProp.GetValue(tcs)!;
                };
            }
            else
            {
                var completeM = typeof(WinRtAsyncCache)
                    .GetMethod(nameof(CompleteAction), BindingFlags.NonPublic | BindingFlags.Static)!;

                return obj =>
                {
                    var tcs = new TaskCompletionSource<object?>(TaskCreationOptions.RunContinuationsAsynchronously);
                    var body = Expression.Call(completeM,
                        Expression.Constant(tcs),
                        Expression.Convert(dp[1], typeof(int)));
                    setter.Invoke(obj, [Expression.Lambda(delegateType, body, dp).Compile()]);
                    return tcs.Task;
                };
            }
        }
        return null;
    }

    private static void CompleteResult<T>(object tcs, object sender, int status, MethodInfo getResults)
    {
        var typed = (TaskCompletionSource<T>)tcs;
        try
        {
            if (status == 1)
                typed.TrySetResult((T)getResults.Invoke(sender, null)!);
            else if (status == 2)
                typed.TrySetCanceled();
            else
                typed.TrySetException(new Exception($"WinRT async operation failed (status {status})"));
        }
        catch (TargetInvocationException ex) { typed.TrySetException(ex.InnerException ?? ex); }
        catch (Exception ex) { typed.TrySetException(ex); }
    }

    private static void CompleteAction(TaskCompletionSource<object?> tcs, int status)
    {
        if (status == 1) tcs.TrySetResult(null);
        else if (status == 2) tcs.TrySetCanceled();
        else tcs.TrySetException(new Exception($"WinRT async action failed (status {status})"));
    }
}
