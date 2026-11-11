// Replaces src/Main in the unit bundle: the real module starts the webOS service when imported.
export function getComputerName(): string {
    return 'FCast-unit-test';
}

export function getAppName(): string {
    return 'FCast Receiver webOS';
}

export function getAppVersion(): string {
    return '0.0.0-test';
}

export function getV4Context(): null {
    return null;
}

export function getPlayMessage(): null {
    return null;
}

export function getPlaybackUpdateMessage(): null {
    return null;
}

export async function errorHandler(error: Error): Promise<void> {
    throw error;
}
