interface TestCase {
    name: string;
    fn: () => void | Promise<void>;
}

const tests: TestCase[] = [];

export function test(name: string, fn: () => void | Promise<void>): void {
    tests.push({ name, fn });
}

function describeError(error: any): string {
    return error && error.message ? error.message : String(error);
}

export async function run(): Promise<void> {
    let pass = 0;
    let fail = 0;
    for (const t of tests) {
        try {
            await t.fn();
            pass++;
            console.log('ok ' + t.name);
        } catch (error) {
            fail++;
            console.log('not ok ' + t.name + ': ' + describeError(error));
        }
    }
    console.log('# pass ' + pass + ' fail ' + fail);
    // Exits explicitly: a test may leave a repeating timer of the code under test running.
    process.exit(fail > 0 ? 1 : 0);
}
