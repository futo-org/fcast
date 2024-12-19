/* eslint-disable @typescript-eslint/no-explicit-any */
import { preloadData } from 'common/main/Preload';

enum RemoteKeyCode {
    Stop = 413,
    Rewind = 412,
    Play = 415,
    Pause = 19,
    FastForward = 417,
    Back = 461,
}

const serviceId = 'com.futo.fcast.receiver.service';

const playService = window.webOS.service.request(`luna://${serviceId}/`, {
    method:"play",
    parameters: {},
    onSuccess: (message: any) => {
        if (message.value.subscribed === true) {
            console.log('Main: Registered play handler with service');
        }
        else {
            if (message.value !== undefined && message.value.playData !== undefined) {
                console.log(`Main: Playing ${JSON.stringify(message)}`);
                preloadData.getDeviceInfoService.cancel();
                playService.cancel();

                // WebOS 22 and earlier does not work well using the history API,
                // so manually handling page navigation...
                // history.pushState({}, '', '../main_window/index.html');
                window.open('../player/index.html');
            }
        }
    },
    onFailure: (message: any) => {
        console.error(`Main: play ${JSON.stringify(message)}`);
    },
    subscribe: true,
    resubscribe: true
});

const launchHandler = (args: any) => {
    // args don't seem to be passed in via event despite what documentation says...
    const params = window.webOSDev.launchParams();
    console.log(`Main: (Re)launching FCast Receiver with args: ${JSON.stringify(params)}`);

    const lastTimestamp = localStorage.getItem('lastTimestamp');
    if (params.playData !== undefined && params.timestamp != lastTimestamp) {
        localStorage.setItem('lastTimestamp', params.timestamp);
        if (preloadData.getDeviceInfoService !== undefined) {
            preloadData.getDeviceInfoService.cancel();
        }
        if (playService !== undefined) {
            playService.cancel();
        }

        // WebOS 22 and earlier does not work well using the history API,
        // so manually handling page navigation...
        // history.pushState({}, '', '../main_window/index.html');
        window.open('../player/index.html');
    }
};

document.addEventListener('webOSLaunch', (ags) => { console.log('lunch'); launchHandler(ags)});
document.addEventListener('webOSRelaunch', (ags) => { console.log('relun'); launchHandler(ags)});

// Cannot go back to a state where user was previously casting a video, so exit.
// window.onpopstate = () => {
//     window.webOS.platformBack();
// };

document.addEventListener('keydown', (event: any) => {
    // console.log("KeyDown", event);

    switch (event.keyCode) {
        // WebOS 22 and earlier does not work well using the history API,
        // so manually handling page navigation...
        case RemoteKeyCode.Back:
            window.webOS.platformBack();
            break;
        default:
            break;
    }
});
