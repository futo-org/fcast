import { Logger, LoggerType } from 'common/Logger';
import { ServiceManager } from 'lib/common';

declare global {
    interface Window {
      webOSApp: any;
    }
}

const logger = new Logger('Main', LoggerType.FRONTEND);
const webPage: HTMLIFrameElement = document.getElementById('page') as HTMLIFrameElement;
let launchHandlerCallback = () => { logger.warn('No (re)launch handler set'); };

function loadPage(path: string) {
    // @ts-ignore
    webPage.src = path;
}

// We are embedding iframe element and using that for page navigation. This preserves a global JS context
// so bugs related to oversubscribing/canceling services are worked around by only subscribing once to
// required services
logger.info('Starting webOS application')

window.webOSApp = {
    serviceManager: new ServiceManager(),
    setLaunchHandler: (callback: () => void) => launchHandlerCallback = callback,
    loadPage: loadPage
};

document.addEventListener('webOSLaunch', launchHandlerCallback);
document.addEventListener('webOSRelaunch', launchHandlerCallback);
loadPage('./main_window/index.html');
