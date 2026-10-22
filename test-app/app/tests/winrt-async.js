// WinRT async operations (IAsyncAction, IAsyncOperation<T> and the WithProgress variants) are
// converted to promises explicitly with NSWinRT.toPromise. They are not thenable, so an
// operation stays an operation and can be passed back to native code.

const StorageFolder = Windows.Storage.StorageFolder;
const ThreadPool = Windows.System.Threading.ThreadPool;
const WINDOWS_DIR = "C:\\Windows";

describe("WinRT async operations", () => {
	it("resolves an IAsyncOperation<T> to its result", async () => {
		const folder = await NSWinRT.toPromise(StorageFolder.GetFolderFromPathAsync(WINDOWS_DIR));
		expect(folder.Name).toBe("Windows");
	});

	it("resolves an IAsyncAction to undefined", async () => {
		let ran = false;
		const result = await NSWinRT.toPromise(
			ThreadPool.RunAsync(() => {
				ran = true;
			}),
		);
		expect(ran).toBe(true);
		expect(result).toBe(undefined);
	});

	it("rejects when the operation fails", async () => {
		let error = null;
		try {
			await NSWinRT.toPromise(StorageFolder.GetFolderFromPathAsync("C:\\ns-does-not-exist-7f3a"));
		} catch (e) {
			error = e;
		}
		expect(error).not.toBe(null);
	});

	it("is not thenable, so promises keep the operation itself", async () => {
		const op = StorageFolder.GetFolderFromPathAsync(WINDOWS_DIR);
		expect(typeof op.then).toBe("undefined");
		expect(await Promise.resolve(op)).toBe(op);
		await NSWinRT.toPromise(op);
	});

	it("exposes the IAsyncInfo members it inherits", async () => {
		const op = StorageFolder.GetFolderFromPathAsync(WINDOWS_DIR);
		expect("Completed" in op).toBe(true);
		expect(typeof op.Status).toBe("number");
		expect(typeof op.Cancel).toBe("function");
		await NSWinRT.toPromise(op);
		expect(op.Status).toBe(1);
	});
});
