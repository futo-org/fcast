package com.futo.fcast.receiver

import android.app.ActivityManager
import android.util.Log
import com.futo.fcast.receiver.models.PlaylistContent
import java.util.UUID
import kotlin.math.abs
import kotlin.math.floor
import kotlin.math.min

//import { downloadFile } from 'common/UtilityBackend'
//import { fs } from 'modules/memfs'
//import { Readable } from 'stream'

data class CacheObject(
    val id: UUID = UUID.randomUUID(),
    var size: Long = 0,
    val path: String = "/cache/${id}",
    val url: String = "app://${path}",
)

class MediaCache(private var _playlist: PlaylistContent) {
    private var _cache: MutableMap<Int, CacheObject>
    private var _cacheUrlMap: MutableMap<String, Int>
    private var _playlistIndex: Int
    private var _quota: Long
    private var _cacheSize: Long
    private var _cacheWindowStart: Int
    private var _cacheWindowEnd: Int
    private var _pendingDownloads: MutableSet<Int>
    private var _isDownloading: Boolean
    private var _destroyed: Boolean

    init {
        _instance = this
        _playlistIndex = _playlist.offset ?: 0
        _cache = mutableMapOf()
        _cacheUrlMap = mutableMapOf()
        _cacheSize = 0
        _cacheWindowStart = 0
        _cacheWindowEnd = 0
        _pendingDownloads = mutableSetOf()
        _isDownloading = false
        _destroyed = false

//        if (!fs.existsSync('/cache')) {
//            fs.mkdirSync('/cache')
//        }

        val info = ActivityManager.MemoryInfo()
        _quota = min(floor(info.availMem.toDouble() / 4).toLong(), 4 * 1024 * 1024 * 1024) // 4GB
        _quota = min(floor(info.availMem.toDouble() / 4).toLong(), 250 * 1024 * 1024) // 250MB
        Log.i(TAG, "Created cache with storage byte quota: $_quota")
    }

    fun destroy() {
//        _cache.forEach((item) => { fs.unlinkSync(item.path) })

        _instance = null
        _cache.clear()
        _cacheUrlMap.clear()
        _quota = 0
        _cacheSize = 0
        _cacheWindowStart = 0
        _cacheWindowEnd = 0
        _pendingDownloads.clear()
        _isDownloading = false
        _destroyed = true
    }

    fun has(playlistIndex: Int): Boolean {
        return _cache.containsKey(playlistIndex)
    }

    fun getUrl(playlistIndex: Int): String? {
        return _cache[playlistIndex]?.url
    }

//    fun getObject(url: String, start: Long = 0, end: Long? = null): Readable {
//        val cacheObject = _cache[_cacheUrlMap[url]]
//        end = end ? end : cacheObject.size - 1
//        return fs.createReadStream(cacheObject.path, { start: start, end: end })
//    }

    fun getObjectSize(url: String): Long {
        return _cache[_cacheUrlMap[url]]?.size ?: 0
    }

    fun cacheItems(playlistIndex: Int) {
        _playlistIndex = playlistIndex

        _playlist.forwardCache?.let {
            if (it > 0) {
                var cacheAmount = it

                for (i in playlistIndex + 1..<_playlist.items.size) {
                    if (cacheAmount == 0) {
                        break
                    }

                    _playlist.items[i].cache?.let {
                        cacheAmount--

                        if (!_cache.containsKey(i)) {
                            _pendingDownloads.add(i)
                        }
                    }
                }
            }
        }

        _playlist.backwardCache?.let {
            if (it > 0) {
                var cacheAmount = it

                for (i in playlistIndex - 1 downTo 0) {
                    if (cacheAmount == 0) {
                        break
                    }

                    _playlist.items[i].cache?.let {
                        cacheAmount--

                        if (!_cache.containsKey(i)) {
                            _pendingDownloads.add(i)
                        }
                    }
                }
            }
        }

        updateCacheWindow()

        if (!_isDownloading) {
            _isDownloading = true
            downloadItems()
        }
    }

    private fun downloadItems() {
        if (_pendingDownloads.isNotEmpty()) {
            var itemIndex = 0
            var minDistance = _playlist.items.size
            for (i in _pendingDownloads) {
                if (abs(_playlistIndex - i) < minDistance) {
                    minDistance = abs(_playlistIndex - i)
                    itemIndex = i
                } else if (abs(_playlistIndex - i) == minDistance && i > _playlistIndex) {
                    itemIndex = i
                }
            }
            _pendingDownloads.remove(itemIndex)

            // Due to downloads being async, pending downloads can become out-of-sync with the current playlist index/target cache window
            if (!shouldDownloadItem(itemIndex)) {
                Log.d(
                    TAG,
                    "Discarding download index $itemIndex since its outside cache window [${_cacheWindowStart} - ${_cacheWindowEnd}]"
                )
                downloadItems()
                return
            }

            val tempCacheObject = CacheObject()
//            downloadFile(_playlist.items[itemIndex].url, tempCacheObject.path, true, _playlist.items[itemIndex].headers,
//                (downloadedBytes: Long) => {
//                // Case occurs when user changes playlist while items are still downloading in the old media cache instance
//                if (_destroyed) {
//                    Log.w(TAG, "MediaCache instance destroyed, aborting download")
//                    return false
//                }
//
//                var underQuota = true
//                if (_cacheSize + downloadedBytes > _quota) {
//                    underQuota = purgeCacheItems(itemIndex, downloadedBytes)
//                }
//
//                return underQuota
//            }, null)
//            .then(() => {
//                if (_destroyed) {
//                    fs.unlinkSync(tempCacheObject.path)
//                    return
//                }
//
//                finalizeCacheItem(tempCacheObject, itemIndex)
//                downloadItems()
//            }, (error) => {
//                Log.w(TAG, error)
//
//                if (!_destroyed) {
//                    downloadItems()
//                }
//            })
        } else {
            _isDownloading = false
        }
    }

