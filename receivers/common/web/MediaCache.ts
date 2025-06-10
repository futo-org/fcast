import { PlaylistContent } from 'common/Packets';
import { downloadFile } from 'common/UtilityBackend';
import { Logger, LoggerType } from 'common/Logger';
import { fs } from 'modules/memfs';
import { v4 as uuidv4 } from 'modules/uuid';
import { Readable } from 'stream';
import * as os from 'os';
const logger = new Logger('MediaCache', LoggerType.BACKEND);

class CacheObject {
    public id: string;
    public size: number;
    public url: string;
    public path: string;

    constructor() {
        this.id = uuidv4();
        this.size = 0;
        this.path = `/cache/${this.id}`;
        this.url = `app://${this.path}`;
    }
}

export class MediaCache {
    private static instance: MediaCache = null;
    private cache = new Map<number, CacheObject>();
    private cacheUrlMap = new Map<string,number>();
    private playlist: PlaylistContent;
    private quota: number;
    private cacheSize: number = 0;
    private cacheWindowStart: number = 0;
    private cacheWindowEnd: number = 0;

    constructor(playlist: PlaylistContent) {
        MediaCache.instance = this;
        this.playlist = playlist;

        if (!fs.existsSync('/cache')) {
            fs.mkdirSync('/cache');
        }

        // @ts-ignore
        if (TARGET === 'electron') {
            this.quota = Math.min(Math.floor(os.freemem() / 4), 4 * 1024 * 1024 * 1024); // 4GB

        // @ts-ignore
        } else if (TARGET === 'webOS' || TARGET === 'tizenOS') {
            this.quota = Math.min(Math.floor(os.freemem() / 4), 250 * 1024 * 1024); // 250MB
        }
        else {
            this.quota = Math.min(Math.floor(os.freemem() / 4), 250 * 1024 * 1024); // 250MB
        }

        logger.info('Created cache with storage byte quota:', this.quota);
    }

    public destroy() {
        MediaCache.instance = null;
        this.cache.clear();
        this.cache = null;
        this.cacheUrlMap.clear();
        this.cacheUrlMap = null;
        this.playlist = null;
        this.quota = 0;
        this.cacheSize = 0;
        this.cacheWindowStart = 0;
        this.cacheWindowEnd = 0;
    }

    public static getInstance() {
        return MediaCache.instance;
    }

    public has(playlistIndex: number): boolean {
        return this.cache.has(playlistIndex);
    }

    public getUrl(playlistIndex: number): string {
        return this.cache.get(playlistIndex).url;
    }

    public getObject(url: string, start: number = 0, end: number = null): Readable {
        const cacheObject = this.cache.get(this.cacheUrlMap.get(url));
        end = end ? end : cacheObject.size - 1;
        return fs.createReadStream(cacheObject.path, { start: start, end: end });
    }

    public getObjectSize(url: string): number {
        return this.cache.get(this.cacheUrlMap.get(url)).size;
    }

    public cacheForwardItems(cacheIndex: number, cacheAmount: number, playlistIndex: number) {
        if (cacheAmount > 0) {
            for (let i = cacheIndex; i < this.playlist.items.length; i++) {
                const item = this.playlist.items[i];
                if (item.cache) {
                    if (this.cache.has(i)) {
                        this.cacheForwardItems(i + 1, cacheAmount - 1, playlistIndex);
                        break;
                    }
                    const tempCacheObject = new CacheObject();

                    downloadFile(item.url, tempCacheObject.path,
                    (downloadedBytes: number) => {
                        let underQuota = true;
                        if (this.cacheSize + downloadedBytes > this.quota) {
                            underQuota = this.purgeCacheItems(i, downloadedBytes, playlistIndex);
                        }

                        return underQuota;
                    }, null,
                    (downloadedBytes: number) => {
                        this.finalizeCacheItem(tempCacheObject, i, downloadedBytes, playlistIndex);
                        this.cacheForwardItems(i + 1, cacheAmount - 1, playlistIndex);
                    }, true)
                    .catch((error) => {
                        logger.error(error);
                    });
                    break;
                }
            }
        }
    }

