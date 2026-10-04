# Windows runtime test app

The Windows counterpart of the Android (`test-app`) and iOS (`TestRunner`) runtime test suites: Jasmine-style specs that run inside a real WinUI app built from `template/framework`, against native fixtures compiled into it.

```
test-app/
  app/                         JavaScript: main.js (entry), runner/ (Jasmine-compatible runner), tests/
  App_Resources/Windows/
    src/TestFixtures/*.cs      C# fixtures, compiled into the app like any app's App_Resources sources
    app.csproj                 app-level MSBuild hook (unpackaged build for the runner)
  plugins/test-plugin/         a plugin shipping C# sources in platforms/windows/src
  run.ps1                      build + run
```

## Running

From the repository root, in a shell where `cargo` and the .NET SDK are available:

```powershell
pwsh test-app/run.ps1                    # build nativescript.dll (debug) and the app, run every spec
pwsh test-app/run.ps1 -Filter Threading  # only specs whose full name matches the regex
pwsh test-app/run.ps1 -SkipRuntimeBuild  # reuse target/debug/nativescript.dll
pwsh test-app/run.ps1 -Clean             # regenerate platforms/ from scratch
```

`run.ps1` does what `ns platform add windows` and `ns prepare windows` do: it generates `test-app/platforms/windows/TestRunner` from `template/framework`, copies `app/` and `App_Resources/Windows` into it, and stages each plugin's `platforms/windows` folder under `plugins/<name>`. It uses the freshly built runtime DLL and the `dotnet-bridge` sources from this repository. `dotnet-tool` runs from source through `cargo`. Pass `-EngineDll` to test a napi engine build (for example `packages/windows-v8/target/release/windows_v8.dll`) instead of the classic runtime.

The app is built unpackaged (`WindowsPackageType=None`, self-contained Windows App SDK) so it starts straight from `bin\` and can read environment variables. When the run finishes:

- `bin/test-results.log` holds one `[PASS]`/`[FAIL]` line per spec plus a `[TEST SUMMARY]` line;
- `bin/test-results.xml` holds JUnit XML;
- the process exit code is the number of failed specs.

The script prints the log and exits non-zero when any spec failed.

## What the specs cover

| Spec | Covers |
| --- | --- |
| `csharp-sources.js` | App and plugin C# types reachable by namespace with no registration step: constructors and overloads, properties, statics, arrays, enums, internal types, nested namespaces. Also JS functions passed as `Func<>`/`Action<>`, C# events, and `Task` results awaited without blocking the UI thread. |
| `extend-dotnet.js` | Extending C# classes with `Class.extend()`, ES2015 `class`, `@NativeClass` and TypeScript ES5 output. Native code reaches JS overrides; constructor arguments are forwarded. `super` and `this.super` reach the base implementation; protected and abstract members and virtual properties can be overridden. JS fields are visible to native calls, `instanceof` works, and identity is kept across native round-trips. Interfaces are implemented with `new Interface({...})` and `@Interfaces`. |
| `extend-winrt.js` | Extending WinUI classes: XAML calls `MeasureOverride`/`ArrangeOverride`/`OnApplyTemplate` on the JS subclass. Covers `super`, `instanceof` of the subclass and its WinRT bases, `Class.extend()` and TypeScript ES5 output. |
| `threading.js` | WinRT and .NET callbacks invoked on background threads are delivered on the JS thread that created them (the UI thread or a worker). Return values and exceptions flow back to the native caller. Worker specs are skipped on engines without workers. |
| `boxing.js` | Values in `Object`-typed members (`Tag`, `PropertySet`) read back as JS primitives. |
| `lifetime.js` | JS subclass instances that nothing references are collected on both sides. An instance only native code holds keeps working, with its JS object revived. WinRT UI subclasses in the visual tree keep their fields. Needs `gc()`, so it is skipped on engines that don't expose it. |
| `layout-mutation.js` | Adding, removing and reparenting XAML elements synchronously from `SizeChanged`, `LayoutUpdated` and `CompositionTarget.Rendering`, and from promise continuations scheduled there. |

Each spec asserts behaviour that works the same way on iOS and Android.

## Adding specs

Add a file under `app/tests/` and list it in `app/main.js`. If it needs a native fixture, add a `.cs` file under `App_Resources/Windows/src/TestFixtures/`; it is compiled into the app and reachable from JS by namespace. The runner supports `describe`/`it`/`xit`/`fit`, `beforeEach`/`afterEach`/`beforeAll`/`afterAll`, async specs (`done` or a returned promise) and the usual `expect` matchers.
