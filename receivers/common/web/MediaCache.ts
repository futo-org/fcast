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
    private cache: Map<number, CacheObject>;
    private cacheUrlMap: Map<string,number>;
    private playlist: PlaylistContent;
    private playlistIndex: number;
    private quota: number;
    private cacheSize: number;
    private cacheWindowStart: number;
    private cacheWindowEnd: number;
    private pendingDownloads: Set<number>;
    private isDownloading: boolean;
    private destroyed: boolean;

    constructor(playlist: PlaylistContent) {
        MediaCache.instance = this;
        this.playlist = playlist;
        this.playlistIndex = playlist.offset ? playlist.offset : 0;
        this.cache = new Map<number, CacheObject>();
        this.cacheUrlMap = new Map<string,number>();
        this.cacheSize = 0;
        this.cacheWindowStart = 0;
        this.cacheWindowEnd = 0;
        this.pendingDownloads = new Set();
        this.isDownloading = false;
        this.destroyed = false;

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
        this.cache.forEach((item) => { fs.unlinkSync(item.path); });

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
        this.pendingDownloads.clear();
        this.isDownloading = false;
        this.destroyed = true;
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

    public cacheItems(playlistIndex: number) {
        this.playlistIndex = playlistIndex;

        if (this.playlist.forwardCache && this.playlist.forwardCache > 0) {
            let cacheAmount = this.playlist.forwardCache;

            for (let i = playlistIndex + 1; i < this.playlist.items.length; i++) {
                if (cacheAmount === 0) {
                    break;
                }

                if (this.playlist.items[i].cache) {
                    cacheAmount--;

                    if (!this.cache.has(i)) {
                        this.pendingDownloads.add(i);
                    }
                }
            }
        }

        if (this.playlist.backwardCache && this.playlist.backwardCache > 0) {
            let cacheAmount = this.playlist.backwardCache;

            for (let i = playlistIndex - 1; i >= 0; i--) {
                if (cacheAmount === 0) {
                    break;
                }

                if (this.playlist.items[i].cache) {
                    cacheAmount--;

                    if (!this.cache.has(i)) {
                        this.pendingDownloads.add(i);
                    }
                }
            }
        }

        this.updateCacheWindow();

        if (!this.isDownloading) {
            this.isDownloading = true;
            this.downloadItems();
        }
    }

    private downloadItems() {
        if (this.pendingDownloads.size > 0) {
            let itemIndex = 0;
            let minDistance = this.playlist.items.length;
            for (let i of this.pendingDownloads.values()) {
                if (Math.abs(this.playlistIndex - i) < minDistance) {
                    minDistance = Math.abs(this.playlistIndex - i);
                    itemIndex = i;
                }
                else if (Math.abs(this.playlistIndex - i) === minDistance && i > this.playlistIndex) {
                    itemIndex = i;
                }
            }
            this.pendingDownloads.delete(itemIndex);

            // Due to downloads being async, pending downloads can become out-of-sync with the current playlist index/target cache window
            if (!this.shouldDownloadItem(itemIndex)) {
                logger.debug(`Discarding download index ${itemIndex} since its outside cache window [${this.cacheWindowStart} - ${this.cacheWindowEnd}]`);
                this.downloadItems();
                return;
            }

            const tempCacheObject = new CacheObject();
            downloadFile(this.playlist.items[itemIndex].url, tempCacheObject.path, true, this.playlist.items[itemIndex].headers,
            (downloadedBytes: number) => {
                // Case occurs when user changes playlist while items are still downloading in the old media cache instance
                if (this.destroyed) {
                    logger.warn('MediaCache instance destroyed, aborting download');
                    return false;
                }

                let underQuota = true;
                if (this.cacheSize + downloadedBytes > this.quota) {
                    underQuota = this.purgeCacheItems(itemIndex, downloadedBytes);
                }

                return underQuota;
            }, null)
            .then(() => {
                if (this.destroyed) {
                    fs.unlinkSync(tempCacheObject.path);
                    return;
                }

                this.finalizeCacheItem(tempCacheObject, itemIndex);
                this.downloadItems();
            }, (error) => {
                logger.warn(error);

                if (!this.destroyed) {
                    this.downloadItems();
                }
            });
        }
        else {
            this.isDownloading = false;
        }
    }

    private shouldDownloadItem(index: number): boolean {
        let download = false;

        if (index > this.playlistIndex) {
            if (this.playlist.forwardCache && this.playlist.forwardCache > 0) {
                const indexList = [...this.cache.keys(), index].sort((a, b) => a - b);
                let forwardCacheItems = this.playlist.forwardCache;

                for (let i of indexList) {
                    if (i > this.playlistIndex) {
                        forwardCacheItems--;

                        if (i === index) {
                            download = true;
                        }
                        else if (forwardCacheItems === 0) {
                            break;
                        }
                    }
                }
            }
        }
        else if (index < this.playlistIndex) {
            if (this.playlist.backwardCache && this.playlist.backwardCache > 0) {
                const indexList = [...this.cache.keys(), index].sort((a, b) => b - a);
                let backwardCacheItems = this.playlist.backwardCache;

                for (let i of indexList) {
                    if (i < this.playlistIndex) {
                        backwardCacheItems--;

                        if (i === index) {
                            download = true;
                        }
                        else if (backwardCacheItems === 0) {
                            break;
                        }
                    }
                }
            }
        }

        return download;
    }

    private purgeCacheItems(downloadItem: number, downloadedBytes: number): boolean {
        let underQuota = true;

        while (this.cacheSize + downloadedBytes > this.quota) {
            let purgeIndex = this.playlistIndex;
            let purgeDistance = 0;
            logger.debug(`Downloading item ${downloadItem} with playlist index ${this.playlistIndex} and cache window: [${this.cacheWindowStart} - ${this.cacheWindowEnd}]`);

            // Priority:
            // 1. Purge first encountered item outside cache window
            // 2. Purge item furthest from view index inside window (except next item from view index)
            for (let index of this.cache.keys()) {
                if (index === downloadItem || index === this.playlistIndex || index === this.playlistIndex + 1) {
                    continue;
                }

                if (index < this.cacheWindowStart || index > this.cacheWindowEnd) {
                    purgeIndex = index;
                    break;
                }
                else if (Math.abs(this.playlistIndex - index) > purgeDistance) {
                    purgeDistance = Math.abs(this.playlistIndex - index);
                    purgeIndex = index;
                }
            }

            if (purgeIndex !== this.playlistIndex) {
                const deleteItem = this.cache.get(purgeIndex);
                fs.unlinkSync(deleteItem.path);
                this.cacheSize -= deleteItem.size;
                this.cacheUrlMap.delete(deleteItem.url);
                this.cache.delete(purgeIndex);
                this.updateCacheWindow();
                logger.info(`Item ${downloadItem} pending download (${downloadedBytes} bytes) cannot fit in cache, purging ${purgeIndex} from cache. Remaining quota ${this.quota - this.cacheSize} bytes`);
            }
            else {
                // Cannot purge current item since we may already be streaming it
                logger.warn(`Aborting item caching, cannot fit item ${downloadItem} (${downloadedBytes} bytes) within remaining space quota (${this.quota - this.cacheSize} bytes)`);
                underQuota = false;
                break;
            }
        }

        return underQuota;
    }

    private finalizeCacheItem(cacheObject: CacheObject, index: number) {
        const size = fs.statSync(cacheObject.path).size;
        cacheObject.size = size;
        this.cacheSize += size;
        logger.info(`Cached item ${index} (${cacheObject.size} bytes) with remaining quota ${this.quota - this.cacheSize} bytes: ${cacheObject.url}`);

        this.cache.set(index, cacheObject);
        this.cacheUrlMap.set(cacheObject.url, index);
        this.updateCacheWindow();
    }

    private updateCacheWindow() {
        const indexList = [...this.cache.keys()].sort((a, b) => a - b);

        if (this.playlist.forwardCache && this.playlist.forwardCache > 0) {
            let forwardCacheItems = this.playlist.forwardCache;
            for (let index of indexList) {
                if (index > this.playlistIndex) {
                    forwardCacheItems--;

                    if (forwardCacheItems === 0) {
                        this.cacheWindowEnd = index;
                        break;
                    }
                }
            }
        }
        else {
            this.cacheWindowEnd = this.playlistIndex;
        }

        if (this.playlist.backwardCache && this.playlist.backwardCache > 0) {
            let backwardCacheItems = this.playlist.backwardCache;
            for (let index of indexList) {
                if (index < this.playlistIndex) {
                    backwardCacheItems--;

                    if (backwardCacheItems === 0) {
                        this.cacheWindowStart = index;
                        break;
                    }
                }
            }
        }
        else {
            this.cacheWindowStart = this.playlistIndex
        }
    }
}
