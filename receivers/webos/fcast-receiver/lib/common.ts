import { v4 as uuidv4 } from 'modules/uuid';
import { Logger, LoggerType } from 'common/Logger';
require('lib/webOSTVjs-1.2.10/webOSTV.js');
require('lib/webOSTVjs-1.2.10/webOSTV-dev.js');

const logger = new Logger('Common', LoggerType.FRONTEND);
const serviceId = 'com.futo.fcast.receiver.service';

export enum RemoteKeyCode {
    Stop = 413,
    Rewind = 412,
    Play = 415,
    Pause = 19,
    FastForward = 417,
    Back = 461,
}

export class ServiceManager {
    private static serviceChannelSuccessCbHandler?: (message: any) => void;
    private static serviceChannelFailureCbHandler?: (message: any) => void;
    private static serviceChannelCompleteCbHandler?: (message: any) => void;

    constructor() {
        // @ts-ignore
        window.webOS.service.request(`luna://${serviceId}/`, {
            method: 'service_channel',
            parameters: { subscriptionId: uuidv4() },
            onSuccess: (message: any) => {
                if (message.value?.subscribed === true) {
                    logger.info(`requestService: Registered 'service_channel' handler with service`);
                }
                else if (ServiceManager.serviceChannelSuccessCbHandler) {
                    ServiceManager.serviceChannelSuccessCbHandler(message);
                }
            },
            onFailure: (message: any) => {
                logger.error('Error subscribing to the service_channel:', message);

                if (ServiceManager.serviceChannelFailureCbHandler) {
                    ServiceManager.serviceChannelFailureCbHandler(message);
                }
            },
            onComplete: (message: any) => {
                if (ServiceManager.serviceChannelCompleteCbHandler) {
                    ServiceManager.serviceChannelCompleteCbHandler(message);
                }
            },
            subscribe: true,
            resubscribe: true
        });
    }


    public subscribeToServiceChannel(successCb: (message: any) => void, failureCb?: (message: any) => void, onCompleteCb?: (message: any) => void) {
        ServiceManager.serviceChannelSuccessCbHandler = successCb;
        ServiceManager.serviceChannelFailureCbHandler = failureCb;
        ServiceManager.serviceChannelCompleteCbHandler = onCompleteCb;
    }

    public call(method: string, parameters?: any, successCb?: (message: any) => void, failureCb?: (message: any) => void, onCompleteCb?: (message: any) => void) {
        // @ts-ignore
        const service = window.webOS.service.request(`luna://${serviceId}/`, {
                method: 'app_channel',
                parameters: { event: method, value: parameters },
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

        return service;
    }
}

// CSS media queries do not work on older webOS versions...
export function initializeWindowSizeStylesheet() {
    const resolution = sessionStorage.getItem('resolution');

    if (resolution) {
        window.onload = () => {
            if (resolution == '1920x1080') {
                document.head.insertAdjacentHTML('beforeend', '<link rel="stylesheet" href="./1920x1080.css" />');
            }
            else {
                document.head.insertAdjacentHTML('beforeend', '<link rel="stylesheet" href="./1280x720.css" />');
            }
        }
    }
    else {
        window.onresize = () => {
            if (window.innerWidth >= 1920 && window.innerHeight >= 1080) {
                sessionStorage.setItem('resolution', '1920x1080');
                document.head.insertAdjacentHTML('beforeend', '<link rel="stylesheet" href="./1920x1080.css" />');
            }
            else {
                sessionStorage.setItem('resolution', '1280x720');
                document.head.insertAdjacentHTML('beforeend', '<link rel="stylesheet" href="./1280x720.css" />');
            }
        };
    }
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
