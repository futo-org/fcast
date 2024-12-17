import 'common/main/Preload';

// Cannot go back to a state where user was previously casting a video, so exit.
window.onpopstate = () => {
    window.webOS.platformBack();
};
