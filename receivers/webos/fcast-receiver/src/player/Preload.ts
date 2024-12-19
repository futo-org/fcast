/* eslint-disable @typescript-eslint/no-explicit-any */
import { preloadData } from 'common/player/Preload';

const launchHandler = (args: any) => {
    // args don't seem to be passed in via event despite what documentation says...
    const params = window.webOSDev.launchParams();
    console.log(`Player: (Re)launching FCast Receiver with args: ${JSON.stringify(params)}`);

    const lastTimestamp = localStorage.getItem('lastTimestamp');
    if (params.playData !== undefined && params.timestamp != lastTimestamp) {
        localStorage.setItem('lastTimestamp', params.timestamp);

        if (preloadData.playerWindowOpen !== undefined) {
            preloadData.playerWindowOpen = false;
        }
        if (preloadData.playService !== undefined) {
            preloadData.playService.cancel();
        }
        if (preloadData.pauseService !== undefined) {
            preloadData.pauseService.cancel();
        }
        if (preloadData.resumeService !== undefined) {
            preloadData.resumeService.cancel();
        }
        if (preloadData.stopService !== undefined) {
            preloadData.stopService.cancel();
        }
        if (preloadData.seekService !== undefined) {
            preloadData.seekService.cancel();
        }
        if (preloadData.setVolumeService !== undefined) {
            preloadData.setVolumeService.cancel();
        }
        if (preloadData.setSpeedService !== undefined) {
            preloadData.setSpeedService.cancel();
        }

        // WebOS 22 and earlier does not work well using the history API,
        // so manually handling page navigation...
        // history.pushState({}, '', '../main_window/index.html');
        window.open('../player/index.html');
    }
};

document.addEventListener('webOSLaunch', (ags) => { console.log('lunch'); launchHandler(ags)});
document.addEventListener('webOSRelaunch', (ags) => { console.log('relun'); launchHandler(ags)});
