const logger = window.targetAPI.logger;
const serviceId = 'com.futo.fcast.receiver.service';

export enum RemoteKeyCode {
    Stop = 413,
    Rewind = 412,
    Play = 415,
    Pause = 19,
    FastForward = 417,
    Back = 461,
}

export function requestService(method: string, successCb: (message: any) => void, failureCb?: (message: any) => void, onCompleteCb?: (message: any) => void): any {
    return window.webOS.service.request(`luna://${serviceId}/`, {
        method: method,
        parameters: {},
        onSuccess: (message: any) => {
            if (message.value?.subscribed === true) {
                logger.info(`requestService: Registered ${method} handler with service`);
            }
            else {
                successCb(message);
            }
        },
        onFailure: (message: any) => {
            logger.error(`requestService: ${method} ${JSON.stringify(message)}`);

            if (failureCb) {
                failureCb(message);
            }
        },
        onComplete: (message: any) => {
            if (onCompleteCb) {
                onCompleteCb(message);
            }
        },
        subscribe: true,
        resubscribe: true
    });
}

export function callService(method: string, parameters?: any, successCb?: (message: any) => void, failureCb?: (message: any) => void, onCompleteCb?: (message: any) => void) {
    return window.webOS.service.request(`luna://${serviceId}/`, {
            method: method,
            parameters: parameters,
            onSuccess: (message: any) => {
                if (successCb) {
                    successCb(message);
                }
            },
            onFailure: (message: any) => {
                logger.error(`callService: ${method} ${JSON.stringify(message)}`);

                if (failureCb) {
                    failureCb(message);
                }
            },
            onComplete: (message: any) => {
                if (onCompleteCb) {
                    onCompleteCb(message);
                }
            },
            subscribe: false,
            resubscribe: false
    });
}

export function targetKeyUpEventListener(event: KeyboardEvent): { handledCase: boolean, key: string } {
    let handledCase = false;
    let key = '';

    // .keyCode instead of alternatives is required to work properly on webOS
    switch (event.keyCode) {
        // Unhandled cases (used for replacing undefined key codes)
        case RemoteKeyCode.Stop:
            key = 'Stop';
            break;
        case RemoteKeyCode.Rewind:
            key = 'Rewind';
            break;
        case RemoteKeyCode.Play:
            key = 'Play';
            break;
        case RemoteKeyCode.Pause:
            key = 'Pause';
            break;
        case RemoteKeyCode.FastForward:
            key = 'FastForward';
            break;
        case RemoteKeyCode.Back:
            key = 'Back';
            break;
        default:
            break;
    }

    return { handledCase: handledCase, key: key };
};
