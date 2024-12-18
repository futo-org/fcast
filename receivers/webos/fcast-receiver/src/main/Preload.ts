import 'common/main/Preload';

enum RemoteKeyCode {
    Stop = 413,
    Rewind = 412,
    Play = 415,
    Pause = 19,
    FastForward = 417,
    Back = 461,
}

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
