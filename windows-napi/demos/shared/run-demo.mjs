// Shared demo body: identical WinRT calls run from Node, Bun, and Deno.
// Each runtime's entry point loads the native addon its own way (see ../deno/main.js and
// ../bun/index.js) and hands the resulting `nswinrt.js` exports to `runDemo` below, so this
// file proves the interop surface (not the loading mechanism) is what's shared.

export async function runDemo(runtimeName, api) {
  const { Windows, toPromise, enableAutoPump, interop, native } = api;

  const line = (s = '') => console.log(s);
  const rule = () => line('-'.repeat(60));

  line(`@nativescript/windows-napi demo: running under ${runtimeName}`);
  rule();

  // 1. Sync WinRT: read/write an instance property (Windows.Globalization.Calendar).
  const cal = new Windows.Globalization.Calendar();
  const now = `${cal.Year}-${String(cal.Month).padStart(2, '0')}-${String(cal.Day).padStart(2, '0')} ` +
    `${String(cal.Hour).padStart(2, '0')}:${String(cal.Minute).padStart(2, '0')}:${String(cal.Second).padStart(2, '0')}`;
  line(`WinRT clock (Windows.Globalization.Calendar): ${now}`);

  // 2. Sync WinRT: static factory + instance methods (Windows.Security.Cryptography).
  const CryptographicBuffer = Windows.Security.Cryptography.CryptographicBuffer;
  const randomBuffer = CryptographicBuffer.GenerateRandom(16);
  const token = CryptographicBuffer.EncodeToHexString(randomBuffer);
  line(`Random session token (CryptographicBuffer.GenerateRandom): ${token}`);

  // 3. Sync WinRT: build + stringify a JsonObject (Windows.Data.Json).
  const JsonValue = Windows.Data.Json.JsonValue;
  const payload = new Windows.Data.Json.JsonObject();
  payload.SetNamedValue('runtime', JsonValue.CreateStringValue(runtimeName));
  payload.SetNamedValue('token', JsonValue.CreateStringValue(token));
  payload.SetNamedValue('pid', JsonValue.CreateNumberValue(globalThis.process?.pid ?? -1));
  line(`JSON round-trip (Windows.Data.Json.JsonObject.Stringify): ${payload.Stringify()}`);

  // 4. Async WinRT → JS Promise: run work on a WinRT thread-pool thread and await it like any
  // other promise. enableAutoPump() keeps the STA message loop pumped in the background so no
  // manual pumping is needed here.
  enableAutoPump();
  const ThreadPool = Windows.System.Threading.ThreadPool;
  const started = globalThis.performance?.now?.() ?? Date.now();
  let ranOnPool = false;
  await toPromise(ThreadPool.RunAsync(() => { ranOnPool = true; }));
  const elapsed = (globalThis.performance?.now?.() ?? Date.now()) - started;
  line(`Async round-trip (ThreadPool.RunAsync → Promise): completed=${ranOnPool} in ${elapsed.toFixed(2)}ms`);

  // 5. Native-side formatting helpers, ported straight from the standalone runtime's console.
  rule();
  line(native.tableFor([
    { step: 'Calendar', kind: 'sync', result: now },
    { step: 'CryptographicBuffer', kind: 'sync', result: token },
    { step: 'JsonObject', kind: 'sync', result: payload.Stringify() },
    { step: 'ThreadPool.RunAsync', kind: 'async', result: `${elapsed.toFixed(2)}ms` },
  ]));

  line(`uuid (interop.nsUuid via installInterop): ${interop.uuid ? interop.uuid() : native.nsUuid()}`);
  rule();
  line(`Done: ${runtimeName} drove real Windows Runtime APIs through the same napi-rs addon Node uses.`);
}
