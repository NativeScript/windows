// Shared UI demo body. Opens a real OS window (the default browser) via WinRT, driven
// identically from Node, Bun, and Deno. Same idea as run-demo.mjs (one body, per-runtime
// loader) but this one produces visible UI instead of console output only.
//
// Windows.System.Launcher.LaunchUriAsync is one of the few WinRT UI-adjacent APIs that works
// unmodified from an unpackaged console host: it hands the URI to the shell (same as
// double-clicking a link) instead of needing package identity, an AppUserModelID, or a
// XAML-initialized thread the way Windows.UI.Xaml / Windows.UI.Notifications do.
export async function runUiDemo(runtimeName, api) {
  const { Windows, toPromise, enableAutoPump } = api;

  const line = (s = '') => console.log(s);
  const rule = () => line('-'.repeat(60));

  line(`@nativescript/windows-napi UI demo: running under ${runtimeName}`);
  rule();

  enableAutoPump();
  const uri = new Windows.Foundation.Uri('https://github.com/NativeScript/windows-runtime');
  line(`Built URI (Windows.Foundation.Uri): ${uri.AbsoluteUri}`);

  const launched = await toPromise(Windows.System.Launcher.LaunchUriAsync(uri));
  line(`Launched via the shell (Windows.System.Launcher.LaunchUriAsync): ${launched}`);

  rule();
  line(`Done: ${runtimeName} opened a real OS window through the same napi-rs addon Node uses.`);
  line('(Check your default browser: a new tab/window should have opened.)');
}