    public cacheBackwardItems(cacheIndex: number, cacheAmount: number, playlistIndex: number) {
        if (cacheAmount > 0) {
            for (let i = cacheIndex; i >= 0; i--) {
                const item = this.playlist.items[i];
                if (item.cache) {
                    if (this.cache.has(i)) {
                        this.cacheBackwardItems(i - 1, cacheAmount - 1, playlistIndex);
                        break;
                    }
                    const tempCacheObject = new CacheObject();

                    downloadFile(item.url, tempCacheObject.path,
                    (downloadedBytes: number) => {
                        let underQuota = true;
                        if (this.cacheSize + downloadedBytes > this.quota) {
                            underQuota = this.purgeCacheItems(i, downloadedBytes, playlistIndex);
                        }

                        return underQuota;
                    }, null,
                    (downloadedBytes: number) => {
                        this.finalizeCacheItem(tempCacheObject, i, downloadedBytes, playlistIndex);
                        this.cacheBackwardItems(i - 1, cacheAmount - 1, playlistIndex);
                    }, true)
                    .catch((error) => {
                        logger.error(error);
                    });
                    break;
                }
            }
        }
    }

    private purgeCacheItems(downloadItem: number, downloadedBytes: number, playlistIndex: number): boolean {
        this.updateCacheWindow(playlistIndex);
        let underQuota = true;
        let purgeIndex = playlistIndex;
        let purgeDistance = 0;
        logger.debug(`Downloading item ${downloadItem} with playlist index ${playlistIndex} and cache window: [${this.cacheWindowStart} - ${this.cacheWindowEnd}]`);

        // Priority:
        // 1. Purge first encountered item outside cache window
        // 2. Purge item furthest from view index inside window (except next item from view index)
        for (let index of this.cache.keys()) {
            if (index === downloadItem || index === playlistIndex || index === playlistIndex + 1) {
                continue;
            }

            if (index < this.cacheWindowStart) {
                purgeIndex = index;
                break;
            }
            else if (index > this.cacheWindowEnd) {
                purgeIndex = index;
                break;
            }
            else if (Math.abs(playlistIndex - index) > purgeDistance) {
                purgeDistance = Math.abs(playlistIndex - index);
                purgeIndex = index;
            }
        }

        if (purgeIndex !== playlistIndex) {
            const deleteItem = this.cache.get(purgeIndex);
            this.cacheSize -= deleteItem.size;
            this.cacheUrlMap.delete(deleteItem.url);
            this.cache.delete(purgeIndex);
            this.updateCacheWindow(playlistIndex);
            logger.info(`Item ${downloadItem} pending download (${downloadedBytes} bytes) cannot fit in cache, purging ${purgeIndex} from cache. Remaining quota ${this.quota - this.cacheSize} bytes`);

            if (this.cacheSize + downloadedBytes > this.quota) {
                underQuota = this.purgeCacheItems(downloadItem, downloadedBytes, playlistIndex);
            }
        }
        else {
            // Cannot purge current item since we may already be streaming it
            logger.warn(`Aborting item caching, cannot fit item ${downloadItem} (${downloadedBytes} bytes) within remaining space quota (${this.quota - this.cacheSize} bytes)`);
            underQuota = false;
        }

        return underQuota;
    }

    private finalizeCacheItem(cacheObject: CacheObject, index: number, size: number, playlistIndex: number) {
        cacheObject.size = size;
        this.cacheSize += size;
        logger.info(`Cached item ${index} (${cacheObject.size} bytes) with remaining quota ${this.quota - this.cacheSize} bytes: ${cacheObject.url}`);

        this.cache.set(index, cacheObject);
        this.cacheUrlMap.set(cacheObject.url, index);
        this.updateCacheWindow(playlistIndex);
    }

    private updateCacheWindow(playlistIndex: number) {
        if (this.playlist.forwardCache && this.playlist.forwardCache > 0) {
            let forwardCacheItems = this.playlist.forwardCache;
            for (let index of this.cache.keys()) {
                if (index > playlistIndex) {
                    forwardCacheItems--;

                    if (forwardCacheItems === 0) {
                        this.cacheWindowEnd = index;
                        break;
                    }
                }
            }
        }
        else {
            this.cacheWindowEnd = playlistIndex;
        }

        if (this.playlist.backwardCache && this.playlist.backwardCache > 0) {
            let backwardCacheItems = this.playlist.backwardCache;
            for (let index of this.cache.keys()) {
                if (index < playlistIndex) {
                    backwardCacheItems--;

                    if (backwardCacheItems === 0) {
                        this.cacheWindowStart = index;
                        break;
                    }
                }
            }
        }
        else {
            this.cacheWindowStart = playlistIndex
        }
    }
}
