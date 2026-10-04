using System;
using System.Threading;
using System.Threading.Tasks;

// Calls into JavaScript from threads other than the UI thread (tests/threading.js). On iOS and
// Android a callback invoked from a background thread still reaches JavaScript; these fixtures
// check the same on Windows. None of them block the UI thread while the background thread calls
// back, because that deadlocks on every runtime (the UI thread owns the JS isolate).
namespace TestFixtures.Threading
{
    public static class BackgroundInvoker
    {
        public static int UiThreadId { get; private set; }

        public static void CaptureUiThread() => UiThreadId = Environment.CurrentManagedThreadId;

        public static bool IsUiThread() => Environment.CurrentManagedThreadId == UiThreadId;

        /// Calls `compute` on a thread-pool thread and reports its result through `done`, also
        /// from that thread: (result, ranOnBackgroundThread).
        public static void ComputeInBackground(Func<int, int> compute, int value, Action<int, bool> done)
        {
            Task.Run(() =>
            {
                var background = Environment.CurrentManagedThreadId != UiThreadId;
                var result = compute(value);
                done(result, background);
            });
        }

        /// Calls `callback` from `count` background threads at once and reports how many calls
        /// returned, from the last thread to finish.
        public static void FanOut(Func<int, int> callback, int count, Action<int, int> done)
        {
            var remaining = count;
            var sum = 0;
            for (var i = 0; i < count; i++)
            {
                var n = i;
                new Thread(() =>
                {
                    Interlocked.Add(ref sum, callback(n));
                    if (Interlocked.Decrement(ref remaining) == 0) done(count, sum);
                }) { IsBackground = true }.Start();
            }
        }

        public static void ThrowInBackground(Func<int> callback, Action<string> done)
        {
            Task.Run(() =>
            {
                try
                {
                    callback();
                    done("no-exception");
                }
                catch (Exception e)
                {
                    done(e.GetType().Name);
                }
            });
        }
    }

    public class Ticker
    {
        public event EventHandler<int> Tick;

        public void StartInBackground(int ticks)
        {
            new Thread(() =>
            {
                for (var i = 1; i <= ticks; i++) Tick?.Invoke(this, i);
            }) { IsBackground = true }.Start();
        }
    }
}