    private fun shouldDownloadItem(index: Int): Boolean {
        var download = false

        if (index > _playlistIndex) {
            _playlist.forwardCache?.let {
                if (it > 0) {
                    val indexList = _cache.keys.sorted()
                    var forwardCacheItems = it

                    for (i in indexList) {
                        if (i > _playlistIndex) {
                            forwardCacheItems--

                            if (i == index) {
                                download = true
                            } else if (forwardCacheItems == 0) {
                                break
                            }
                        }
                    }
                }
            }
        } else if (index < _playlistIndex) {
            _playlist.backwardCache?.let {
                if (it > 0) {
                    val indexList = _cache.keys.sortedDescending()
                    var backwardCacheItems = it

                    for (i in indexList) {
                        if (i < _playlistIndex) {
                            backwardCacheItems--

                            if (i == index) {
                                download = true
                            } else if (backwardCacheItems == 0) {
                                break
                            }
                        }
                    }
                }
            }
        }

        return download
    }

    private fun purgeCacheItems(downloadItem: Int, downloadedBytes: Long): Boolean {
        var underQuota = true

        while (_cacheSize + downloadedBytes > _quota) {
            var purgeIndex = _playlistIndex
            var purgeDistance = 0
            Log.d(
                TAG,
                "Downloading item $downloadItem with playlist index $_playlistIndex and cache window: [${_cacheWindowStart} - ${_cacheWindowEnd}]"
            )

            // Priority:
            // 1. Purge first encountered item outside cache window
            // 2. Purge item furthest from view index inside window (except next item from view index)
            for (index in _cache.keys) {
                if (index == downloadItem || index == _playlistIndex || index == _playlistIndex + 1) {
                    continue
                }

                if (index < _cacheWindowStart || index > _cacheWindowEnd) {
                    purgeIndex = index
                    break
                } else if (abs(_playlistIndex - index) > purgeDistance) {
                    purgeDistance = abs(_playlistIndex - index)
                    purgeIndex = index
                }
            }

            if (purgeIndex != _playlistIndex) {
                val deleteItem = _cache.get(purgeIndex)
//                fs.unlinkSync(deleteItem.path)
//                _cacheSize -= deleteItem.size
//                _cacheUrlMap.delete(deleteItem.url)
//                _cache.delete(purgeIndex)
                this.updateCacheWindow()
                Log.i(
                    TAG,
                    "Item $downloadItem pending download (${downloadedBytes} bytes) cannot fit in cache, purging $purgeIndex from cache. Remaining quota ${_quota - _cacheSize} bytes"
                )
            } else {
                // Cannot purge current item since we may already be streaming it
                Log.w(
                    TAG,
                    "Aborting item caching, cannot fit item $downloadItem (${downloadedBytes} bytes) within remaining space quota (${_quota - _cacheSize} bytes)"
                )
                underQuota = false
                break
            }
        }

        return underQuota
    }

    private fun finalizeCacheItem(cacheObject: CacheObject, index: Int) {
//        val size = fs.statSync(cacheObject.path).size
//        cacheObject.size = size
//        _cacheSize += size
        Log.i(
            TAG,
            "Cached item $index (${cacheObject.size} bytes) with remaining quota ${_quota - _cacheSize} bytes: ${cacheObject.url}"
        )

        _cache[index] = cacheObject
        _cacheUrlMap[cacheObject.url] = index
        this.updateCacheWindow()
    }

    private fun updateCacheWindow() {
        val indexList = _cache.keys.sorted()

        _playlist.forwardCache?.also {
            if (it > 0) {
                var forwardCacheItems = it
                for (index in indexList) {
                    if (index > _playlistIndex) {
                        forwardCacheItems--

                        if (forwardCacheItems == 0) {
                            _cacheWindowEnd = index
                            break
                        }
                    }
                }
            }
        } ?: run {
            _cacheWindowEnd = _playlistIndex
        }

        _playlist.backwardCache?.also {
            if (it > 0) {
                var backwardCacheItems = it
                for (index in indexList) {
                    if (index < _playlistIndex) {
                        backwardCacheItems--

                        if (backwardCacheItems == 0) {
                            _cacheWindowStart = index
                            break
                        }
                    }
                }
            }
        } ?: run {
            _cacheWindowStart = _playlistIndex
        }
    }

    companion object {
        const val TAG = "MediaCache"
        private var _instance: MediaCache? = null

        fun getInstance(): MediaCache? {
            return _instance
        }
    }
}
