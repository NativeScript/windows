// E2E: real WinRT async → JS Promise. Exercises the full stack — delegate arg (IWorkItemHandler),
// IAsyncAction.Completed event wiring, Status property, message-pump completion delivery.
const { native, Windows, toPromise, awaitWithPump } = require('../nswinrt.js');

let pass = 0, fail = 0;
function check(name, cond) {
  if (cond) pass++;
  else { console.log(`FAIL ${name}`); fail++; }
}

async function main() {
  // toPromise on a null / non-async value.
  check('toPromise(null)', (await toPromise(null)) === null);
  check('toPromise(42)', (await toPromise(42)) === 42);

  // ThreadPool.RunAsync(handler) → IAsyncAction that completes on a pool thread.
  const ThreadPool = Windows.System.Threading.ThreadPool;
  let ran = false;
  const action = ThreadPool.RunAsync(() => { ran = true; });
  check('RunAsync returns async op', action && 'Completed' in action);

  const result = await awaitWithPump(toPromise(action), 5000);
  check('async action completed', ran === true);
  check('IAsyncAction result is undefined', result === undefined);

  // A second run to confirm the pump loop + Completed wiring is reusable.
  let ran2 = false;
  await awaitWithPump(toPromise(ThreadPool.RunAsync(() => { ran2 = true; })), 5000);
  check('second async action completed', ran2 === true);

  // Transparent async: with auto-pump enabled, a plain `await` (no manual pumping) works.
  const { enableAutoPump, disableAutoPump } = require('../nswinrt.js');
  enableAutoPump(2);
  let ran3 = false;
  await toPromise(ThreadPool.RunAsync(() => { ran3 = true; }));
  check('auto-pump transparent await', ran3 === true);

  // IAsyncOperation<T>: toPromise resolves to GetResults() and rejects on failure.
  const StorageFolder = Windows.Storage.StorageFolder;
  const folder = await toPromise(StorageFolder.GetFolderFromPathAsync('C:\\Windows'));
  check('toPromise(IAsyncOperation<T>) resolves to GetResults()', folder && folder.Name === 'Windows');

  let rejected = false;
  try {
    await toPromise(StorageFolder.GetFolderFromPathAsync('C:\\ns-does-not-exist-7f3a'));
  } catch (_) {
    rejected = true;
  }
  check('toPromise of a failing operation rejects', rejected);

  // Operations are not thenable, so they stay operations and can be passed back to native.
  const op = StorageFolder.GetFolderFromPathAsync('C:\\Windows');
  check('operation is not thenable', typeof op.then === 'undefined');
  check('Promise.resolve keeps the operation', (await Promise.resolve(op)) === op);
  check('operation exposes IAsyncInfo.Status', typeof op.Status === 'number');
  await toPromise(op);
  disableAutoPump();

  console.log(`\n${pass} passed, ${fail} failed`);
  process.exitCode = fail ? 1 : 0;
}

main().catch((e) => { console.log('FATAL', e && e.stack || e); process.exitCode = 1; });
