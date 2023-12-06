document.addEventListener('DOMContentLoaded', function() {
    updateUrlList();
    updateHostList();
    updateVolume();
    updatePlaybackState();

    document.getElementById('clearAll').addEventListener('click', function() {
        chrome.runtime.sendMessage({ action: 'clearAll' });
    });

    document.getElementById('addHost').addEventListener('click', function() {
        const host = prompt('Enter new host (ip:port):');
        if (host) {
            chrome.runtime.sendMessage({ action: 'addHost', host: host });
        }
    });

    chrome.runtime.onMessage.addListener(function(request, sender, sendResponse) {
        if (request.action === 'updateUrls') {
            updateUrlList();
        } else if (request.action === 'updateHosts') {
            updateHostList();
        } else if (request.action == 'updateVolume') {
            updateVolume();
        } else if (request.action == 'updatePlaybackState') {
            updatePlaybackState();
        }
    });
});

function updateUrlList() {
    console.log("updateUrlList");

    chrome.runtime.sendMessage({ action: 'getUrls' }, function(response) {
        console.log("getUrls response", response);

        const urlList = document.getElementById('urlList');
        urlList.innerHTML = '';
        response.urls.forEach(url => {
            const listItem = document.createElement('li');
            listItem.classList.add('url-item');

            const urlText = document.createElement('div');
            urlText.textContent = url.url;

            const buttonContainer = document.createElement('div');
            buttonContainer.classList.add('action-buttons');

            const castButton = document.createElement('button');
            castButton.textContent = 'C';
            castButton.disabled = !response.selectedHost;
            castButton.addEventListener('click', function() {
                if (response.selectedHost) {
                    chrome.runtime.sendMessage({ action: 'castVideo', url });
                }
            });    
            buttonContainer.appendChild(castButton);

            listItem.appendChild(urlText);
            listItem.appendChild(buttonContainer);

            urlList.appendChild(listItem);
        });
    });
}

function updateHostList() {
    console.log("updateHostList");

    chrome.runtime.sendMessage({ action: 'getHosts' }, function(response) {
        console.log("getHosts response", response);

        const hostList = document.getElementById('hostList');
        hostList.innerHTML = '';
        console.log("response.hosts", response.hosts);
        response.hosts.forEach(host => {
            const listItem = document.createElement('li');
            if (host === response.selectedHost) {
                listItem.style.color = 'green';
            }

            listItem.style.display = 'flex';
            listItem.style.justifyContent = 'space-between';
            listItem.style.alignItems = 'center';

            const hostText = document.createElement('span');
            hostText.textContent = host;
            hostText.style.flexGrow = 1;
            listItem.appendChild(hostText);

            const selectButton = document.createElement('button');
            if (host === response.selectedHost) {
                selectButton.textContent = 'Unselect';
                selectButton.classList.add('button-red');
                selectButton.addEventListener('click', function() {
                    chrome.runtime.sendMessage({ action: 'selectHost', host: null });
                });
            } else {
                selectButton.textContent = 'Select';
                selectButton.addEventListener('click', function() {
                    chrome.runtime.sendMessage({ action: 'selectHost', host: host });
                });
            }
            listItem.appendChild(selectButton);

            const deleteButton = document.createElement('button');
            deleteButton.textContent = 'Delete';
            deleteButton.addEventListener('click', function() {
                chrome.runtime.sendMessage({ action: 'deleteHost', host: host });
            });
            listItem.appendChild(deleteButton);

            hostList.appendChild(listItem);
        });

        const controlsDiv = document.getElementById('timeBarControls');
        const timeBar = document.getElementById('timeBar');
        const resumeButton = document.getElementById('resumeButton');
        const pauseButton = document.getElementById('pauseButton');
        const stopButton = document.getElementById('stopButton');
        const volumeControl = document.getElementById('volumeControl');
    
        if (response.selectedHost) {
            controlsDiv.style.opacity = 1;
            timeBar.disabled = false;
            resumeButton.disabled = false;
            pauseButton.disabled = false;
            stopButton.disabled = false;
            volumeControl.disabled = false;
    
            timeBar.addEventListener('input', handleSeek);
            resumeButton.addEventListener('click', handleResume);
            pauseButton.addEventListener('click', handlePause);
            stopButton.addEventListener('click', handleStop);
            volumeControl.addEventListener('input', handleVolumeChanged);
        } else {
            controlsDiv.style.opacity = 0.5;
            timeBar.disabled = true;
            resumeButton.disabled = true;
            pauseButton.disabled = true;
            stopButton.disabled = true;
            volumeControl.disabled = true;
    
            timeBar.removeEventListener('input', handleSeek);
            resumeButton.removeEventListener('click', handleResume);
            pauseButton.removeEventListener('click', handlePause);
            stopButton.removeEventListener('click', handleStop);
            volumeControl.removeEventListener('input', handleVolumeChanged);
        }
    });
}

function updateVolume() {
    console.log("updateVolume");

    chrome.runtime.sendMessage({ action: 'getVolume' }, function (response) {
        const volumeControl = document.getElementById('volumeControl');
        if (response.volume) {
            volumeControl.value = response.volume * 100;
        } else {
            volumeControl.disabled = true;
        }
    });
}

function updatePlaybackState() {
    console.log("updatePlaybackState");

    chrome.runtime.sendMessage({ action: 'getPlaybackState' }, function (response) {
        const timeBar = document.getElementById('timeBar');
        const resumeButton = document.getElementById('resumeButton');
        const pauseButton = document.getElementById('pauseButton');
        const stopButton = document.getElementById('stopButton');
        const volumeControl = document.getElementById('volumeControl');

        if (!response.selectedHost || !response.playbackState || response.playbackState.state === 0) {
            resumeButton.disabled = true;
            pauseButton.disabled = true;
            stopButton.disabled = true;
            timeBar.disabled = true;
            volumeControl.disabled = true;
            return;
        }

        timeBar.max = response.playbackState.duration * 1000;
        timeBar.value = response.playbackState.time * 1000;

        stopButton.disabled = false;
        timeBar.disabled = false;
        volumeControl.disabled = false;

        switch (response.playbackState.state) {
            case 1: // Playing
                resumeButton.disabled = true;
                pauseButton.disabled = false;
                break;
            case 2: // Paused
                resumeButton.disabled = false;
                pauseButton.disabled = true;
                break;
        }        
    });
}

function handleSeek(event) {
    console.log("handleSeek", event);
    chrome.runtime.sendMessage({ action: 'seek', time: parseFloat(event.target.value) / 1000.0 });
}

function handleResume(event) {
    console.log("handleResume", event);
    chrome.runtime.sendMessage({ action: 'resume' });
}

function handlePause(event) {
    console.log("handlePause", event);
    chrome.runtime.sendMessage({ action: 'pause' });
}

function handleStop(event) {
    console.log("handleStop", event);
    chrome.runtime.sendMessage({ action: 'stop' });
}

function handleVolumeChanged(event) {
    console.log("handleVolumeChanged", event);
    chrome.runtime.sendMessage({ action: 'setVolume', volume: parseFloat(event.target.value) / 100.0 });
}