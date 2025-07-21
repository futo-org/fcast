import { Logger, LoggerType } from 'common/Logger';
import { ServiceManager } from 'lib/common';
require('lib/webOSTVjs-1.2.10/webOSTV.js');
require('lib/webOSTVjs-1.2.10/webOSTV-dev.js');

declare global {
    interface Window {
      webOSApp: any;
    }
}

const logger = new Logger('Main', LoggerType.FRONTEND);
const webPage: HTMLIFrameElement = document.getElementById('page') as HTMLIFrameElement;
let launchHandlerCallback = () => { logger.warn('No (re)launch handler set'); };
let keyDownEventHandler = () => { logger.warn('No keyDown event handler set'); };
let keyUpEventHandler = () => { logger.warn('No keyUp event handler set'); };

function loadPage(path: string) {
    // @ts-ignore
    webPage.src = path;
}

// We are embedding iframe element and using that for page navigation. This preserves a global JS context
// so bugs related to oversubscribing/canceling services are worked around by only subscribing once to
// required services
logger.info('Starting webOS application')
window.webOS.deviceInfo((info) => { logger.info('Device info:', info); });

window.webOSApp = {
    serviceManager: new ServiceManager(),
    setLaunchHandler: (callback: () => void) => {
        document.removeEventListener('webOSLaunch', launchHandlerCallback);
        document.removeEventListener('webOSRelaunch', launchHandlerCallback);

        launchHandlerCallback = callback;
        document.addEventListener('webOSLaunch', launchHandlerCallback);
        document.addEventListener('webOSRelaunch', launchHandlerCallback);
    },
    setKeyDownHandler: (callback: () => void) => {
        document.removeEventListener('keydown', keyDownEventHandler);

        keyDownEventHandler = callback;
        document.addEventListener('keydown', keyDownEventHandler);
    },
    setKeyUpHandler: (callback: () => void) => {
        document.removeEventListener('keyup', keyUpEventHandler);

        keyUpEventHandler = callback;
        document.addEventListener('keyup', keyUpEventHandler);
    },
    loadPage: loadPage,
    pendingPlay: null,
};

document.addEventListener('webOSLaunch', launchHandlerCallback);
document.addEventListener('webOSRelaunch', launchHandlerCallback);
document.addEventListener('keydown', keyDownEventHandler);
document.addEventListener('keyup', keyUpEventHandler);
loadPage('./main_window/index.html');
