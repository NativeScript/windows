// Generics closed over an enum: IAsyncOperation<Enum>, IVector<Enum> and IReference<Enum>. The
// closed type's IID hashes the enum's name, so the enum must not be reduced to Int32 there.
const { Windows, toPromise, awaitWithPump } = require('../nswinrt.js');

let pass = 0, fail = 0;
function check(name, cond) {
  if (cond) pass++;
  else { console.log(`FAIL ${name}`); fail++; }
}
function attempt(name, fn) {
  try { return fn(); } catch (e) { console.log(`FAIL ${name}: ${e.message}`); fail++; }
}

async function main() {
  // IAsyncOperation<VibrationAccessStatus>
  const Haptics = Windows.Devices.Haptics;
  const op = Haptics.VibrationDevice.RequestAccessAsync();
  check('IAsyncOperation<Enum> exposes Status', typeof op.Status === 'number');
  let status;
  try {
    status = await awaitWithPump(toPromise(op), 5000);
  } catch (e) {
    console.log(`FAIL toPromise(IAsyncOperation<Enum>): ${e.message}`);
    fail++;
  }
  check('IAsyncOperation<Enum> resolves to a number', typeof status === 'number');
  check('IAsyncOperation<Enum> GetResults after completion', attempt('GetResults', () => op.GetResults()) === status);

  // IVector<DeviceClass>
  const Enumeration = Windows.Devices.Enumeration;
  const classes = attempt('IVector<Enum>', () => new Enumeration.DevicePicker().Filter.SupportedDeviceClasses);
  if (classes) {
    attempt('IVector<Enum>.Append', () => {
      classes.Append(Enumeration.DeviceClass.AudioCapture);
      classes.Append(Enumeration.DeviceClass.VideoCapture);
    });
    check('IVector<Enum>.Size', classes.Size === 2);
    check('IVector<Enum>.GetAt', attempt('GetAt', () => classes.GetAt(0)) === Enumeration.DeviceClass.AudioCapture);
    check('IVector<Enum>[i]', classes[1] === Enumeration.DeviceClass.VideoCapture);
  }

  // IReference<ManualFocusDistance>
  const Devices = Windows.Media.Devices;
  const focus = new Devices.FocusSettings();
  check('IReference<Enum> starts null', focus.Distance == null);
  attempt('IReference<Enum> set', () => { focus.Distance = Devices.ManualFocusDistance.Nearest; });
  check('IReference<Enum> round-trips', focus.Distance === Devices.ManualFocusDistance.Nearest);
  attempt('IReference<Enum> set null', () => { focus.Distance = null; });
  check('IReference<Enum> clears to null', focus.Distance == null);

  console.log(`\n${pass} passed, ${fail} failed`);
  process.exitCode = fail ? 1 : 0;
}

main().catch((e) => { console.log('FATAL', e && e.stack || e); process.exitCode = 1; });
