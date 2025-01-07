export enum ToastIcon {
    INFO,
    ERROR,
}

const toastQueue = []

export function toast(message: string, icon: ToastIcon = ToastIcon.INFO, duration: number = 5000) {
    toastQueue.push({ message: message, icon: icon, duration: duration });

    if (toastQueue.length === 1) {
        renderToast(message, icon, duration);
    }
}

function renderToast(message: string, icon: ToastIcon = ToastIcon.INFO, duration: number = 5000) {
    const toastNotification = document.getElementById('toast-notification');
    const toastIcon = document.getElementById('toast-icon');
    const toastText = document.getElementById('toast-text');

    if (!(toastNotification && toastIcon && toastText)) {
        throw 'Toast component could not be initialized';
    }

    window.setTimeout(() => {
        toastNotification.className = 'toast-fade-out';
        toastNotification.style.opacity = '0';
        toastQueue.shift();

        if (toastQueue.length > 0) {
            window.setTimeout(() => {
                let toast = toastQueue[0];
                renderToast(toast.message, toast.icon, toast.duration);
            }, 1000);
        }
    }, duration);

    switch (icon) {
        case ToastIcon.INFO:
            toastIcon.style.backgroundImage = 'url(../assets/icons/app/info.svg)';
            break;

        case ToastIcon.ERROR:
            toastIcon.style.backgroundImage = 'url(../assets/icons/app/error.svg)';
            break;

        default:
            break;
    }

    toastText.textContent = message;
    toastNotification.className = 'toast-fade-in';
    toastNotification.style.opacity = '1';
}
