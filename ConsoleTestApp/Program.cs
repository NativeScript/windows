using System.Runtime.InteropServices;

#if DEBUG
// Use the simple DLL name so the loader resolves the copy placed next to the
// executable during build. The previous relative path fails when the process
// working directory differs from the repo root.
const string NativeScriptLibrary = "nativescript";
#else
const string NativeScriptLibrary = "nativescript";
#endif

[DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_init))]
static extern Int64 runtime_init([MarshalAs(UnmanagedType.LPUTF8Str)] string entry);

[DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_deinit))]
static extern void runtime_deinit(Int64 runtime);

[DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_runscript))]
static extern void runtime_runscript(Int64 runtime, [MarshalAs(UnmanagedType.LPUTF8Str)] string entry, [MarshalAs(UnmanagedType.LPUTF8Str)] string filename);

#if DEBUG
[DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_devtools_start))]
static extern IntPtr runtime_devtools_start(long runtime, ushort port);

[DllImport(NativeScriptLibrary, EntryPoint = nameof(runtime_free_string))]
static extern void runtime_free_string(IntPtr ptr);
#endif

var baseDir = AppDomain.CurrentDomain.BaseDirectory;
var lowerEntry = Path.Combine(baseDir, "app", "main.js");
var upperEntry = Path.Combine(baseDir, "App", "main.js");
string entry = System.IO.File.Exists(lowerEntry) ? lowerEntry : upperEntry;
Int64 runtime = runtime_init(AppContext.BaseDirectory);
#if DEBUG
IntPtr devtoolsPtr = IntPtr.Zero;
try {
	// runtime_devtools_start may not be present in non-devtools builds of the
	// native DLL; guard the call and continue if the symbol is missing.
	devtoolsPtr = runtime_devtools_start(runtime, 43000);
} catch (EntryPointNotFoundException) {
	devtoolsPtr = IntPtr.Zero;
}
if (devtoolsPtr != IntPtr.Zero)
{
	try {
		var ws = Marshal.PtrToStringUTF8(devtoolsPtr);
		try { runtime_free_string(devtoolsPtr); } catch (EntryPointNotFoundException) { }
		if (!string.IsNullOrEmpty(ws)) Console.WriteLine($"[NativeScript DevTools] {ws}");
	} catch (Exception) {
		// Ignore any errors reading the devtools string.
	}
}
#endif
var script = File.ReadAllText(Path.GetFullPath(entry));
runtime_runscript(runtime, script, Path.GetFileName(entry));

Console.WriteLine("Hello, World!");

runtime_deinit(runtime);
