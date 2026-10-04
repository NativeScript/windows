using System;
using System.Threading;

// Lifetime of JS subclass instances (tests/lifetime.js): the managed half of an instance JS has
// dropped must become collectable, while one native code still holds keeps working.
namespace TestFixtures.Lifetime
{
    public class Tracked
    {
        private static int s_finalized;

        public static int Finalized => Volatile.Read(ref s_finalized);

        public virtual string Name() => "tracked";

        ~Tracked() => Interlocked.Increment(ref s_finalized);
    }

    public static class Gc
    {
        public static void Collect()
        {
            GC.Collect();
            GC.WaitForPendingFinalizers();
            GC.Collect();
        }
    }
}
