import { MediaItem, PlayMessage } from 'common/Packets';

export function playMessageFromMediaItem(item: MediaItem) {
    return item ? new PlayMessage(
            item.container, item.url, item.content,
            item.time, item.volume, item.speed,
            item.headers, item.metadata
        ) : new PlayMessage("");
}

export function mediaItemFromPlayMessage(message: PlayMessage) {
    return message ? new MediaItem(
            message.container, message.url, message.content,
            message.time, message.volume, message.speed,
            null, null, message.headers, message.metadata
    ) : new MediaItem("");
}
